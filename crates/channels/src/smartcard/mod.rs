// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Smartcard request dispatcher (§17): executes `SmartcardMessage`
//! requests arriving on `channel::SMARTCARD` against a
//! [`SmartcardIntegration`] backend and produces the replies.
//!
//! The backend trait is **synchronous and blocking** (`get_status_change`
//! parks up to its timeout), so dispatch never runs on the Tokio runtime —
//! the threading model mirrors freerdp-integration's device (§17.2):
//!
//! - one **dispatcher thread** runs every regular operation sequentially
//!   and owns the [`TransactionGate`]: operations that must wait for a
//!   foreign transaction are parked and re-run in arrival order, the 5 s
//!   idle expiry is driven by the receive timeout, and a released context
//!   drops its parked operations (answered with `SCARD_E_CANCELLED` — a
//!   request is never left unanswered, the server side would hang);
//! - each `GetStatusChange` gets a **dedicated watcher thread**, because it
//!   blocks inside the backend for seconds and the dispatcher must keep
//!   serving regular traffic meanwhile.
//!
//! Replies are pushed to a plain [`flume::Sender<SmartcardMessage>`] sink
//! the session drains onto the reliable stream; tests inject their own
//! receiver and drive the service without a session.
//!
//! Teardown order (mirrors freerdp-integration's `free_handler`): stop
//! accepting work → cancel every live context (blocked watches return) →
//! join the dispatcher (it drains parked ops with `SCARD_E_CANCELLED`) →
//! join the watchers.
//!
//! The backend contract ([`SmartcardIntegration`]), the opaque handle and
//! state types, the PC/SC constants and the [`TransactionGate`] live in the
//! submodules below (formerly the standalone `smartcard` crate).

pub mod consts;
pub mod dummy;
pub mod gate;
mod integration;
#[cfg(feature = "pcsc-backend")]
pub mod pcsc_backend;
pub mod types;

pub use consts::*;
pub use dummy::DummyBackend;
pub use gate::{GateOp, TRANSACTION_IDLE_TIMEOUT, TransactionGate};
pub use integration::SmartcardIntegration;
#[cfg(feature = "pcsc-backend")]
pub use pcsc_backend::PcscBackend;
pub use types::*;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use protocol::{ReaderState, SmartcardMessage};
use tracing::{debug, error, info, trace, warn};

/// Wire handles are `u32`, backend handles `u64`: zero-extend on the way
/// in and truncate on the way out (backend ids count up from 1, so the
/// low 32 bits are unique for any realistic session).
fn wire_id(raw: u64) -> u32 {
    u32::try_from(raw).unwrap_or(u32::MAX)
}

/// What the dispatcher remembers about a connected card: the negotiated
/// protocol (needed to rebuild the [`ScardHandle`] from the wire id) and
/// the context that owns it (so a released context drops its cards and
/// its parked operations).
#[derive(Debug, Clone, Copy)]
struct CardInfo {
    protocol: u32,
    context: u32,
}

/// Work items of the dispatcher thread.
#[derive(Debug)]
enum Work {
    /// One request received on the smartcard channel.
    Request(SmartcardMessage),
    /// Session teardown: drain parked operations and exit.
    Shutdown,
}

/// Handle of a running smartcard dispatcher. Created with
/// [`SmartcardService::start`], fed with [`SmartcardService::enqueue`] and
/// torn down by [`SmartcardService::stop`] or `Drop`.
pub struct SmartcardService {
    work_tx: flume::Sender<Work>,
    /// Reply sink, kept for the inline fallback when the dispatcher
    /// thread could not start (or died): requests are still answered.
    sink: flume::Sender<SmartcardMessage>,
    dispatcher: Option<std::thread::JoinHandle<()>>,
    watchers: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
    /// Raw ids of the live contexts, shared with the dispatcher so
    /// teardown can cancel them (unblocking parked watches).
    live_contexts: Arc<Mutex<BTreeSet<u64>>>,
    backend: Option<Arc<dyn SmartcardIntegration>>,
}

impl std::fmt::Debug for SmartcardService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartcardService")
            .field("running", &self.dispatcher.is_some())
            .field("backend", &self.backend.is_some())
            .finish_non_exhaustive()
    }
}

