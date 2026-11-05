// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Output pin of the virtual camera: format negotiation, allocator setup
//! and the streaming thread that pumps frames from the shared slot
//! (`vcam-ipc`) into the DirectShow graph.

// DShow API sizes are i32/usize mixed; geometry is bounded (<=1920x1080).
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{
    ALLOCATOR_PROPERTIES, IEnumMediaTypes, IEnumMediaTypes_Impl, IMemAllocator, IMemInputPin,
    IMediaSample, IPin, IPin_Impl, PIN_DIRECTION, PIN_INFO, PINDIR_OUTPUT, VFW_E_ALREADY_CONNECTED,
    VFW_E_NOT_CONNECTED,
};
use windows::Win32::Media::DirectShow::{IAMStreamConfig, IAMStreamConfig_Impl};
use windows::Win32::Media::KernelStreaming::{IKsPropertySet, IKsPropertySet_Impl};
use windows::Win32::Media::MediaFoundation::{AM_MEDIA_TYPE, AMPROPSETID_Pin, CLSID_MemoryAllocator};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::*;

use vcam_ipc::CamSharedReader;

use crate::media::{self, CamFormat};
use crate::pixel;
use crate::state::Shared;

/// Default geometry when no producer is attached yet.
pub const DEFAULT_WIDTH: u32 = 1280;
/// Default height.
pub const DEFAULT_HEIGHT: u32 = 720;
/// Default frame rate.
pub const DEFAULT_FPS: u32 = 30;

/// The single output pin.
#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
pub struct UdsCamPin {
    pub(crate) shared: Arc<Shared>,
}

/// Pin-side mutable state, guarded by the shared mutex.
pub struct PinState {
    /// Negotiated format / geometry.
    pub format: CamFormat,
    /// Negotiated width.
    pub width: u32,
    /// Negotiated height.
    pub height: u32,
    /// Negotiated fps.
    pub fps: u32,
    /// Downstream input pin while connected.
    pub connected: Option<IPin>,
    /// Downstream memory input while connected.
    pub mem_input: Option<IMemInputPin>,
    /// Allocator (stock memory allocator or downstream-provided).
    pub allocator: Option<IMemAllocator>,
    /// Streaming thread while active.
    pub stream: Option<JoinHandle<()>>,
    /// Signals the streaming thread to exit.
    pub quit: Arc<AtomicBool>,
    /// The pin's own `IPin`, set once after construction (broken on Drop).
    pub this_pin: Option<IPin>,
}

impl PinState {
    /// Placeholder before the pin object exists (fields unused until
    /// [`UdsCamPin::new`] replaces the state).
    pub fn placeholder() -> Self {
        Self {
            format: CamFormat::Nv12,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fps: DEFAULT_FPS,
            connected: None,
            mem_input: None,
            allocator: None,
            stream: None,
            quit: Arc::new(AtomicBool::new(false)),
            this_pin: None,
        }
    }

    /// Fresh state: dimensions come from the shared slot if a producer is
    /// already publishing, else the defaults.
    fn new() -> Self {
        let mut s = Self::placeholder();
        if let Some((w, h)) = CamSharedReader::open().ok().and_then(|r| r.current_dims()) {
            s.width = w;
            s.height = h;
        }
        s
    }
}

impl UdsCamPin {
    /// Creates the pin on the shared filter state.
    pub fn new(shared: Arc<Shared>) -> Self {
        // The pin is a standalone COM object: keep the DLL loaded while
        // any consumer holds it (see UdsCamFilter::create).
        crate::globals::dll_add_ref();
        *shared.pin.lock().unwrap() = PinState::new();
        Self { shared }
    }

    /// Starts the streaming thread (called by the filter on Pause/Run).
    pub fn start_streaming(shared: &Shared) {
        let mut pin = shared.pin.lock().unwrap();
        if pin.connected.is_none() || pin.stream.is_some() {
            return;
        }
        let (Some(mem_input), Some(allocator)) = (pin.mem_input.clone(), pin.allocator.clone()) else {
            return;
        };
        let (format, width, height, fps) = (pin.format, pin.width, pin.height, pin.fps);
        pin.quit.store(false, Ordering::Release);
        let quit = pin.quit.clone();
        // COM interface wrappers are !Send (they hold NonNull); move the raw
        // pointers across and reconstruct borrowed views in the thread. The
        // originals stay alive in PinState until after `join`.
        let mem_input_raw = mem_input.as_raw() as usize;
        let allocator_raw = allocator.as_raw() as usize;
        pin.stream = Some(std::thread::spawn(move || {
            stream_loop(
                None,
                mem_input_raw,
                allocator_raw,
                format,
                width,
                height,
                fps,
                quit,
            );
        }));
    }

