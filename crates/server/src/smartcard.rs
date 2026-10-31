//! Smartcard channel, server side (§17): the request-origin plumbing.
//!
//! The real origin of smartcard requests will be the virtual smart card
//! reader driver (UMDF; TODO doc/plan §17 — plug it in here when it
//! exists). Until then [`NullSmartcardRelay`] keeps the channel inert.
//!
//! The pieces:
//!
//! - [`SmartcardRouter`]: session-side plumbing owned by the session
//!   loop. Stamps outgoing requests with a fresh `request_id`, keeps the
//!   `request_id → oneshot` registry of pending calls, receives the
//!   encoded requests through an unbounded channel the session loop
//!   drains onto the reliable stream, and routes incoming
//!   [`SmartcardMessage`] replies to the waiting call. Requests arriving
//!   FROM the client (there should be none — requests are S→C) are
//!   logged and dropped, as are replies with an unknown `request_id`.
//!
//! - [`SmartcardRelay`]: the trait an origin talks to. [`RouterRelay`]
//!   implements it over a [`SmartcardRouter`]; [`NullSmartcardRelay`]
//!   fails every call with [`RelayError::Unavailable`].
//!
//! Timeouts are per operation: a `GetStatusChange` may legitimately park
//! for its whole client-side timeout, so it gets that timeout plus a
//! margin; everything else shares a default.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::SmartcardMessage;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

/// Default reply timeout for regular operations. Generous on purpose:
/// real cards (the eDNI lesson, §17.9) can take tens of seconds on a
/// single Transmit; a tight timeout is worse than a slow reply.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Extra wait granted to `GetStatusChange` on top of its own timeout
/// (the client parks inside the backend for the full requested time).
pub const GSC_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);

/// Failure of one relayed smartcard call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    /// No request origin exists yet (the virtual reader driver is not
    /// built, §17): the channel is inert.
    #[error("smartcard: no request origin (virtual reader driver not built, §17)")]
    Unavailable,
    /// The client did not answer within the operation's timeout.
    #[error("smartcard: request timed out")]
    Timeout,
    /// The session (or its smartcard plumbing) is gone.
    #[error("smartcard: channel disconnected")]
    Disconnected,
}

/// Origin-facing smartcard request path: send one request on
/// `channel::SMARTCARD` and resolve its reply.
#[async_trait::async_trait]
pub trait SmartcardRelay: Send + Sync + std::fmt::Debug {
    /// Sends `request` (its `request_id` is replaced with a fresh one)
    /// and resolves with the client's reply — which may be
    /// [`SmartcardMessage::Error`] carrying a raw SCARD code.
    async fn call(&self, request: SmartcardMessage) -> Result<SmartcardMessage, RelayError>;
}

/// Session-side smartcard plumbing, see the module docs. Share it in an
/// [`Arc`] between the session loop (routing replies, draining the
/// outbound queue) and the request origin (a [`RouterRelay`]).
#[derive(Debug)]
pub struct SmartcardRouter {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<SmartcardMessage>>>,
    out: mpsc::UnboundedSender<SmartcardMessage>,
    default_timeout: Duration,
    gsc_margin: Duration,
}

impl SmartcardRouter {
    /// Router with the default timeouts; requests leave through `out`.
    #[must_use]
    pub fn new(out: mpsc::UnboundedSender<SmartcardMessage>) -> Self {
        Self::with_timeouts(out, DEFAULT_REQUEST_TIMEOUT, GSC_TIMEOUT_MARGIN)
    }

    /// Router with explicit timeouts (tests use tiny ones).
    #[must_use]
    pub fn with_timeouts(
        out: mpsc::UnboundedSender<SmartcardMessage>,
        default_timeout: Duration,
        gsc_margin: Duration,
    ) -> Self {
        Self {
            next_id: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            out,
            default_timeout,
            gsc_margin,
        }
    }

    /// Registers a pending call; returns its freshly assigned id and the
    /// receiver its reply resolves.
    fn register(&self) -> (u32, oneshot::Receiver<SmartcardMessage>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().expect("pending poisoned").insert(id, tx);
        (id, rx)
    }

    /// Drops a pending call without a reply (timeout path).
    fn unregister(&self, id: u32) {
        self.pending.lock().expect("pending poisoned").remove(&id);
    }

    /// How long a call for `request` may wait for its reply.
    fn timeout_for(&self, request: &SmartcardMessage) -> Duration {
        match request {
            SmartcardMessage::GetStatusChange { timeout_ms, .. } => {
                Duration::from_millis(u64::from(*timeout_ms)) + self.gsc_margin
            }
            _ => self.default_timeout,
        }
    }

    /// Decodes and routes one packet received on `channel::SMARTCARD`.
    pub fn handle_packet(&self, packet: &[u8]) {
        match SmartcardMessage::decode(packet) {
            Ok(msg) => self.route(msg),
            Err(e) => warn!("bad smartcard message: {e}"),
        }
    }

    /// Routes one decoded message: replies resolve their pending call
    /// (unknown ids are dropped), requests from the client are a
    /// protocol violation — logged and dropped, never fatal.
    pub fn route(&self, msg: SmartcardMessage) {
        if msg.is_request() {
            warn!(?msg, "unexpected smartcard request from the client; dropped");
            return;
        }
        let id = msg.request_id();
        let waiter = self.pending.lock().expect("pending poisoned").remove(&id);
        if let Some(tx) = waiter {
            let _ = tx.send(msg);
        } else {
            warn!(request_id = id, "smartcard reply with unknown request id; dropped");
        }
    }
}

/// Functional [`SmartcardRelay`] over a session's [`SmartcardRouter`]:
/// this is the handle the virtual reader driver will get (§17).
#[derive(Debug, Clone)]
pub struct RouterRelay {
    router: Arc<SmartcardRouter>,
}