impl SmartcardService {
    /// Starts the dispatcher thread against `backend` (`None` answers
    /// every request with `SCARD_E_NO_SERVICE` — the PC/SC subsystem may
    /// be absent). Replies are sent to `sink`.
    #[must_use]
    pub fn start(
        backend: Option<Arc<dyn SmartcardIntegration>>,
        sink: flume::Sender<SmartcardMessage>,
    ) -> Self {
        let (work_tx, work_rx) = flume::unbounded();
        let live_contexts = Arc::new(Mutex::new(BTreeSet::new()));
        let watchers = Arc::new(Mutex::new(Vec::new()));
        let dispatcher = Dispatcher {
            backend: backend.clone(),
            sink: sink.clone(),
            gate: TransactionGate::new(),
            cards: HashMap::new(),
            live_contexts: Arc::clone(&live_contexts),
            watchers: Arc::clone(&watchers),
        };
        let spawned = std::thread::Builder::new()
            .name("scard-dispatch".into())
            .spawn(move || dispatcher.run(&work_rx));
        let dispatcher = match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                // The enqueue fallback keeps answering with NO_SERVICE.
                error!("cannot spawn the smartcard dispatcher thread: {e}");
                None
            }
        };
        info!(backend = backend.is_some(), "smartcard dispatcher started");
        Self {
            work_tx,
            sink,
            dispatcher,
            watchers,
            live_contexts,
            backend,
        }
    }

    /// Queues one request for execution. When the dispatcher thread is
    /// gone (spawn failure, panic) the request is still answered, with
    /// `SCARD_E_NO_SERVICE`.
    pub fn enqueue(&self, msg: SmartcardMessage) {
        debug_assert!(msg.is_request());
        let request_id = msg.request_id();
        if self.work_tx.send(Work::Request(msg)).is_err() {
            warn!("smartcard dispatcher is gone; answering SCARD_E_NO_SERVICE");
            let _ = self.sink.send(SmartcardMessage::Error {
                request_id,
                code: SCARD_E_NO_SERVICE,
            });
        }
    }

    /// Stops the service: cancels the live contexts, drains parked
    /// operations (answered with `SCARD_E_CANCELLED`) and joins every
    /// thread. Also runs on `Drop`.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let Some(handle) = self.dispatcher.take() else {
            return;
        };
        // Stop accepting work, then cancel every live context so watches
        // blocked inside the backend return (SCardCancel semantics).
        let _ = self.work_tx.send(Work::Shutdown);
        if let Some(backend) = &self.backend {
            let contexts: Vec<u64> = self
                .live_contexts
                .lock()
                .expect("live contexts poisoned")
                .iter()
                .copied()
                .collect();
            for raw in contexts {
                if let Err(code) = backend.cancel(&ScardContext::from_raw(raw)) {
                    warn!(context = raw, code, "smartcard cancel failed during teardown");
                }
            }
        }
        if handle.join().is_err() {
            error!("smartcard dispatcher thread panicked");
        }
        // A backend that ignores cancel delays each join by the watch's
        // remaining timeout at worst.
        for watcher in self.watchers.lock().expect("watchers poisoned").drain(..) {
            let _ = watcher.join();
        }
        debug!("smartcard dispatcher stopped");
    }
}

impl Drop for SmartcardService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// State owned by the dispatcher thread.
struct Dispatcher {
    backend: Option<Arc<dyn SmartcardIntegration>>,
    sink: flume::Sender<SmartcardMessage>,
    gate: TransactionGate<SmartcardMessage>,
    /// Wire card id → protocol + owning context.
    cards: HashMap<u32, CardInfo>,
    live_contexts: Arc<Mutex<BTreeSet<u64>>>,
    watchers: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

impl Dispatcher {
    fn run(mut self, rx: &flume::Receiver<Work>) {
        loop {
            // The receive timeout doubles as the transaction-expiry tick:
            // when a transaction is held, wake up when it should expire.
            let work = match self.gate.time_to_expiry(Instant::now()) {
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(work) => Some(work),
                    Err(flume::RecvTimeoutError::Timeout) => None,
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(work) => Some(work),
                    Err(_) => break,
                },
            };
            // Expire first, so a request arriving right at the deadline
            // does not revive a transaction that has already timed out.
            if let Some(card) = self.gate.expire_idle(Instant::now()) {
                debug!(card, "smartcard transaction idle for too long; released");
                self.drain_parked();
            }
            match work {
                Some(Work::Request(msg)) => self.handle(msg),
                Some(Work::Shutdown) => break,
                None => {}
            }
        }
        // The session is going away: answer the parked operations instead
        // of dropping them silently.
        for parked in self.gate.take_all() {
            self.reply_error(&parked, SCARD_E_CANCELLED);
        }
    }

