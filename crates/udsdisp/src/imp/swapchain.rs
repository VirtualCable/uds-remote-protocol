//! Swap-chain processing: when the OS assigns a swap-chain to a monitor, the
//! rendered desktop image flows through it. We run the acquire / process /
//! release loop on a dedicated thread, mirroring IddSampleDriver's
//! `SwapChainProcessor`.
//!
//! M2 scope: process = log frame metadata and dirty rects (throttled). The
//! hand-off of frames to the agent is M6 territory; DXGI duplication on top
//! of the virtual monitor (M3) does not need anything more from the driver.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HMODULE, LUID, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIFactory4,
    IDXGIResource,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows::core::{Interface, PCWSTR};
use wdk_sys::{NTSTATUS, call_unsafe_wdf_function_binding};

use super::{iddcx, iddcx::bindings, log_line, shared};

/// `E_PENDING` (0x8000000A) as a plain i32: `IddCxSwapChainReleaseAndAcquireBuffer`
/// returns it when no new frame is ready yet.
const E_PENDING: i32 = 0x8000_000A_u32 as i32;

/// How often dirty-rect details are logged at most (the loop can run at the
/// full monitor refresh rate; the log is for verification, not telemetry).
const DIRTY_LOG_INTERVAL: Duration = Duration::from_millis(250);

/// Dirty rects are logged for verification, not processed — cap the fetch.
const MAX_LOGGED_RECTS: usize = 8;

/// A running swap-chain processing loop (one per assigned swap-chain).
pub struct Task {
    /// Stop signal for the processing thread (Win32 event handle as `usize`
    /// to keep the state map free of `Send`/`Sync` concerns).
    pub stop_event: usize,
    /// Processing thread; joined on unassign.
    pub thread: Option<std::thread::JoinHandle<()>>,
}

/// `EvtIddCxMonitorAssignSwapChain`: the OS assigned a swap-chain to the
/// monitor. Creates the D3D11 device on the OS-chosen render adapter and
/// spawns the processing thread (IddSampleDriver's `AssignSwapChain`).
///
/// # Safety
/// Registered with IddCx; `input` is valid for the call duration.
pub unsafe extern "C" fn assign(
    monitor: bindings::IDDCX_MONITOR,
    input: *const bindings::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    // SAFETY: guaranteed by IddCx for the duration of the callback.
    let input = unsafe { &*input };
    let luid = input.RenderAdapterLuid;
    log_line(&format!(
        "EvtIddCxMonitorAssignSwapChain monitor={monitor:?} swapchain={:?} RenderAdapterLuid={:#x}:{:#x}",
        input.hSwapChain, luid.HighPart, luid.LowPart
    ));

    // The OS unassigns before reassigning, but stop a previous task
    // defensively so state stays consistent on any call order.
    stop_task(monitor as usize);

    let (device, _context) = match create_render_device(luid) {
        Ok(ok) => ok,
        Err(e) => {
            log_line(&format!(
                "render device creation failed: {e} — deleting swap-chain so the OS retries"
            ));
            // SAFETY: the swap-chain handle comes from the OS in this callback.
            unsafe { delete_wdf_object(input.hSwapChain as usize) };
            return 0;
        }
    };

    // SAFETY: standard event creation; no name, auto-reset, initially unset.
    let stop_event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
        Ok(event) => event,
        Err(e) => {
            log_line(&format!("CreateEventW failed: {e} — deleting swap-chain"));
            // SAFETY: see above.
            unsafe { delete_wdf_object(input.hSwapChain as usize) };
            return 0;
        }
    };

    let swapchain = input.hSwapChain as usize;
    let available = input.hNextSurfaceAvailable as usize;
    let stop = stop_event.0 as usize;
    let thread = std::thread::spawn(move || processing_loop(swapchain, available, stop, device));

    let mut state = shared::lock();
    if let Some(entry) = state.monitors.iter_mut().find(|m| m.handle == monitor as usize) {
        entry.swapchain_task = Some(Task {
            stop_event: stop,
            thread: Some(thread),
        });
    } else {
        log_line(&format!("assign for unknown monitor {monitor:?}; task left running"));
    }
    0
}

