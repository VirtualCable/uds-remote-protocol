// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Webcam sink that publishes decoded frames into the session shared slot
//! (`vcam::ipc`), which the "UDS Webcam" DirectShow filter (`vcam` crate)
//! serves to applications. Latest-frame-wins by design (§18.57).

use vcam::ipc::CamSharedWriter;

use crate::cam::CamSink;

/// [`CamSink`] writing decoded BGRA frames to the virtual camera slot.
pub struct VcamSink {
    writer: CamSharedWriter,
}

impl VcamSink {
    /// Creates (or attaches to) the session shared slot.
    pub fn create() -> anyhow::Result<Self> {
        Ok(Self { writer: CamSharedWriter::create()? })
    }
}

impl CamSink for VcamSink {
    fn write_frame(&mut self, frame: &codec::DecodedFrame, capture_ts_ms: u32) {
        // DecodedFrame is tight BGRA (stride == width*4).
        self.writer.write_frame(&frame.data, frame.width, frame.height, capture_ts_ms);
    }
}
