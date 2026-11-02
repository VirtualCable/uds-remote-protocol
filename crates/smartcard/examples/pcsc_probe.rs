// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Manual probe for the PC/SC backend against a real reader + card.
//!
//! Lab use only: `sudo cargo run -p smartcard --features pcsc-backend \
//!   --example pcsc_probe`

use smartcard::{SmartcardIntegration, pcsc_backend::PcscBackend, types::ScardIORequest};

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn main() {
    let backend = PcscBackend::new();

    let ctx = match backend.establish_context(2 /* SCARD_SCOPE_USER */) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("establish_context failed: {e:#010x}");
            std::process::exit(1);
        }
    };
    println!("context: ok");

    let readers = backend.list_readers(&ctx, None).unwrap_or_else(|e| {
        eprintln!("list_readers failed: {e:#010x}");
        std::process::exit(1);
    });
    println!("readers: {readers:?}");
    if readers.is_empty() {
        std::process::exit(1);
    }

    let conn = backend
        .connect(
            &ctx,
            &readers[0],
            2, /* SCARD_SHARE_SHARED */
            3, /* T0|T1 */
        )
        .unwrap_or_else(|e| {
            eprintln!("connect failed: {e:#010x}");
            std::process::exit(1);
        });
    println!("connected, active_protocol = {}", conn.active_protocol);

    // SELECT GIDS applet (AID A00000039742544659).
    let select_gids = [
        0x00, 0xA4, 0x04, 0x00, 0x09, 0xA0, 0x00, 0x00, 0x03, 0x97, 0x42, 0x54, 0x46, 0x59,
    ];
    let pci = ScardIORequest {
        protocol: conn.active_protocol,
        extra_bytes: Vec::new(),
    };
    match backend.transmit(&conn.handle, &pci, &select_gids) {
        Ok(res) => println!("SELECT GIDS -> {}", hex(&res.recv_buffer)),
        Err(e) => println!("transmit failed: {e:#010x}"),
    }

    // VERIFY user PIN 1234 (GIDS: P2=0x80, exact length, no padding).
    let verify_pin = [0x00, 0x20, 0x00, 0x80, 0x04, 0x31, 0x32, 0x33, 0x34];
    match backend.transmit(&conn.handle, &pci, &verify_pin) {
        Ok(res) => println!("VERIFY PIN -> {}", hex(&res.recv_buffer)),
        Err(e) => println!("verify failed: {e:#010x}"),
    }

    // GET CHALLENGE (GIDS does not implement it: expect 6D 00).
    let get_challenge = [0x00, 0x84, 0x00, 0x00, 0x08];
    match backend.transmit(&conn.handle, &pci, &get_challenge) {
        Ok(res) => println!("GET CHALLENGE -> {}", hex(&res.recv_buffer)),
        Err(e) => println!("transmit failed: {e:#010x}"),
    }

    backend.disconnect(&conn.handle, 0 /* SCARD_LEAVE_CARD */).ok();
    backend.release_context(&ctx).ok();
    println!("done");
}