/// `EvtIddCxMonitorUnassignSwapChain`: the OS took the swap-chain back (mode
/// change, monitor departure, device power-down). Stops the processing task.
///
/// # Safety
/// Registered with IddCx.
pub unsafe extern "C" fn unassign(monitor: bindings::IDDCX_MONITOR) -> NTSTATUS {
    log_line(&format!("EvtIddCxMonitorUnassignSwapChain monitor={monitor:?}"));
    stop_task(monitor as usize);
    0
}

/// Signals the monitor's processing thread to stop, joins it and releases the
/// stop event. The thread itself deletes the swap-chain WDF object on exit
/// (that is what makes the OS hand us a fresh swap-chain later). Idempotent:
/// the task is an `Option::take`, so a second stop is a no-op.
pub(crate) fn stop_task(monitor: usize) {
    let task = shared::lock()
        .monitors
        .iter_mut()
        .find(|m| m.handle == monitor)
        .and_then(|m| m.swapchain_task.take());
    if let Some(task) = task {
        // SAFETY: the event is ours and still open; it is closed after the
        // join so the thread can never observe a closed handle.
        unsafe {
            let _ = SetEvent(HANDLE(task.stop_event as *mut core::ffi::c_void));
        }
        if let Some(thread) = task.thread {
            let _ = thread.join();
        }
        // SAFETY: the thread is joined; nobody references the event anymore.
        unsafe {
            let _ = CloseHandle(HANDLE(task.stop_event as *mut core::ffi::c_void));
        }
        log_line("swap-chain task stopped");
    }
}

/// Creates a D3D11 device on the render adapter the OS picked for the
/// swap-chain (IddSampleDriver's `Direct3DDevice::Init`). Returns the device,
/// its immediate context, and logs the adapter description — the LUID choice
/// is the open platform question of the spike (plan §2.6), so it is logged.
fn create_render_device(luid: bindings::LUID) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let win_luid = LUID {
        LowPart: luid.LowPart,
        HighPart: luid.HighPart,
    };
    // SAFETY: standard DXGI/D3D11 device creation; COM owns the lifetimes.
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        // EnumAdapterByLuid lives on IDXGIFactory4 (DXGI 1.4); every OS we
        // target ships it.
        let factory4: IDXGIFactory4 =
            factory.cast().map_err(|e| format!("QI IDXGIFactory4: {e}"))?;
        let adapter: IDXGIAdapter = factory4
            .EnumAdapterByLuid(win_luid)
            .map_err(|e| format!("EnumAdapterByLuid({:#x}:{:#x}): {e}", win_luid.HighPart, win_luid.LowPart))?;
        if let Ok(adapter1) = adapter.cast::<IDXGIAdapter1>() {
            if let Ok(desc) = adapter1.GetDesc1() {
                let end = desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len());
                log_line(&format!(
                    "render adapter description: {}",
                    String::from_utf16_lossy(&desc.Description[..end])
                ));
            }
        }
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;
        let context = context.ok_or("D3D11CreateDevice returned no context")?;
        Ok((device, context))
    }
}

