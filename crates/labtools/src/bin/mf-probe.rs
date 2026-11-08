// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com
//! mf-probe: lists the video-encoder MFTs registered in the system and,
//! with `--encode N`, runs N synthetic 1080p30 frames through the chosen
//! VP9 MFT measuring encode time (§18.75 H1). Read-only lab tool.

#[cfg(all(windows, feature = "mf-probe"))]
use codec::VideoEncoder;

fn main() {
    #[cfg(all(windows, feature = "mf-probe"))]
    {
        let args: Vec<String> = std::env::args().collect();
        let encode_frames: u32 = args
            .iter()
            .skip_while(|a| *a != "--encode")
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let gpu = args.iter().any(|a| a == "--gpu");
        let h264 = args.iter().any(|a| a == "--h264");
        match codec::mf::enumerate_encoders() {
            Ok(list) if list.is_empty() => println!("no video-encoder MFTs found"),
            Ok(list) => {
                for m in &list {
                    println!(
                        "{:5} async={:<5} d3d11={:<5} {}",
                        m.subtype, m.async_mft, m.d3d11_aware, m.name
                    );
                }
                println!("-- {} MFT(s)", list.len());
            }
            Err(e) => {
                eprintln!("enumeration failed: {e}");
                std::process::exit(1);
            }
        }
        if encode_frames > 0 {
            std::process::exit(encode_test(encode_frames, gpu, h264));
        }
    }
    #[cfg(not(all(windows, feature = "mf-probe")))]
    {
        eprintln!("mf-probe needs Windows and the codec/codec-mf feature");
        std::process::exit(2);
    }
}

/// Fills `bgra` with the gradient + moving-square test pattern for frame `i`.
#[cfg(all(windows, feature = "mf-probe"))]
#[allow(clippy::cast_possible_truncation, clippy::many_single_char_names)]
fn fill_pattern(bgra: &mut [u8], w: u32, h: u32, i: u32) {
    for (n, px) in bgra.chunks_exact_mut(4).enumerate() {
        let (x, y) = ((n as u32) % w, (n as u32) / w);
        px[0] = (x * 255 / w) as u8; // B
        px[1] = (y * 255 / h) as u8; // G
        px[2] = ((x + y) * 255 / (w + h)) as u8; // R
        px[3] = 255;
    }
    let sq = 200u32;
    let sx = (i * 40) % (w - sq);
    for y in 200..200 + sq {
        for x in sx..sx + sq {
            let o = ((y * w + x) * 4) as usize;
            bgra[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
    }
}

/// Encodes `frames` synthetic frames (gradient + moving square) and prints
/// timing/size stats. With `gpu` the frames are uploaded to a D3D11 texture
/// and fed through the zero-copy path (`encode_gpu`); `h264` selects the
/// H264 MFT instead of VP9 (the RTX VM has no VP9 hardware encoder, so the
/// GPU path is validated with H264 — §18.75 H2). Exit code 0 on success.
#[cfg(all(windows, feature = "mf-probe"))]
fn encode_test(frames: u32, gpu: bool, h264: bool) -> i32 {
    let (w, h, fps) = (1920u32, 1080u32, 30u32);
    let kbps: u32 = std::env::var("MF_KBPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let codec_sel = if h264 {
        codec::mf::MftCodec::H264
    } else {
        codec::mf::MftCodec::Vp9
    };
    let device = if gpu {
        match create_d3d11_device() {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("D3D11 device creation failed: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    // Lab probe: hardware only when GPU is requested; otherwise accept any
    // registered MFT (software included).
    let mut enc = match codec::mf::MfEncoder::new_mft(codec_sel, w, h, fps, kbps, gpu, device.clone())
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("MfEncoder init failed: {e}");
            return 1;
        }
    };
    println!("encoder ready: {} (gpu={})", enc.mft_name(), gpu);
    let mut staging = None;
    let mut target = None;
    if gpu {
        match create_probe_textures(device.as_ref().expect("device"), w, h) {
            Ok((s, t)) => {
                staging = Some(s);
                target = Some(t);
            }
            Err(e) => {
                eprintln!("probe textures: {e}");
                return 1;
            }
        }
    }
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mut total_ms = 0f64;
    let mut max_ms = 0f64;
    let mut total_bytes = 0usize;
    let mut keyframes = 0u32;
    for i in 0..frames {
        fill_pattern(&mut bgra, w, h, i);
        let start = std::time::Instant::now();
        let cpu_feed = std::env::var_os("MF_CPU_FEED").is_some();
        let packet = if gpu && !cpu_feed {
            let dev = device.as_ref().expect("device");
            let ctx = match unsafe { dev.GetImmediateContext() } {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("no immediate context: {e}");
                    return 1;
                }
            };
            upload_to_texture(&ctx, staging.as_ref().expect("staging"), &bgra, w, h);
            unsafe {
                ctx.CopyResource(
                    target.as_ref().expect("target"),
                    staging.as_ref().expect("staging"),
                );
            }
            match enc.encode_gpu(target.as_ref().expect("target")) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("GPU encode failed at frame {i}: {e}");
                    return 1;
                }
            }
        } else {
            match enc.encode(&codec::VideoFrame {
                width: w,
                height: h,
                stride: (w * 4) as usize,
                data: &bgra,
            }) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("encode failed at frame {i}: {e}");
                    return 1;
                }
            }
        };
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        total_ms += ms;
        max_ms = max_ms.max(ms);
        total_bytes += packet.data.len();
        if packet.keyframe {
            keyframes += 1;
        }
    }
    println!(
        "encoded {} frames: avg {:.2} ms max {:.2} ms, {} bytes total ({} keyframes)",
        frames,
        total_ms / f64::from(frames),
        max_ms,
        total_bytes,
        keyframes
    );
    0
}

