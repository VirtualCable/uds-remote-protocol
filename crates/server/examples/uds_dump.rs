//! Dumps the udsaudio driver loopback ring (via IOCTL_READ_LOOPBACK through
//! `UdsLoopbackSource`) to a raw PCM file (48 kHz stereo s16le), to inspect
//! exactly what the driver harvest path delivers.
//!
//! Usage: uds_dump <out.pcm> [seconds]

#[cfg(all(windows, feature = "windows-capture"))]
fn main() -> anyhow::Result<()> {
    use server::audio::AudioSource;
    use server::audio_uds::UdsLoopbackSource;
    use std::io::Write as _;

    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "uds_dump.pcm".to_string());
    let seconds: u64 = args.next().as_deref().unwrap_or("30").parse()?;

    const RATE: u32 = 48_000;
    const CHANNELS: u8 = 2;
    const FRAME_SAMPLES: usize = 960; // 20 ms
    let mut source = UdsLoopbackSource::new(RATE, CHANNELS, FRAME_SAMPLES)?;
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut frame = vec![0i16; FRAME_SAMPLES * usize::from(CHANNELS)];
    let frames = seconds * 1000 / 20;
    let start = std::time::Instant::now();
    for i in 0..frames {
        source.fill_frame(&mut frame)?;
        for s in &frame {
            out.write_all(&s.to_le_bytes())?;
        }
        // Pace to real time: the ring is filled by the reader thread.
        let target = start + std::time::Duration::from_millis((i + 1) * 20);
        if let Some(wait) = target.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    out.flush()?;
    eprintln!("wrote {seconds}s of 48kHz stereo s16le to {path}");
    Ok(())
}

#[cfg(not(all(windows, feature = "windows-capture")))]
fn main() {
    eprintln!("uds_dump needs Windows with the windows-capture feature");
    std::process::exit(1);
}