    /// Stops the streaming thread (called by the filter on Stop).
    pub fn stop_streaming(shared: &Shared) {
        let mut pin = shared.pin.lock().unwrap();
        pin.quit.store(true, Ordering::Release);
        // Unblock GetBuffer inside the thread.
        if let Some(alloc) = &pin.allocator {
            unsafe {
                let _ = alloc.Decommit();
            }
        }
        if let Some(t) = pin.stream.take() {
            drop(pin);
            let _ = t.join();
        }
    }
}

impl Drop for UdsCamPin {
    fn drop(&mut self) {
        // Break the construction-time reference cycle (see filter.rs).
        self.shared.pin.lock().unwrap().this_pin = None;
        crate::globals::dll_release();
    }
}

/// Streaming loop: one iteration per frame interval; publishes the latest
/// shared frame or black when no producer is attached.
///
/// `mem_input_raw`/`allocator_raw` are raw COM pointers borrowed from
/// `PinState` (kept alive until this thread is joined).
#[allow(clippy::too_many_arguments)]
fn stream_loop(
    mut reader: Option<CamSharedReader>,
    mem_input_raw: usize,
    allocator_raw: usize,
    format: CamFormat,
    width: u32,
    height: u32,
    fps: u32,
    quit: Arc<AtomicBool>,
) {
    // Borrowed interface views: ManuallyDrop, the owners live in PinState.
    let mem_input = unsafe {
        std::mem::ManuallyDrop::new(IMemInputPin::from_raw(
            std::ptr::NonNull::new(mem_input_raw as *mut core::ffi::c_void).unwrap().as_ptr(),
        ))
    };
    let allocator = unsafe {
        std::mem::ManuallyDrop::new(IMemAllocator::from_raw(
            std::ptr::NonNull::new(allocator_raw as *mut core::ffi::c_void).unwrap().as_ptr(),
        ))
    };
    let frame_ms = (1000 / fps.max(1)).max(1);
    let frame_dur: i64 = 10_000_000 / i64::from(fps.max(1));
    let frame_bytes = format.frame_bytes(width, height);
    let mut bgra: Vec<u8> = Vec::new();
    let mut index: i64 = 0;
    unsafe {
        loop {
            if quit.load(Ordering::Acquire) || crate::globals::is_detached() {
                break;
            }
            if let Some(r) = &reader {
                let _ = WaitForSingleObject(r.event_handle(), frame_ms);
            } else {
                std::thread::sleep(Duration::from_millis(frame_ms.into()));
            }
            // Re-attach to the slot if it appears late.
            if reader.is_none() {
                reader = CamSharedReader::open().ok();
            }

            let mut sample: Option<IMediaSample> = None;
            if allocator
                .GetBuffer(&mut sample, None, None, 0)
                .is_err()
            {
                // Decommitted (Stop) or torn down.
                break;
            }
            let Some(sample) = sample else { continue };
            let Ok(buf) = sample.GetPointer() else { continue };
            if index == 0 {
                crate::vlog!("stream: first sample");
            }
            if (sample.GetSize() as usize) < frame_bytes {
                continue;
            }
            let out = std::slice::from_raw_parts_mut(buf, frame_bytes);
            match reader.as_mut().and_then(|r| r.read_frame(&mut bgra)) {
                Some((sw, sh, _ts)) => {
                    // Scale when the consumer negotiated a different geometry.
                    if sw != width || sh != height {
                        bgra = pixel::scale_bgra_nearest(&bgra, sw, sh, width, height);
                    }
                    match format {
                        CamFormat::Nv12 => pixel::bgra_to_nv12(&bgra, width, height, out),
                        CamFormat::Rgb24 => pixel::bgra_to_rgb24_bottom_up(&bgra, width, height, out),
                    }
                }
                None => match format {
                    CamFormat::Nv12 => pixel::nv12_black(width, height, out),
                    CamFormat::Rgb24 => pixel::rgb24_black(width, height, out),
                },
            }
            let _ = sample.SetActualDataLength(frame_bytes as i32);
            let start = index * frame_dur;
            let _ = sample.SetTime(Some(&start), Some(&(start + frame_dur)));
            let _ = sample.SetSyncPoint(true);
            if let Err(e) = mem_input.Receive(&sample) {
                crate::vlog!("stream: Receive failed: {e:?}");
                break;
            }
            index += 1;
        }
    }
}