/// Creates a D3D11 device for the GPU probe. `MF_ADAPTER=<idx>` picks a
/// specific DXGI adapter (default: the LAST hardware adapter — on hybrid
/// VMs adapter 0 is the virtual display driver, not the real GPU).
#[cfg(all(windows, feature = "mf-probe"))]
fn create_d3d11_device() -> windows::core::Result<windows::Win32::Graphics::Direct3D11::ID3D11Device> {
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1};
    let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }?;
    let mut adapters: Vec<(String, IDXGIAdapter1)> = Vec::new();
    for i in 0.. {
        let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        let desc = unsafe { a.GetDesc1() }?;
        let name = {
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            String::from_utf16_lossy(&desc.Description[..end])
        };
        eprintln!("adapter {i}: {name}");
        adapters.push((name, a));
    }
    let pick: usize = std::env::var("MF_ADAPTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| adapters.len().saturating_sub(1));
    let (name, adapter) = adapters
        .into_iter()
        .nth(pick)
        .ok_or(windows::core::Error::from_thread())?;
    eprintln!("using adapter {pick}: {name}");
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            None,
        )?;
    }
    device.ok_or(windows::core::Error::from_thread())
}

/// Creates the staging (CPU-writable) and target (GPU, VP-eligible) BGRA
/// textures for the probe.
#[cfg(all(windows, feature = "mf-probe"))]
fn create_probe_textures(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    w: u32,
    h: u32,
) -> windows::core::Result<(
    windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
)> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_WRITE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut staging))? };
    desc.Usage = D3D11_USAGE_DEFAULT;
    desc.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
    desc.CPUAccessFlags = 0;
    let mut target: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut target))? };
    Ok((
        staging.ok_or(windows::core::Error::from_thread())?,
        target.ok_or(windows::core::Error::from_thread())?,
    ))
}

/// Uploads BGRA pixels into a staging texture via Map/Unmap.
#[cfg(all(windows, feature = "mf-probe"))]
fn upload_to_texture(
    ctx: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    staging: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    bgra: &[u8],
    w: u32,
    h: u32,
) {
    use windows::Win32::Graphics::Direct3D11::D3D11_MAP_WRITE;
    unsafe {
        let mut mapped = std::mem::zeroed();
        if ctx.Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&raw mut mapped)).is_ok() {
            let row = (w * 4) as usize;
            for y in 0..h as usize {
                let dst = std::slice::from_raw_parts_mut(
                    (mapped.pData as *mut u8).add(y * mapped.RowPitch as usize),
                    row,
                );
                dst.copy_from_slice(&bgra[y * row..(y + 1) * row]);
            }
            ctx.Unmap(staging, 0);
        }
    }
}