impl RouterRelay {
    /// Wraps a router.
    #[must_use]
    pub fn new(router: Arc<SmartcardRouter>) -> Self {
        Self { router }
    }
}

#[async_trait::async_trait]
impl SmartcardRelay for RouterRelay {
    async fn call(&self, request: SmartcardMessage) -> Result<SmartcardMessage, RelayError> {
        let (id, rx) = self.router.register();
        let timeout = self.router.timeout_for(&request);
        if self.router.out.send(request.with_request_id(id)).is_err() {
            self.router.unregister(id);
            return Err(RelayError::Disconnected);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(RelayError::Disconnected),
            Err(_) => {
                self.router.unregister(id);
                Err(RelayError::Timeout)
            }
        }
    }
}

/// Stub origin for phase 1: every call fails, the channel stays inert
/// until the virtual reader driver exists (§17).
#[derive(Debug, Default)]
pub struct NullSmartcardRelay;

#[async_trait::async_trait]
impl SmartcardRelay for NullSmartcardRelay {
    async fn call(&self, _request: SmartcardMessage) -> Result<SmartcardMessage, RelayError> {
        Err(RelayError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SCOPE_USER;

    /// `SCARD_S_SUCCESS` (the server side speaks raw codes; the smartcard
    /// crate is a client-side dependency).
    const SCARD_S_SUCCESS: u32 = 0;

    fn test_router() -> (
        Arc<SmartcardRouter>,
        mpsc::UnboundedReceiver<SmartcardMessage>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(SmartcardRouter::with_timeouts(
                tx,
                Duration::from_millis(100),
                Duration::from_millis(50),
            )),
            rx,
        )
    }

    #[tokio::test]
    async fn reply_routes_to_the_waiting_call() {
        let (router, mut out_rx) = test_router();
        let relay = RouterRelay::new(Arc::clone(&router));
        let call = tokio::spawn(async move {
            relay
                .call(SmartcardMessage::EstablishContext {
                    request_id: 0,
                    scope: SCOPE_USER,
                })
                .await
        });

        // The request left stamped with the registry's id.
        let sent = out_rx.recv().await.expect("request queued");
        let id = sent.request_id();
        assert_eq!(
            sent,
            SmartcardMessage::EstablishContext {
                request_id: id,
                scope: SCOPE_USER,
            }
        );
        assert_ne!(id, 0);

        router.route(SmartcardMessage::ContextEstablished {
            request_id: id,
            context: 7,
        });
        let reply = call.await.expect("call task").expect("reply");
        assert_eq!(
            reply,
            SmartcardMessage::ContextEstablished {
                request_id: id,
                context: 7,
            }
        );
        assert!(router.pending.lock().expect("pending").is_empty());
    }

    #[tokio::test]
    async fn timeout_expires_and_cleans_the_registry() {
        let (router, _out_rx) = test_router();
        let relay = RouterRelay::new(Arc::clone(&router));
        let result = relay
            .call(SmartcardMessage::ListReaders {
                request_id: 0,
                context: 1,
            })
            .await;
        assert_eq!(result, Err(RelayError::Timeout));
        // The expired call is gone from the registry: a late reply would
        // hit the unknown-id path instead of leaking the entry.
        assert!(router.pending.lock().expect("pending").is_empty());
    }

    #[tokio::test]
    async fn unknown_request_id_is_dropped() {
        let (router, mut out_rx) = test_router();
        // Nobody waits on id 999: dropped without disturbing the pending
        // calls.
        router.route(SmartcardMessage::Error {
            request_id: 999,
            code: SCARD_S_SUCCESS,
        });
        let relay = RouterRelay::new(Arc::clone(&router));
        let call = tokio::spawn(async move {
            relay
                .call(SmartcardMessage::Status {
                    request_id: 0,
                    card: 5,
                })
                .await
        });
        let sent = out_rx.recv().await.expect("request queued");
        assert_eq!(router.pending.lock().expect("pending").len(), 1);
        // Late reply for an unrelated id does not resolve the pending one.
        router.route(SmartcardMessage::Disconnected { request_id: 999 });
        router.route(SmartcardMessage::Disconnected {
            request_id: sent.request_id(),
        });
        assert_eq!(
            call.await.expect("call task"),
            Ok(SmartcardMessage::Disconnected {
                request_id: sent.request_id(),
            })
        );
    }

    #[tokio::test]
    async fn requests_from_the_client_are_dropped() {
        let (router, _out_rx) = test_router();
        // Requests are S→C: receiving one is a protocol violation, logged
        // and dropped (nothing is registered, nothing panics).
        router.route(SmartcardMessage::EstablishContext {
            request_id: 1,
            scope: SCOPE_USER,
        });
        assert!(router.pending.lock().expect("pending").is_empty());
    }

    #[tokio::test]
    async fn gsc_timeout_gets_the_watch_margin() {
        let (router, _out_rx) = test_router();
        let gsc = SmartcardMessage::GetStatusChange {
            request_id: 0,
            context: 1,
            timeout_ms: 2_000,
            states: Vec::new(),
        };
        assert_eq!(
            router.timeout_for(&gsc),
            Duration::from_millis(2_000) + Duration::from_millis(50)
        );
        let other = SmartcardMessage::Cancel {
            request_id: 0,
            context: 1,
        };
        assert_eq!(router.timeout_for(&other), Duration::from_millis(100));
    }

    #[tokio::test]
    async fn null_relay_is_inert() {
        let relay = NullSmartcardRelay;
        let result = relay
            .call(SmartcardMessage::Status {
                request_id: 0,
                card: 1,
            })
            .await;
        assert_eq!(result, Err(RelayError::Unavailable));
    }
}