    fn handle(&mut self, msg: SmartcardMessage) {
        trace!(?msg, "smartcard request");
        // GetStatusChange parks inside the backend for seconds: it gets
        // its own thread so the dispatcher keeps serving regular traffic.
        if let SmartcardMessage::GetStatusChange {
            request_id,
            context,
            timeout_ms,
            states,
        } = msg
        {
            self.spawn_watch(request_id, context, timeout_ms, &states);
            return;
        }
        let (op, card, context) = self.classify(&msg);
        if self.gate.must_wait(op, card) {
            trace!(?op, card, "smartcard op parked (foreign transaction)");
            self.gate.park(op, card, context, msg);
            return;
        }
        let code = self.execute(&msg);
        self.gate.on_completed(op, card, context, code, Instant::now());
        // Cancel only interrupts blocking status waits, but a released
        // context can never run its parked operations: drop them now.
        if op == GateOp::ReleaseContext && code == SCARD_S_SUCCESS {
            for parked in self.gate.take_context(context) {
                self.reply_error(&parked, SCARD_E_CANCELLED);
            }
        }
        self.drain_parked();
    }

    /// Runs the parked operations that no longer have to wait, in
    /// arrival order.
    fn drain_parked(&mut self) {
        while let Some((op, card, context, parked)) = self.gate.next_runnable() {
            trace!(?op, card, "smartcard op resumed");
            let code = self.execute(&parked);
            self.gate.on_completed(op, card, context, code, Instant::now());
        }
    }

    /// Gate view of one request: operation kind, the card it touches
    /// (0 = none) and the context that owns it (0 = none).
    fn classify(&self, msg: &SmartcardMessage) -> (GateOp, u64, u64) {
        let card_ctx = |card: u32| self.cards.get(&card).map_or(0, |info| u64::from(info.context));
        match msg {
            SmartcardMessage::ReleaseContext { context, .. } => {
                (GateOp::ReleaseContext, 0, u64::from(*context))
            }
            SmartcardMessage::Disconnect { card, .. } => {
                (GateOp::Disconnect, u64::from(*card), card_ctx(*card))
            }
            SmartcardMessage::Reconnect { card, .. } => {
                (GateOp::Reconnect, u64::from(*card), card_ctx(*card))
            }
            SmartcardMessage::Transmit { card, .. } => {
                (GateOp::Transmit, u64::from(*card), card_ctx(*card))
            }
            SmartcardMessage::Control { card, .. } => {
                (GateOp::Control, u64::from(*card), card_ctx(*card))
            }
            SmartcardMessage::BeginTransaction { card, .. } => {
                (GateOp::Begin, u64::from(*card), card_ctx(*card))
            }
            SmartcardMessage::EndTransaction { card, .. } => {
                (GateOp::End, u64::from(*card), card_ctx(*card))
            }
            _ => (GateOp::Other, 0, 0),
        }
    }

    /// Rebuilds the backend handle of a wire card id: zero-extended raw
    /// id plus the protocol remembered from `Connected`/`Reconnected`
    /// (unknown cards get protocol 0 and fail in the backend).
    fn scard_handle(&self, card: u32) -> ScardHandle {
        let protocol = self.cards.get(&card).map_or(0, |info| info.protocol);
        ScardHandle::from_raw(u64::from(card), protocol)
    }

    /// Sends one reply; the session being gone is fine (returns the code
    /// for the gate bookkeeping).
    fn reply(&self, msg: SmartcardMessage) -> u32 {
        let _ = self.sink.send(msg);
        SCARD_S_SUCCESS
    }

    /// Answers one request with a SCARD error code.
    fn reply_error(&self, req: &SmartcardMessage, code: u32) -> u32 {
        let _ = self.sink.send(SmartcardMessage::Error {
            request_id: req.request_id(),
            code,
        });
        code
    }

