// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Latency probe message tests (design doc §12.1): 2-step NTP-style scheme
//! with optional response. RTT = (t4 - t1) - (t3 - t2), no synchronized
//! clocks needed.

use protocol::{Probe, ProbeAck, ProbeError, compute_rtt};

#[test]
fn probe_roundtrip() {
    let probe = Probe { t1: 123_456_789 };
    let bytes = probe.encode();
    assert_eq!(bytes.len(), 9); // tag(1) + t1(8)
    assert_eq!(Probe::decode(&bytes).unwrap(), probe);
}

#[test]
fn probe_ack_roundtrip() {
    let ack = ProbeAck {
        t1: 100,
        t2: 200,
        t3: 250,
    };
    let bytes = ack.encode();
    assert_eq!(bytes.len(), 25); // tag(1) + 3 timestamps(24)
    assert_eq!(ProbeAck::decode(&bytes).unwrap(), ack);
}

#[test]
fn probe_rejects_garbage() {
    assert!(Probe::decode(&[]).is_err());
    assert!(Probe::decode(&[1, 2, 3]).is_err()); // truncated
    assert!(Probe::decode(&[0xFF; 9]).is_err()); // unknown tag
}

#[test]
fn probe_ack_rejects_garbage() {
    assert!(ProbeAck::decode(&[0u8; 25]).is_err()); // wrong tag
    assert!(ProbeAck::decode(&[2, 1, 2, 3]).is_err()); // truncated
}

#[test]
fn rtt_formula_is_ntp_style() {
    // t1=1000 (A sends), t2=1100 (B receives), t3=1105 (B replies),
    // t4=1250 (A receives the ack):
    // RTT = (1250 - 1000) - (1105 - 1100) = 250 - 5 = 245
    assert_eq!(compute_rtt(1000, 1100, 1105, 1250), 245);
    // Zero processing time at B: RTT = pure network round trip
    assert_eq!(compute_rtt(1000, 1100, 1100, 1240), 240);
}

#[test]
fn rtt_saturates_on_clock_anomalies() {
    // t4 < t1 (negative round trip): the measurement is invalid -> 0.
    assert_eq!(compute_rtt(2000, 0, 0, 1000), 0);
    // t3 < t2 (B's clock jumped backwards): processing time counts as 0,
    // so RTT is the full undiscounted round trip (conservative).
    assert_eq!(compute_rtt(1000, 1100, 1050, 1250), 250);
}

#[test]
fn probe_error_is_displayable() {
    let err = ProbeError::Truncated;
    assert_ne!(format!("{err}"), "");
}
