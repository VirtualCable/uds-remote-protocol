// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! End-to-end smartcard dispatcher test (§17): requests are encoded,
//! decoded (the wire loop) and executed by a real `SmartcardService`
//! against the scripted `DummyBackend`, asserting reply tags and the
//! `request_id` echo.

use std::sync::Arc;
use std::time::Duration;

use channels::smartcard::SmartcardService;
use protocol::{ReaderState, SCOPE_USER, SmartcardMessage};
use channels::smartcard::dummy::{DUMMY_ATR, DUMMY_READER};
use channels::smartcard::{
    DummyBackend, SCARD_E_CANCELLED, SCARD_E_NO_SERVICE, SCARD_PROTOCOL_T1, SCARD_SHARE_SHARED,
    SCARD_STATE_PRESENT, SmartcardIntegration,
};

/// Generous reply deadline: the dispatcher is idle in these tests, so any
/// wait means something is broken (an unanswered request hangs forever).
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

fn start(backend: Option<Arc<DummyBackend>>) -> (SmartcardService, flume::Receiver<SmartcardMessage>) {
    let (sink, rx) = flume::unbounded();
    let backend: Option<Arc<dyn SmartcardIntegration>> =
        backend.map(|b| b as Arc<dyn SmartcardIntegration>);
    let svc = SmartcardService::start(backend, sink);
    (svc, rx)
}

/// Runs one request through encode → dispatch → decode and returns the
/// reply.
fn roundtrip(svc: &SmartcardService, rx: &flume::Receiver<SmartcardMessage>, req: &SmartcardMessage) -> SmartcardMessage {
    let decoded = SmartcardMessage::decode(&req.encode()).expect("request re-decodes");
    svc.enqueue(decoded);
    rx.recv_timeout(REPLY_TIMEOUT).expect("reply arrives")
}

/// Establishes a context and connects a card; returns `(context, card)`.
fn establish_and_connect(
    svc: &SmartcardService,
    rx: &flume::Receiver<SmartcardMessage>,
    next_id: &mut u32,
) -> (u32, u32) {
    *next_id += 1;
    let reply = roundtrip(
        svc,
        rx,
        &SmartcardMessage::EstablishContext {
            request_id: *next_id,
            scope: SCOPE_USER,
        },
    );
    let context = match reply {
        SmartcardMessage::ContextEstablished { request_id, context } => {
            assert_eq!(request_id, *next_id);
            context
        }
        other => panic!("expected ContextEstablished, got {other:?}"),
    };
    *next_id += 1;
    let reply = roundtrip(
        svc,
        rx,
        &SmartcardMessage::Connect {
            request_id: *next_id,
            context,
            reader: DUMMY_READER.to_string(),
            share_mode: SCARD_SHARE_SHARED,
            preferred_protocols: SCARD_PROTOCOL_T1,
        },
    );
    let card = match reply {
        SmartcardMessage::Connected {
            request_id,
            card,
            active_protocol,
        } => {
            assert_eq!(request_id, *next_id);
            assert_eq!(active_protocol, SCARD_PROTOCOL_T1);
            card
        }
        other => panic!("expected Connected, got {other:?}"),
    };
    (context, card)
}