    /// Executes one request against the backend, sends its reply and
    /// returns the SCARD result code (feeds [`TransactionGate::on_completed`]).
    #[expect(clippy::too_many_lines, reason = "one arm per request variant")]
    fn execute(&mut self, msg: &SmartcardMessage) -> u32 {
        let Some(backend) = self.backend.clone() else {
            return self.reply_error(msg, SCARD_E_NO_SERVICE);
        };
        let request_id = msg.request_id();
        match msg {
            SmartcardMessage::EstablishContext { scope, .. } => {
                match backend.establish_context(*scope) {
                    Ok(ctx) => {
                        self.live_contexts
                            .lock()
                            .expect("live contexts poisoned")
                            .insert(ctx.raw());
                        self.reply(SmartcardMessage::ContextEstablished {
                            request_id,
                            context: wire_id(ctx.raw()),
                        })
                    }
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::ReleaseContext { context, .. } => {
                let ctx = ScardContext::from_raw(u64::from(*context));
                match backend.release_context(&ctx) {
                    Ok(()) => {
                        self.live_contexts
                            .lock()
                            .expect("live contexts poisoned")
                            .remove(&ctx.raw());
                        self.cards.retain(|_, info| info.context != *context);
                        self.reply(SmartcardMessage::ContextReleased { request_id })
                    }
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::ListReaders { context, .. } => {
                let ctx = ScardContext::from_raw(u64::from(*context));
                match backend.list_readers(&ctx, None) {
                    Ok(readers) => self.reply(SmartcardMessage::Readers { request_id, readers }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Cancel { context, .. } => {
                let ctx = ScardContext::from_raw(u64::from(*context));
                match backend.cancel(&ctx) {
                    Ok(()) => self.reply(SmartcardMessage::Cancelled { request_id }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Connect {
                context,
                reader,
                share_mode,
                preferred_protocols,
                ..
            } => {
                let ctx = ScardContext::from_raw(u64::from(*context));
                match backend.connect(&ctx, reader, *share_mode, *preferred_protocols) {
                    Ok(result) => {
                        let card = wire_id(result.handle.raw());
                        self.cards.insert(
                            card,
                            CardInfo {
                                protocol: result.active_protocol,
                                context: *context,
                            },
                        );
                        self.reply(SmartcardMessage::Connected {
                            request_id,
                            card,
                            active_protocol: result.active_protocol,
                        })
                    }
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Disconnect {
                card, disposition, ..
            } => {
                let handle = self.scard_handle(*card);
                match backend.disconnect(&handle, *disposition) {
                    Ok(()) => {
                        self.cards.remove(card);
                        self.reply(SmartcardMessage::Disconnected { request_id })
                    }
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Reconnect {
                card,
                share_mode,
                preferred_protocols,
                initialization,
                ..
            } => {
                let handle = self.scard_handle(*card);
                match backend.reconnect(&handle, *share_mode, *preferred_protocols, *initialization)
                {
                    Ok(active_protocol) => {
                        if let Some(info) = self.cards.get_mut(card) {
                            info.protocol = active_protocol;
                        }
                        self.reply(SmartcardMessage::Reconnected {
                            request_id,
                            active_protocol,
                        })
                    }
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Status { card, .. } => {
                let handle = self.scard_handle(*card);
                match backend.status(&handle) {
                    Ok(status) => self.reply(SmartcardMessage::StatusReply {
                        request_id,
                        reader: status.reader_names.first().cloned().unwrap_or_default(),
                        state: status.state,
                        protocol: status.protocol,
                        atr: status.atr,
                    }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Transmit {
                card,
                send_protocol,
                data,
                ..
            } => {
                let handle = self.scard_handle(*card);
                let pci = ScardIORequest {
                    protocol: *send_protocol,
                    extra_bytes: Vec::new(),
                };
                match backend.transmit(&handle, &pci, data) {
                    Ok(result) => self.reply(SmartcardMessage::TransmitReply {
                        request_id,
                        recv_protocol: result
                            .recv_pci
                            .map_or(*send_protocol, |pci| pci.protocol),
                        data: result.recv_buffer,
                    }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::Control { card, code, data, .. } => {
                let handle = self.scard_handle(*card);
                match backend.control(&handle, *code, data) {
                    Ok(data) => self.reply(SmartcardMessage::ControlReply { request_id, data }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::BeginTransaction { card, .. } => {
                let handle = self.scard_handle(*card);
                match backend.begin_transaction(&handle) {
                    Ok(()) => self.reply(SmartcardMessage::TransactionBegun { request_id }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::EndTransaction {
                card, disposition, ..
            } => {
                let handle = self.scard_handle(*card);
                match backend.end_transaction(&handle, *disposition) {
                    Ok(()) => self.reply(SmartcardMessage::TransactionEnded { request_id }),
                    Err(code) => self.reply_error(msg, code),
                }
            }
            SmartcardMessage::GetStatusChange { .. } => {
                // Intercepted by `handle` (dedicated watcher thread).
                self.reply_error(msg, SCARD_E_INVALID_HANDLE)
            }
            reply => {
                warn!(?reply, "smartcard reply where a request was expected; ignored");
                SCARD_S_SUCCESS
            }
        }
    }

    /// Runs one `GetStatusChange` watch on its own thread (it blocks
    /// inside the backend up to `timeout_ms`). States travel even on
    /// `SCARD_E_TIMEOUT` — an empty body makes the remote SCardSvr
    /// re-enumerate in a flood (RDPESC lesson, §17.3).
    fn spawn_watch(
        &mut self,
        request_id: u32,
        context: u32,
        timeout_ms: u32,
        states: &[ReaderState],
    ) {
        let Some(backend) = self.backend.clone() else {
            let _ = self.sink.send(SmartcardMessage::Error {
                request_id,
                code: SCARD_E_NO_SERVICE,
            });
            return;
        };
        let raw = u64::from(context);
        if !self
            .live_contexts
            .lock()
            .expect("live contexts poisoned")
            .contains(&raw)
        {
            let _ = self.sink.send(SmartcardMessage::Error {
                request_id,
                code: SCARD_E_INVALID_HANDLE,
            });
            return;
        }
        let ctx = ScardContext::from_raw(raw);
        let states_in: Vec<ReaderStateIn> = states
            .iter()
            .map(|s| ReaderStateIn {
                reader_name: s.reader.clone(),
                current_state: s.current_state,
            })
            .collect();
        let timeout = Duration::from_millis(u64::from(timeout_ms));
        let sink = self.sink.clone();
        let spawned = std::thread::Builder::new()
            .name("scard-watch".into())
            .spawn(move || {
                let reply = match backend.get_status_change(&ctx, timeout, &states_in) {
                    Ok((outs, code)) if code == SCARD_S_SUCCESS || code == SCARD_E_TIMEOUT => {
                        SmartcardMessage::StatusChange {
                            request_id,
                            states: outs
                                .iter()
                                .map(|o| ReaderState {
                                    reader: o.reader_name.clone(),
                                    current_state: o.current_state,
                                    event_state: o.event_state,
                                    atr: o.atr.clone(),
                                })
                                .collect(),
                        }
                    }
                    Ok((_, code)) | Err(code) => SmartcardMessage::Error { request_id, code },
                };
                let _ = sink.send(reply);
            });
        match spawned {
            Ok(handle) => self
                .watchers
                .lock()
                .expect("watchers poisoned")
                .push(handle),
            Err(e) => {
                error!("cannot spawn the smartcard watch thread: {e}");
                let _ = self.sink.send(SmartcardMessage::Error {
                    request_id,
                    code: SCARD_F_INTERNAL_ERROR,
                });
            }
        }
    }
}

/// Builds the platform backend for a session: the real PC/SC stack with
/// the `smartcard` feature, `None` (→ `SCARD_E_NO_SERVICE` replies)
/// otherwise or when the subsystem is unavailable.
#[cfg(feature = "smartcard")]
pub fn default_backend() -> Option<Arc<dyn SmartcardIntegration>> {
    let backend = PcscBackend::new();
    if backend.is_available() {
        Some(Arc::new(backend))
    } else {
        warn!("PC/SC subsystem unavailable; smartcard requests will fail with SCARD_E_NO_SERVICE");
        None
    }
}

/// Without the `smartcard` feature there is no backend: requests are
/// answered with `SCARD_E_NO_SERVICE`.
#[cfg(not(feature = "smartcard"))]
pub fn default_backend() -> Option<Arc<dyn SmartcardIntegration>> {
    None
}