/// Validates an incoming media type against the offered format list;
/// returns the parsed (format, width, height) when acceptable.
///
/// # Safety
/// `mt` follows the DirectShow media type contract.
unsafe fn acceptable(mt: *const AM_MEDIA_TYPE) -> Option<(CamFormat, u32, u32)> {
    let (format, w, h, _fps) = unsafe { media::parse_media_type(mt) }?;
    media::valid_geometry(w, h).then_some((format, w, h))
}

#[allow(non_snake_case)]
impl IPin_Impl for UdsCamPin_Impl {
    fn Connect(&self, preceivepin: Ref<IPin>, pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        crate::vlog!("pin Connect pmt_null={}", pmt.is_null());
        let receive = preceivepin.ok()?;
        let mut pin = self.shared.pin.lock().unwrap();
        if pin.connected.is_some() {
            crate::vlog!("pin Connect: already connected");
            return Err(VFW_E_ALREADY_CONNECTED.into());
        }
        // Pick the current selection, or honor the requested media type
        // (format AND geometry — browsers SetFormat before connecting).
        let (format, width, height) = if pmt.is_null() {
            (pin.format, pin.width, pin.height)
        } else {
            crate::vlog!("pin Connect: validating requested media type");
            match unsafe { acceptable(pmt) } {
                Some(f) => f,
                None => return Err(E_INVALIDARG.into()),
            }
        };
        crate::vlog!("pin Connect: chosen {format:?} {width}x{height}");
        // Build the concrete media type and hand the connection to the
        // downstream input pin (baseclasses contract).
        let mt = unsafe { media::make_media_type(format, width, height, pin.fps) }
            .ok_or(E_UNEXPECTED)?;
        let this = pin.this_pin.clone().ok_or(E_UNEXPECTED)?;
        crate::vlog!("pin Connect: ReceiveConnection");
        let result = unsafe { receive.ReceiveConnection(&this, mt) };
        unsafe { media::free_media_type(mt) };
        crate::vlog!("pin Connect: ReceiveConnection -> {result:?}");
        result?;

        crate::vlog!("pin Connect: cast IMemInputPin");
        let mem_input: IMemInputPin = receive.cast()?;
        // Prefer the downstream allocator; fall back to the stock one.
        crate::vlog!("pin Connect: GetAllocator");
        let allocator = unsafe { mem_input.GetAllocator() }.or_else(|_| unsafe {
            CoCreateInstance(&CLSID_MemoryAllocator, None, CLSCTX_INPROC_SERVER)
        })?;
        let props = ALLOCATOR_PROPERTIES {
            cBuffers: 4,
            cbBuffer: format.frame_bytes(width, height) as i32,
            cbAlign: 1,
            cbPrefix: 0,
        };
        crate::vlog!("pin Connect: SetProperties/NotifyAllocator/Commit");
        unsafe {
            allocator.SetProperties(&props)?;
            mem_input.NotifyAllocator(&allocator, false)?;
            allocator.Commit()?;
        }
        pin.format = format;
        pin.width = width;
        pin.height = height;
        pin.connected = Some(receive.clone());
        pin.mem_input = Some(mem_input);
        pin.allocator = Some(allocator);
        crate::vlog!("pin connected fmt={format:?} {width}x{height}");
        Ok(())
    }

    fn ReceiveConnection(&self, _pconnector: Ref<IPin>, _pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        // Output pins never receive connections.
        Err(E_UNEXPECTED.into())
    }

    fn Disconnect(&self) -> Result<()> {
        UdsCamPin::stop_streaming(&self.shared);
        let mut pin = self.shared.pin.lock().unwrap();
        pin.connected = None;
        pin.mem_input = None;
        pin.allocator = None;
        Ok(())
    }

