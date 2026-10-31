// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.

//! Windows implementation of `udsscard-ctl run`.

use std::io::BufRead;
use std::os::windows::io::FromRawHandle;

use anyhow::{Context, bail};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use udsscard_proto::{
    CMD_EJECT_CARD, CMD_INJECT_CARD, CMD_PIPE_NAME, IO_PIPE_NAME, IO_RESP_OK, IO_TRANSMIT,
    read_frame, write_frame,
};

/// Same fake card ATR the driver's stand-alone spike used ("eUDS-Card").
const DEFAULT_ATR: &str = "3b8901455554532d43617264 96";

fn parse_hex(s: &str) -> anyhow::Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace() && *c != ':').collect();
    if !clean.len().is_multiple_of(2) {
        bail!("hex string must have an even number of digits");
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16)
                .with_context(|| format!("bad hex byte at offset {i}"))
        })
        .collect()
}

fn hex_of(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Create one byte-mode named pipe instance and wait for the driver to
/// connect. Blocking; a client that connected before `ConnectNamedPipe`
/// (ERROR_PIPE_CONNECTED) counts as success.
fn create_and_wait(name: &str) -> anyhow::Result<std::fs::File> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; the returned
    // handle is checked and owned by the File we build from it.
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string.
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            8192,
            8192,
            0,
            None,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: last-error is read immediately after the failing call.
        let err = unsafe { GetLastError() };
        bail!("CreateNamedPipeW({name}) failed: {err:?}");
    }
    // SAFETY: `handle` is a live pipe server handle; no overlapped IO. A
    // client that connected before this call (ERROR_PIPE_CONNECTED) is fine.
    if unsafe { ConnectNamedPipe(handle, None) }.is_err() {
        // SAFETY: last-error is read immediately after the failing call.
        let err = unsafe { GetLastError() };
        if err != ERROR_PIPE_CONNECTED {
            bail!("ConnectNamedPipe({name}) failed: {err:?}");
        }
    }
    println!("[ctl] driver connected on {name}");
    // SAFETY: ownership of `handle` moves into the File.
    Ok(unsafe { std::fs::File::from_raw_handle(handle.0) })
}

pub fn run() -> anyhow::Result<()> {
    let mut atr = parse_hex(DEFAULT_ATR)?;
    let mut auto_inject = true;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    if args.is_empty() || args[0] != "run" {
        bail!("usage: udsscard-ctl run [--atr HEX] [--no-auto-inject]");
    }
    i += 1;
    while i < args.len() {
        match args[i].as_str() {
            "--atr" => {
                i += 1;
                atr = parse_hex(args.get(i).context("--atr needs a hex value")?)?;
            }
            "--no-auto-inject" => auto_inject = false,
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    println!("[ctl] waiting for driver on {CMD_PIPE_NAME} ...");
    let mut cmd = create_and_wait(CMD_PIPE_NAME)?;
    let io = create_and_wait(IO_PIPE_NAME)?;

    if auto_inject {
        write_frame(&mut cmd, CMD_INJECT_CARD, &atr)?;
        println!("[ctl] injected card, ATR {}", hex_of(&atr));
    }

    // IO loop: answer every transmit with `6D 00` (INS not supported).
    std::thread::spawn(move || {
        let mut io = io;
        loop {
            match read_frame(&mut io) {
                Ok((IO_TRANSMIT, apdu)) => {
                    println!("[ctl] transmit {} bytes: {}", apdu.len(), hex_of(&apdu));
                    if let Err(e) = write_frame(&mut io, IO_RESP_OK, &[0x6D, 0x00]) {
                        println!("[ctl] io write failed: {e}");
                        break;
                    }
                }
                Ok((tag, payload)) => {
                    println!("[ctl] unknown io frame tag={tag} len={}", payload.len());
                }
                Err(e) => {
                    println!("[ctl] io pipe closed: {e}");
                    break;
                }
            }
        }
    });

    println!("[ctl] commands: inject | eject | quit");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        match line.trim() {
            "inject" => {
                write_frame(&mut cmd, CMD_INJECT_CARD, &atr)?;
                println!("[ctl] injected");
            }
            "eject" => {
                write_frame(&mut cmd, CMD_EJECT_CARD, &[])?;
                println!("[ctl] ejected");
            }
            "quit" | "exit" => break,
            "" => {}
            other => println!("[ctl] unknown command: {other}"),
        }
    }
    Ok(())
}
