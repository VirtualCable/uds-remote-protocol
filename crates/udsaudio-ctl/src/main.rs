//! Test/verification utility for the UDS virtual audio driver.
//!
//! Windows-only. Subcommands:
//!
//! - `list`: enumerate render/capture endpoints (find the UDS ones);
//! - `push-tone [seconds] [freq]`: feed a sine wave into the virtual
//!   microphone through the `\\.\UDSAudioCtl` control device;
//! - `verify-mic [seconds] [freq]`: push the tone while capturing from the
//!   UDS microphone endpoint with WASAPI; PASS when the tone is detected;
//! - `verify-loopback [seconds] [freq]`: render the tone to the UDS speaker
//!   while capturing its WASAPI loopback; PASS when the tone is detected
//!   (proves the render→loopback copy in the driver).

#[cfg(windows)]
mod imp;

fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        imp::run()
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("udsaudio-ctl only runs on Windows: the driver is a Windows kernel driver")
    }
}