/// The frame loop, on its own thread: hand our DXGI device to IddCx, then
/// acquire → (log metadata) → finish in a loop until the stop event fires or
/// the swap-chain dies. Mirrors IddSampleDriver's `SwapChainProcessor::RunCore`.
fn processing_loop(swapchain: usize, available_event: usize, stop_event: usize, device: ID3D11Device) {
    let swapchain = swapchain as bindings::IDDCX_SWAPCHAIN;

    // IddCxSwapChainSetDevice needs our D3D11 device as an IDXGIDevice.
    let dxgi_device: IDXGIDevice = match device.cast() {
        Ok(d) => d,
        Err(e) => {
            log_line(&format!("QI IDXGIDevice failed: {e}"));
            finish_loop(swapchain);
            return;
        }
    };
    let set_device = bindings::IDARG_IN_SWAPCHAINSETDEVICE {
        pDevice: dxgi_device.as_raw().cast(),
    };
    // SAFETY: swapchain is the handle from the assign callback; pDevice is a
    // valid IDXGIDevice we keep alive for the loop's duration.
    let hr = unsafe { iddcx::swapchain_set_device(swapchain, &set_device) };
    if hr < 0 {
        log_line(&format!("IddCxSwapChainSetDevice -> {hr:#010x}"));
        finish_loop(swapchain);
        return;
    }
    log_line("IddCxSwapChainSetDevice ok, frame loop starts");

    let mut frames: u64 = 0;
    let mut last_dirty_log = Instant::now() - Duration::from_secs(60);
    loop {
        // SAFETY: swapchain is ours until we delete it at loop exit.
        let (hr, out) = unsafe { iddcx::swapchain_release_and_acquire(swapchain) };
        if hr == E_PENDING {
            // No new frame: wait for the next surface or the stop signal
            // (16 ms timeout keeps the loop responsive like the sample).
            let handles = [
                HANDLE(available_event as *mut core::ffi::c_void),
                HANDLE(stop_event as *mut core::ffi::c_void),
            ];
            // SAFETY: both handles are valid Win32 events.
            let wait = unsafe { WaitForMultipleObjects(&handles, false, 16) };
            if wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT {
                continue;
            }
            if wait == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                break; // stop requested
            }
            log_line(&format!("WaitForMultipleObjects -> {wait:?}, exiting frame loop"));
            break;
        }
        if hr < 0 {
            // e.g. DXGI_ERROR_ACCESS_LOST: the swap-chain is gone; the OS will
            // assign a new one after we delete this object below.
            log_line(&format!(
                "IddCxSwapChainReleaseAndAcquireBuffer -> {hr:#010x}, exiting frame loop"
            ));
            break;
        }

        frames += 1;
        let meta = &out.MetaData;
        if meta.DirtyRectCount > 0 && last_dirty_log.elapsed() >= DIRTY_LOG_INTERVAL {
            log_frame_details(swapchain, meta);
            last_dirty_log = Instant::now();
        }
        if frames == 1 || frames % 300 == 0 {
            log_line(&format!(
                "frame loop alive: frames={frames} last_frame={} dirty={} move={}",
                meta.PresentationFrameNumber, meta.DirtyRectCount, meta.MoveRegionCount
            ));
        }

        // The acquired surface comes with a reference we own; release it now
        // (we reacquire through the metadata anyway — sample semantics).
        if !meta.pSurface.is_null() {
            // SAFETY: pSurface is a live IDXGIResource whose reference we own
            // until the next acquire; from_raw takes that ownership and drops
            // (Releases) it here.
            drop(unsafe { IDXGIResource::from_raw(meta.pSurface.cast()) });
        }

        // SAFETY: we hold an acquired frame.
        let hr = unsafe { iddcx::swapchain_finished_processing_frame(swapchain) };
        if hr < 0 {
            log_line(&format!(
                "IddCxSwapChainFinishedProcessingFrame -> {hr:#010x}, exiting frame loop"
            ));
            break;
        }
    }

    log_line(&format!("frame loop exit after {frames} frames"));
    finish_loop(swapchain);
}

/// Logs the dirty rects of the current frame (throttled by the caller).
fn log_frame_details(swapchain: bindings::IDDCX_SWAPCHAIN, meta: &bindings::IDDCX_METADATA) {
    let mut rects = [bindings::RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    }; MAX_LOGGED_RECTS];
    // SAFETY: a frame is currently acquired; rects is a valid buffer.
    let (hr, copied) = unsafe { iddcx::swapchain_get_dirty_rects(swapchain, &mut rects) };
    if hr < 0 {
        log_line(&format!("IddCxSwapChainGetDirtyRects -> {hr:#010x}"));
        return;
    }
    let mut dump = String::new();
    for r in rects.iter().take(copied as usize) {
        use std::fmt::Write as _;
        let _ = write!(dump, " ({},{})-({},{})", r.left, r.top, r.right, r.bottom);
    }
    log_line(&format!(
        "frame={} dirty={} move={} first_rects:{dump}",
        meta.PresentationFrameNumber, meta.DirtyRectCount, meta.MoveRegionCount
    ));
}

/// Loop teardown: delete the swap-chain WDF object so the OS can provide a
/// new one (IddSampleDriver's Run does exactly this on exit).
fn finish_loop(swapchain: bindings::IDDCX_SWAPCHAIN) {
    // SAFETY: the loop is over; the swap-chain object belongs to us.
    unsafe { delete_wdf_object(swapchain as usize) };
}

/// `WdfObjectDelete` on a raw handle (the swap-chain is a WDF object).
///
/// # Safety
/// `object` must be a valid, owned WDF object handle.
unsafe fn delete_wdf_object(object: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfObjectDelete, object as wdk_sys::WDFOBJECT);
    }
}