    fn ConnectedTo(&self) -> Result<IPin> {
        self.shared.pin.lock().unwrap().connected.clone().ok_or_else(|| VFW_E_NOT_CONNECTED.into())
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> Result<()> {
        crate::vlog!("pin ConnectionMediaType connected={}", self.shared.pin.lock().unwrap().connected.is_some());
        let pin = self.shared.pin.lock().unwrap();
        if pin.connected.is_none() {
            return Err(VFW_E_NOT_CONNECTED.into());
        }
        if pmt.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            let src = media::make_media_type(pin.format, pin.width, pin.height, pin.fps)
                .ok_or(E_UNEXPECTED)?;
            // Copy (not move): the destination owns the copy.
            let src_ref = &*src;
            std::ptr::copy_nonoverlapping(src, pmt, 1);
            let vih = CoTaskMemAlloc(src_ref.cbFormat as usize);
            if vih.is_null() {
                media::free_media_type(pmt);
                return Err(E_UNEXPECTED.into());
            }
            std::ptr::copy_nonoverlapping(src_ref.pbFormat, vih.cast::<u8>(), src_ref.cbFormat as usize);
            (*pmt).pbFormat = vih.cast();
            media::free_media_type(src);
        }
        Ok(())
    }

    fn QueryPinInfo(&self, pinfo: *mut PIN_INFO) -> Result<()> {
        crate::vlog!("pin QueryPinInfo");
        if pinfo.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            let info = &mut *pinfo;
            info.dir = PINDIR_OUTPUT;
            let name: Vec<u16> = "Output\0".encode_utf16().collect();
            info.achName[..name.len()].copy_from_slice(&name);
            info.pFilter = std::mem::ManuallyDrop::new(self.shared.filter_this.lock().unwrap().clone());
        }
        Ok(())
    }

    fn QueryDirection(&self) -> Result<PIN_DIRECTION> {
        crate::vlog!("pin QueryDirection");
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> Result<PWSTR> {
        crate::vlog!("pin QueryId");
        let name: Vec<u16> = "UDS Cam Out\0".encode_utf16().collect();
        unsafe {
            let buf = CoTaskMemAlloc(name.len() * 2).cast::<u16>();
            if buf.is_null() {
                return Err(E_UNEXPECTED.into());
            }
            std::ptr::copy_nonoverlapping(name.as_ptr(), buf, name.len());
            Ok(PWSTR(buf))
        }
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        match unsafe { acceptable(pmt) } {
            Some(_) => S_OK,
            None => S_FALSE,
        }
    }

    fn EnumMediaTypes(&self) -> Result<IEnumMediaTypes> {
        let pin = self.shared.pin.lock().unwrap();
        Ok(UdsEnumMediaTypes::new(pin.fps).into())
    }

    fn QueryInternalConnections(&self, _appin: OutRef<IPin>, npin: *mut u32) -> Result<()> {
        if npin.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *npin = 0 };
        Ok(())
    }

    fn EndOfStream(&self) -> Result<()> {
        Ok(())
    }

    fn BeginFlush(&self) -> Result<()> {
        Ok(())
    }

    fn EndFlush(&self) -> Result<()> {
        Ok(())
    }

    fn NewSegment(&self, _tstart: i64, _tstop: i64, _drate: f64) -> Result<()> {
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IAMStreamConfig_Impl for UdsCamPin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> Result<()> {
        crate::vlog!("pin SetFormat: {:?}", unsafe { crate::media::parse_media_type(pmt) });
        let mut pin = self.shared.pin.lock().unwrap();
        if pin.connected.is_some() {
            return Err(VFW_E_ALREADY_CONNECTED.into());
        }
        let (format, width, height) = unsafe { acceptable(pmt) }.ok_or(E_INVALIDARG)?;
        pin.format = format;
        pin.width = width;
        pin.height = height;
        Ok(())
    }

    fn GetFormat(&self) -> Result<*mut AM_MEDIA_TYPE> {
        let pin = self.shared.pin.lock().unwrap();
        unsafe { media::make_media_type(pin.format, pin.width, pin.height, pin.fps) }
            .ok_or_else(|| E_UNEXPECTED.into())
    }

    fn GetNumberOfCapabilities(&self, picount: *mut i32, pisize: *mut i32) -> Result<()> {
        if picount.is_null() || pisize.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *picount = media::offer_count() as i32;
            *pisize = size_of::<windows::Win32::Media::DirectShow::VIDEO_STREAM_CONFIG_CAPS>() as i32;
        }
        Ok(())
    }

    fn GetStreamCaps(&self, iindex: i32, ppmt: *mut *mut AM_MEDIA_TYPE, pscc: *mut u8) -> Result<()> {
        if ppmt.is_null() || pscc.is_null() {
            return Err(E_POINTER.into());
        }
        let pin = self.shared.pin.lock().unwrap();
        let Some((format, w, h)) = media::offer(usize::try_from(iindex).unwrap_or(usize::MAX)) else {
            return Err(S_FALSE.into());
        };
        unsafe {
            *ppmt = media::make_media_type(format, w, h, pin.fps).ok_or(E_UNEXPECTED)?;
            media::fill_stream_caps(pscc, w, h, pin.fps);
        }
        Ok(())
    }
}

