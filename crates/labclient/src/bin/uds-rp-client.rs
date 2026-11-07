// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Reference standalone client for the UDS remote protocol.
//!
//! Usage: `uds-rp-client [--addr host:port] [--ticket STR] [--key HEX]
//!         [--no-audio] [--smartcard] [--webcam] [--privacy] [--mic|--mic-tone HZ]`
//!
//! Without parameters the connect dialog asks for them; with all three of
//! `--addr`/`--ticket`/`--key` the client connects immediately. The ticket
//! is the human string (hashed to the wire form with SHA-384); the key is
//! the server's shared key in hex.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use labclient::gui::{ConnectForm, SessionFlags};
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    install_panic_hook();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("uds-rp-client: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut form = ConnectForm {
        addr: String::new(),
        ticket: String::new(),
        key_hex: String::new(),
    };
    let mut flags = SessionFlags {
        audio: true,
        smartcard: false,
        webcam: false,
        privacy_screen: false,
        audio_dump: None,
        printer_save: None,
        mic: labclient::gui::MicMode::Off,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-audio" => flags.audio = false,
            "--smartcard" => flags.smartcard = true,
            "--webcam" => flags.webcam = true,
            "--privacy" => flags.privacy_screen = true,
            // §18.57: received PDFs land in Downloads (or the given dir).
            "--printer" => flags.printer_save = Some(default_printer_dir()),
            "--printer-save" => {
                flags.printer_save = Some(take(&mut it, "--printer-save")?.into());
            }
            "--audio-dump" => flags.audio_dump = Some(take(&mut it, "--audio-dump")?.into()),
            // §9.2: capture the default input device.
            "--mic" => flags.mic = labclient::gui::MicMode::Device,
            // Lab aid: synthetic sine instead of a real device (Hz).
            "--mic-tone" => {
                let hz: f64 = take(&mut it, "--mic-tone")?
                    .parse()
                    .map_err(|_| anyhow::anyhow!("bad --mic-tone frequency (Hz)"))?;
                flags.mic = labclient::gui::MicMode::Tone(hz);
            }
            "--addr" => form.addr = take(&mut it, "--addr")?,
            "--ticket" => form.ticket = take(&mut it, "--ticket")?,
            "--key" => form.key_hex = take(&mut it, "--key")?,
            other => bail!("unknown argument: {other}"),
        }
    }
    let complete = !form.addr.is_empty() && !form.ticket.is_empty() && !form.key_hex.is_empty();
    let form = if complete {
        Some(form)
    } else {
        // Whatever was passed on the command line prefills the dialog.
        if form.addr.is_empty() && form.ticket.is_empty() && form.key_hex.is_empty() {
            None
        } else {
            Some(form)
        }
    };
    labclient::gui::run_gui(form, flags).context("GUI")
}

fn take(it: &mut std::iter::Skip<std::env::Args>, flag: &str) -> Result<String> {
    it.next().context(format!("missing value for {flag}"))
}

/// Default destination for redirected print jobs: the user's Downloads
/// folder, falling back to the temp dir. Downloads is a *known folder* and
/// may be redirected anywhere (another drive, a network path), so guessing
/// `%USERPROFILE%\Downloads` is wrong — ask the shell for the real path.
#[cfg(windows)]
fn default_printer_dir() -> std::path::PathBuf {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    unsafe {
        if let Ok(path) = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) {
            let dir = path.to_string().unwrap_or_default();
            CoTaskMemFree(Some(path.as_ptr() as *const _));
            if !dir.is_empty() {
                return std::path::PathBuf::from(dir);
            }
        }
    }
    default_printer_dir_fallback()
}

/// Non-Windows fallback (and Windows fallback if the known-folder query
/// fails): `$HOME/Downloads` if it exists, else the temp dir.
#[cfg(not(windows))]
fn default_printer_dir() -> std::path::PathBuf {
    default_printer_dir_fallback()
}

fn default_printer_dir_fallback() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(std::env::temp_dir, |home| {
            std::path::PathBuf::from(home).join("Downloads")
        })
}

/// Routes worker-thread panics into the tracing log: launched from the
/// desktop there is no console, so a panicking decode/audio thread would
/// otherwise die silently (the default hook only writes to stderr). The
/// default hook still runs afterwards, keeping console behavior unchanged.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload().downcast_ref::<&str>().map_or_else(
            || {
                info.payload()
                    .downcast_ref::<String>()
                    .map_or("<non-string payload>", String::as_str)
            },
            |s| s,
        );
        let location = info.location().map_or_else(
            || "<unknown>".to_string(),
            |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
        );
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        tracing::error!(thread = name, %location, payload, "thread panicked");
        default_hook(info);
    }));
}