#[test]
#[expect(clippy::too_many_lines, reason = "flat scripted conversation")]
fn full_operation_cycle_against_dummy_backend() {
    let (svc, rx) = start(Some(Arc::new(DummyBackend::new())));
    let mut id = 0u32;
    let (context, card) = establish_and_connect(&svc, &rx, &mut id);

    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::ListReaders {
                request_id: id,
                context,
            },
        ),
        SmartcardMessage::Readers {
            request_id: id,
            readers: vec![DUMMY_READER.to_string()],
        }
    );

    id += 1;
    let apdu = vec![0x00, 0xA4, 0x04, 0x00];
    let mut expected = apdu.clone();
    expected.extend_from_slice(&[0x90, 0x00]);
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::Transmit {
                request_id: id,
                card,
                send_protocol: SCARD_PROTOCOL_T1,
                data: apdu,
            },
        ),
        SmartcardMessage::TransmitReply {
            request_id: id,
            recv_protocol: SCARD_PROTOCOL_T1,
            data: expected,
        }
    );

    id += 1;
    assert_eq!(
        roundtrip(&svc, &rx, &SmartcardMessage::BeginTransaction { request_id: id, card }),
        SmartcardMessage::TransactionBegun { request_id: id }
    );
    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::EndTransaction {
                request_id: id,
                card,
                disposition: 0,
            },
        ),
        SmartcardMessage::TransactionEnded { request_id: id }
    );

    // GetStatusChange: parks for its timeout and replies with the states
    // populated (never an empty body, §17.3).
    id += 1;
    let watched = ReaderState {
        reader: DUMMY_READER.to_string(),
        current_state: SCARD_STATE_PRESENT,
        event_state: 0,
        atr: Vec::new(),
    };
    let reply = roundtrip(
        &svc,
        &rx,
        &SmartcardMessage::GetStatusChange {
            request_id: id,
            context,
            timeout_ms: 30,
            states: vec![watched],
        },
    );
    match reply {
        SmartcardMessage::StatusChange { request_id, states } => {
            assert_eq!(request_id, id);
            assert_eq!(states.len(), 1);
            assert_eq!(states[0].reader, DUMMY_READER);
            assert_eq!(states[0].event_state, SCARD_STATE_PRESENT);
            assert_eq!(states[0].atr, DUMMY_ATR);
        }
        other => panic!("expected StatusChange, got {other:?}"),
    }

    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::Disconnect {
                request_id: id,
                card,
                disposition: 0,
            },
        ),
        SmartcardMessage::Disconnected { request_id: id }
    );
    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::ReleaseContext {
                request_id: id,
                context,
            },
        ),
        SmartcardMessage::ContextReleased { request_id: id }
    );

    svc.stop();
}

#[test]
fn programmed_transmit_reply_goes_out_as_is() {
    let backend = Arc::new(DummyBackend::new());
    backend.program_transmit(&[0x00, 0xB0, 0x00, 0x00, 0x02], &[0xCA, 0xFE, 0x90, 0x00]);
    let (svc, rx) = start(Some(backend));
    let mut id = 0u32;
    let (_context, card) = establish_and_connect(&svc, &rx, &mut id);

    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::Transmit {
                request_id: id,
                card,
                send_protocol: SCARD_PROTOCOL_T1,
                data: vec![0x00, 0xB0, 0x00, 0x00, 0x02],
            },
        ),
        SmartcardMessage::TransmitReply {
            request_id: id,
            recv_protocol: SCARD_PROTOCOL_T1,
            data: vec![0xCA, 0xFE, 0x90, 0x00],
        }
    );
    svc.stop();
}

#[test]
fn no_backend_answers_no_service_to_everything() {
    let (svc, rx) = start(None);

    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::EstablishContext {
                request_id: 1,
                scope: SCOPE_USER,
            },
        ),
        SmartcardMessage::Error {
            request_id: 1,
            code: SCARD_E_NO_SERVICE,
        }
    );
    // GetStatusChange too — no thread is spawned for it.
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::GetStatusChange {
                request_id: 2,
                context: 1,
                timeout_ms: 10,
                states: Vec::new(),
            },
        ),
        SmartcardMessage::Error {
            request_id: 2,
            code: SCARD_E_NO_SERVICE,
        }
    );
    svc.stop();
}