/// Pin category property (`AMPROPSETID_Pin` / `AMPROPERTY_PIN_CATEGORY`):
/// capture-graph builders (and Chromium) refuse pins without it.
#[allow(non_snake_case)]
impl IKsPropertySet_Impl for UdsCamPin_Impl {
    fn Set(
        &self,
        _guidpropset: *const GUID,
        _dwpropid: u32,
        _pinstancedata: *const core::ffi::c_void,
        _cbinstancedata: u32,
        _ppropdata: *const core::ffi::c_void,
        _cbpropdata: u32,
    ) -> Result<()> {
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }

    fn Get(
        &self,
        guidpropset: *const GUID,
        dwpropid: u32,
        _pinstancedata: *const core::ffi::c_void,
        _cbinstancedata: u32,
        ppropdata: *mut core::ffi::c_void,
        cbpropdata: u32,
        pcbreturned: *mut u32,
    ) -> Result<()> {
        crate::vlog!("pin KsPropertySet::Get set={:?} id={dwpropid}", unsafe { guidpropset.as_ref() });
        if guidpropset.is_null()
            || unsafe { *guidpropset } != AMPROPSETID_Pin
            || dwpropid != 0
            || ppropdata.is_null()
            || cbpropdata < size_of::<GUID>() as u32
        {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            *(ppropdata.cast::<GUID>()) = windows::Win32::Media::MediaFoundation::PIN_CATEGORY_CAPTURE;
            if !pcbreturned.is_null() {
                *pcbreturned = size_of::<GUID>() as u32;
            }
        }
        Ok(())
    }

    fn QuerySupported(&self, guidpropset: *const GUID, dwpropid: u32) -> Result<u32> {
        crate::vlog!("pin KsPropertySet::QuerySupported id={dwpropid}");
        if !guidpropset.is_null()
            && unsafe { *guidpropset } == AMPROPSETID_Pin
            && dwpropid == 0
        {
            // KSPROPERTY_SUPPORT_GET
            return Ok(1);
        }
        Err(windows::Win32::Foundation::E_NOTIMPL.into())
    }
}

/// `IEnumMediaTypes` over the offered formats × geometries.
#[implement(IEnumMediaTypes)]
struct UdsEnumMediaTypes {
    fps: u32,
    index: Mutex<usize>,
}

impl UdsEnumMediaTypes {
    fn new(fps: u32) -> Self {
        Self { fps, index: Mutex::new(0) }
    }
}

#[allow(non_snake_case)]
impl IEnumMediaTypes_Impl for UdsEnumMediaTypes_Impl {
    fn Next(
        &self,
        cmediatypes: u32,
        ppmediatypes: *mut *mut AM_MEDIA_TYPE,
        pcfetched: *mut u32,
    ) -> HRESULT {
        if ppmediatypes.is_null() {
            return E_POINTER;
        }
        let mut fetched = 0u32;
        {
            let mut index = self.index.lock().unwrap();
            while fetched < cmediatypes {
                let Some((format, w, h)) = media::offer(*index) else { break };
                let mt = unsafe { media::make_media_type(format, w, h, self.fps) };
                match mt {
                    Some(mt) => {
                        unsafe { *ppmediatypes.add(fetched as usize) = mt };
                        fetched += 1;
                        *index += 1;
                    }
                    None => return E_UNEXPECTED,
                }
            }
        }
        if !pcfetched.is_null() {
            unsafe { *pcfetched = fetched };
        }
        if fetched == cmediatypes { S_OK } else { S_FALSE }
    }

    fn Skip(&self, cmediatypes: u32) -> Result<()> {
        let mut index = self.index.lock().unwrap();
        *index = (*index + cmediatypes as usize).min(media::offer_count());
        if *index == media::offer_count() && cmediatypes > 0 {
            return Err(S_FALSE.into());
        }
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        *self.index.lock().unwrap() = 0;
        Ok(())
    }

    fn Clone(&self) -> Result<IEnumMediaTypes> {
        let cloned = UdsEnumMediaTypes::new(self.fps);
        *cloned.index.lock().unwrap() = *self.index.lock().unwrap();
        Ok(cloned.into())
    }
}

