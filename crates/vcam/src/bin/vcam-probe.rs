// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Deterministic vcam probe: builds a capture graph exactly like a browser
//! would (ICaptureGraphBuilder::RenderStream to a null renderer), runs it
//! for a few seconds and reports. Exercises the real COM path of the "UDS
//! Webcam" filter without browser noise. The filter's own trace lands in
//! C:\ProgramData\UdsRp\vcam.log.

use windows::Win32::Media::DirectShow::{ICaptureGraphBuilder, IGraphBuilder, IMediaControl};
use windows::Win32::Media::MediaFoundation::{CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, PIN_CATEGORY_CAPTURE};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::GUID;

/// CLSID of the "UDS Webcam" filter (crates/vcam/src/globals.rs).
const CLSID_UDS_VCAM: GUID = GUID::from_u128(0x9e1a7c42_3b5d_4f6a_8c1e_2d3f4a5b6c7d);
/// CLSID_NullRenderer (not exposed by the windows crate).
const CLSID_NULL_RENDERER: GUID = GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);

fn main() -> anyhow::Result<()> {
    use anyhow::Context as _;
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let graph: IGraphBuilder =
            CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER).context("FilterGraph")?;
        let builder: ICaptureGraphBuilder =
            CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)
                .context("CaptureGraphBuilder2")?;
        builder.SetFiltergraph(&graph).context("SetFiltergraph")?;

        let cam: windows::Win32::Media::DirectShow::IBaseFilter =
            CoCreateInstance(&CLSID_UDS_VCAM, None, CLSCTX_INPROC_SERVER)
                .context("CoCreateInstance(UDS Webcam) — is vcam.dll registered?")?;
        graph.AddFilter(&cam, &windows::core::HSTRING::from("UDS Webcam")).context("AddFilter(cam)")?;

        let null: windows::Win32::Media::DirectShow::IBaseFilter =
            CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER).context("NullRenderer")?;
        graph.AddFilter(&null, &windows::core::HSTRING::from("Null")).context("AddFilter(null)")?;

        println!("rendering stream...");
        builder
            .RenderStream(Some(&PIN_CATEGORY_CAPTURE), &cam, None, &null)
            .context("RenderStream")?;
        println!("stream connected");

        let control: IMediaControl = windows::core::Interface::cast(&graph).context("IMediaControl")?;
        control.Run().context("Run")?;
        println!("running 5s (frames land in vcam.log as 'stream: first sample')...");
        std::thread::sleep(std::time::Duration::from_secs(5));
        control.Stop().context("Stop")?;
    }
    println!("PROBE-OK");
    Ok(())
}