#[test]
fn cancel_interrupts_a_parked_status_watch() {
    let (svc, rx) = start(Some(Arc::new(DummyBackend::new())));
    let mut id = 0u32;

    id += 1;
    let reply = roundtrip(
        &svc,
        &rx,
        &SmartcardMessage::EstablishContext {
            request_id: id,
            scope: SCOPE_USER,
        },
    );
    let context = match reply {
        SmartcardMessage::ContextEstablished { context, .. } => context,
        other => panic!("expected ContextEstablished, got {other:?}"),
    };

    // A long watch (detached thread) plus the cancel that interrupts it.
    id += 1;
    let gsc_id = id;
    let gsc = SmartcardMessage::decode(
        &SmartcardMessage::GetStatusChange {
            request_id: gsc_id,
            context,
            timeout_ms: 30_000,
            states: vec![ReaderState {
                reader: DUMMY_READER.to_string(),
                current_state: SCARD_STATE_PRESENT,
                event_state: 0,
                atr: Vec::new(),
            }],
        }
        .encode(),
    )
    .expect("gsc re-decodes");
    svc.enqueue(gsc);
    std::thread::sleep(Duration::from_millis(100)); // let the watcher park
    id += 1;
    let cancel_id = id;
    let cancel = SmartcardMessage::decode(
        &SmartcardMessage::Cancel {
            request_id: cancel_id,
            context,
        }
        .encode(),
    )
    .expect("cancel re-decodes");
    svc.enqueue(cancel);

    // Both replies arrive (order between threads is not guaranteed).
    let mut replies = vec![
        rx.recv_timeout(REPLY_TIMEOUT).expect("first reply"),
        rx.recv_timeout(REPLY_TIMEOUT).expect("second reply"),
    ];
    replies.sort_by_key(SmartcardMessage::request_id);
    assert_eq!(
        replies,
        vec![
            SmartcardMessage::Error {
                request_id: gsc_id,
                code: SCARD_E_CANCELLED,
            },
            SmartcardMessage::Cancelled {
                request_id: cancel_id,
            },
        ]
    );
    svc.stop();
}

#[test]
fn foreign_transaction_parks_until_end_then_runs() {
    let (svc, rx) = start(Some(Arc::new(DummyBackend::new())));
    let mut id = 0u32;
    let (_context, card_a) = establish_and_connect(&svc, &rx, &mut id);
    let (_context_b, card_b) = establish_and_connect(&svc, &rx, &mut id);
    assert_ne!(card_a, card_b);

    // Card A takes the transaction.
    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::BeginTransaction {
                request_id: id,
                card: card_a,
            },
        ),
        SmartcardMessage::TransactionBegun { request_id: id }
    );

    // Card B's transmit parks: no reply while A holds the card.
    id += 1;
    let parked_id = id;
    let parked = SmartcardMessage::decode(
        &SmartcardMessage::Transmit {
            request_id: parked_id,
            card: card_b,
            send_protocol: SCARD_PROTOCOL_T1,
            data: vec![0x00],
        }
        .encode(),
    )
    .expect("transmit re-decodes");
    svc.enqueue(parked);
    assert!(
        rx.recv_timeout(Duration::from_millis(150)).is_err(),
        "card B must wait for card A's transaction"
    );

    // Ending A's transaction releases the parked op (FIFO after the
    // EndTransaction reply).
    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::EndTransaction {
                request_id: id,
                card: card_a,
                disposition: 0,
            },
        ),
        SmartcardMessage::TransactionEnded { request_id: id }
    );
    assert_eq!(
        rx.recv_timeout(REPLY_TIMEOUT).expect("parked reply"),
        SmartcardMessage::TransmitReply {
            request_id: parked_id,
            recv_protocol: SCARD_PROTOCOL_T1,
            data: vec![0x00, 0x90, 0x00],
        }
    );
    svc.stop();
}

#[test]
fn stop_answers_parked_operations_with_cancelled() {
    let (svc, rx) = start(Some(Arc::new(DummyBackend::new())));
    let mut id = 0u32;
    let (_context, card_a) = establish_and_connect(&svc, &rx, &mut id);
    let (_context_b, card_b) = establish_and_connect(&svc, &rx, &mut id);

    id += 1;
    assert_eq!(
        roundtrip(
            &svc,
            &rx,
            &SmartcardMessage::BeginTransaction {
                request_id: id,
                card: card_a,
            },
        ),
        SmartcardMessage::TransactionBegun { request_id: id }
    );
    id += 1;
    let parked_id = id;
    svc.enqueue(SmartcardMessage::Transmit {
        request_id: parked_id,
        card: card_b,
        send_protocol: SCARD_PROTOCOL_T1,
        data: vec![0x00],
    });
    std::thread::sleep(Duration::from_millis(100)); // let it park

    // Teardown must not leave the parked request unanswered.
    svc.stop();
    assert_eq!(
        rx.recv_timeout(REPLY_TIMEOUT).expect("drained reply"),
        SmartcardMessage::Error {
            request_id: parked_id,
            code: SCARD_E_CANCELLED,
        }
    );
}
