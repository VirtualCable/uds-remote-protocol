// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Card transactions across handles.
//!
//! The remote side opens the same card from several handles and wraps each
//! multi-APDU exchange in BEGIN/END_TRANSACTION. Requests from other
//! handles are parked until the transaction ends, so their APDUs never
//! interleave.
//!
//! A transaction left idle for more than five seconds is dropped, the same
//! limit WinSCard applies ("if a transaction is held on the card for more
//! than five seconds with no operations happening on that card, then the
//! card is reset"). Without it, a caller that gives up while its BEGIN is
//! parked keeps the card forever and every other handle waits behind it.
//! Only the gate is released: the card is not reset, since the transaction
//! exists here and not in the local PC/SC layer.
//!
//! Ported from freerdp-integration's `addins/smartcard/transactions.rs`,
//! with RDPESC IOCTL codes replaced by [`GateOp`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::consts::{SCARD_E_INVALID_HANDLE, SCARD_S_SUCCESS};

/// Idle limit after which a held transaction is dropped (WinSCard limit).
pub const TRANSACTION_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Channel operation as seen by the gate. Anything not listed explicitly
/// is [`GateOp::Other`] and never waits (status queries, watches, cancels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOp {
    /// `BeginTransaction`.
    Begin,
    /// `EndTransaction`.
    End,
    /// `Transmit` (APDU exchange).
    Transmit,
    /// `Control`.
    Control,
    /// `Reconnect`.
    Reconnect,
    /// `Disconnect`.
    Disconnect,
    /// `ReleaseContext`.
    ReleaseContext,
    /// Any other operation: never parked.
    Other,
}

impl GateOp {
    /// Whether the operation touches the card and must serialize against
    /// foreign transactions.
    const fn needs_card_exclusively(self) -> bool {
        matches!(
            self,
            Self::Begin | Self::Transmit | Self::Control | Self::Reconnect
        )
    }
}

fn handle_is_gone(result: u32) -> bool {
    result == SCARD_S_SUCCESS || result == SCARD_E_INVALID_HANDLE
}

#[derive(Debug)]
struct Owner {
    card: u64,
    context: u64,
    depth: u32,
    last_activity: Instant,
}

#[derive(Debug)]
struct Parked<T> {
    op: GateOp,
    card: u64,
    context: u64,
    item: T,
}

/// Per-card mutual exclusion between handles: parks operations that would
/// interleave with an in-progress foreign transaction.
#[derive(Debug)]
pub struct TransactionGate<T> {
    owner: Option<Owner>,
    parked: VecDeque<Parked<T>>,
}

impl<T> Default for TransactionGate<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TransactionGate<T> {
    /// Create an empty gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            owner: None,
            parked: VecDeque::new(),
        }
    }

    /// Card currently owning a transaction, if any.
    #[must_use]
    pub fn owner_card(&self) -> Option<u64> {
        self.owner.as_ref().map(|o| o.card)
    }

    /// Whether an operation on `card` must wait for a foreign transaction.
    #[must_use]
    pub fn must_wait(&self, op: GateOp, card: u64) -> bool {
        op.needs_card_exclusively() && self.owner.as_ref().is_some_and(|o| o.card != card)
    }

    /// Park an operation for later (FIFO).
    pub fn park(&mut self, op: GateOp, card: u64, context: u64, item: T) {
        self.parked.push_back(Parked {
            op,
            card,
            context,
            item,
        });
    }

    /// Oldest parked operation that can run now, if any.
    pub fn next_runnable(&mut self) -> Option<(GateOp, u64, u64, T)> {
        let index = self.parked.iter().position(|p| !self.must_wait(p.op, p.card))?;
        self.parked
            .remove(index)
            .map(|p| (p.op, p.card, p.context, p.item))
    }

    /// Update the owner after an operation has run. `result` is the SCARD
    /// return code of the operation.
    pub fn on_completed(&mut self, op: GateOp, card: u64, context: u64, result: u32, now: Instant) {
        if let Some(owner) = &mut self.owner
            && owner.card == card
        {
            owner.last_activity = now;
        }
        match op {
            GateOp::Begin if result == SCARD_S_SUCCESS => match &mut self.owner {
                Some(owner) if owner.card == card => owner.depth += 1,
                Some(_) => {}
                None => {
                    self.owner = Some(Owner {
                        card,
                        context,
                        depth: 1,
                        last_activity: now,
                    });
                }
            },
            GateOp::End if self.owner_card() == Some(card) => {
                if result == SCARD_E_INVALID_HANDLE {
                    // The handle no longer exists, so nobody can end its
                    // transaction.
                    self.owner = None;
                } else if result == SCARD_S_SUCCESS
                    && let Some(owner) = &mut self.owner
                {
                    owner.depth -= 1;
                    if owner.depth == 0 {
                        self.owner = None;
                    }
                }
            }
            GateOp::Disconnect if self.owner_card() == Some(card) && handle_is_gone(result) => {
                self.owner = None;
            }
            GateOp::ReleaseContext
                if result == SCARD_S_SUCCESS
                    && self.owner.as_ref().is_some_and(|o| o.context == context) =>
            {
                self.owner = None;
            }
            _ => {}
        }
    }

    /// Drop the owner's transaction if it has been idle longer than
    /// [`TRANSACTION_IDLE_TIMEOUT`]. Returns the card whose transaction
    /// expired.
    pub fn expire_idle(&mut self, now: Instant) -> Option<u64> {
        let owner = self.owner.as_ref()?;
        if now.saturating_duration_since(owner.last_activity) <= TRANSACTION_IDLE_TIMEOUT {
            return None;
        }
        let card = owner.card;
        self.owner = None;
        Some(card)
    }

    /// How long until the owner's transaction expires, if there is one.
    #[must_use]
    pub fn time_to_expiry(&self, now: Instant) -> Option<Duration> {
        let owner = self.owner.as_ref()?;
        Some(
            (owner.last_activity + TRANSACTION_IDLE_TIMEOUT)
                .saturating_duration_since(now)
                .max(Duration::from_millis(10)),
        )
    }

    /// Remove the parked operations of a released context.
    pub fn take_context(&mut self, context: u64) -> Vec<T> {
        let (taken, kept): (VecDeque<_>, VecDeque<_>) =
            self.parked.drain(..).partition(|p| p.context == context);
        self.parked = kept;
        taken.into_iter().map(|p| p.item).collect()
    }

    /// Remove every parked operation (teardown).
    pub fn take_all(&mut self) -> Vec<T> {
        self.parked.drain(..).map(|p| p.item).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    const OK: u32 = SCARD_S_SUCCESS;
    const CARD_A: u64 = 0xA;
    const CARD_B: u64 = 0xB;
    const CTX_A: u64 = 0x1;
    const CTX_B: u64 = 0x2;

    fn begin(gate: &mut TransactionGate<u32>, card: u64, context: u64) {
        assert!(!gate.must_wait(GateOp::Begin, card));
        gate.on_completed(GateOp::Begin, card, context, OK, now());
    }

    #[test]
    fn other_handle_waits_until_end_transaction() {
        let mut gate = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);

        assert!(gate.must_wait(GateOp::Begin, CARD_B));
        assert!(gate.must_wait(GateOp::Transmit, CARD_B));
        assert!(gate.must_wait(GateOp::Control, CARD_B));
        assert!(gate.must_wait(GateOp::Reconnect, CARD_B));
        assert!(!gate.must_wait(GateOp::Transmit, CARD_A));
        gate.park(GateOp::Begin, CARD_B, CTX_B, 1);
        assert!(gate.next_runnable().is_none());

        gate.on_completed(GateOp::End, CARD_A, CTX_A, OK, now());

        let (op, card, _, item) = gate.next_runnable().unwrap();
        assert_eq!((op, card, item), (GateOp::Begin, CARD_B, 1));
    }

    #[test]
    fn nested_begin_needs_matching_ends() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        begin(&mut gate, CARD_A, CTX_A);

        gate.on_completed(GateOp::End, CARD_A, CTX_A, OK, now());
        assert_eq!(gate.owner_card(), Some(CARD_A));
        gate.on_completed(GateOp::End, CARD_A, CTX_A, OK, now());
        assert_eq!(gate.owner_card(), None);
    }

    #[test]
    fn end_from_another_handle_keeps_owner() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.on_completed(GateOp::End, CARD_B, CTX_B, OK, now());
        assert_eq!(gate.owner_card(), Some(CARD_A));
    }

    #[test]
    fn failed_end_keeps_owner() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.on_completed(GateOp::End, CARD_A, CTX_A, 0x8010_0001, now());
        assert_eq!(gate.owner_card(), Some(CARD_A));
    }

    #[test]
    fn end_on_invalid_handle_releases() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        begin(&mut gate, CARD_A, CTX_A);
        gate.on_completed(GateOp::End, CARD_A, CTX_A, SCARD_E_INVALID_HANDLE, now());
        assert_eq!(gate.owner_card(), None);
    }

    #[test]
    fn disconnect_of_owner_releases() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.on_completed(GateOp::Disconnect, CARD_A, CTX_A, OK, now());
        assert_eq!(gate.owner_card(), None);
    }

    #[test]
    fn release_context_of_owner_releases() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.on_completed(GateOp::ReleaseContext, 0, CTX_B, OK, now());
        assert_eq!(gate.owner_card(), Some(CARD_A));
        gate.on_completed(GateOp::ReleaseContext, 0, CTX_A, 0x8010_0001, now());
        assert_eq!(gate.owner_card(), Some(CARD_A));
        gate.on_completed(GateOp::ReleaseContext, 0, CTX_A, OK, now());
        assert_eq!(gate.owner_card(), None);
    }

    #[test]
    fn failed_disconnect_keeps_owner() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.on_completed(GateOp::Disconnect, CARD_A, CTX_A, 0x8010_0001, now());
        assert_eq!(gate.owner_card(), Some(CARD_A));
        gate.on_completed(GateOp::Disconnect, CARD_A, CTX_A, SCARD_E_INVALID_HANDLE, now());
        assert_eq!(gate.owner_card(), None);
    }

    #[test]
    fn failed_begin_does_not_take_the_card() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        gate.on_completed(GateOp::Begin, CARD_A, CTX_A, 0x8010_0002, now());
        assert_eq!(gate.owner_card(), None);
    }

    #[test]
    fn status_requests_never_wait() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        assert!(!gate.must_wait(GateOp::Other, CARD_B));
        assert!(!gate.must_wait(GateOp::End, CARD_B));
        assert!(!gate.must_wait(GateOp::Disconnect, CARD_B));
    }

    #[test]
    fn parked_requests_keep_arrival_order() {
        let mut gate = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.park(GateOp::Begin, CARD_B, CTX_B, 1);
        gate.park(GateOp::Begin, 0xC, 0x3, 2);
        gate.on_completed(GateOp::End, CARD_A, CTX_A, OK, now());

        let (op, card, context, item) = gate.next_runnable().unwrap();
        assert_eq!(item, 1);
        gate.on_completed(op, card, context, OK, now());
        assert!(gate.next_runnable().is_none());

        gate.on_completed(GateOp::End, CARD_B, CTX_B, OK, now());
        assert_eq!(gate.next_runnable().map(|r| r.3), Some(2));
    }

    #[test]
    fn take_context_removes_only_that_context() {
        let mut gate = TransactionGate::new();
        begin(&mut gate, CARD_A, CTX_A);
        gate.park(GateOp::Transmit, CARD_B, CTX_B, 1);
        gate.park(GateOp::Begin, 0xC, 0x3, 2);
        gate.park(GateOp::Begin, CARD_B, CTX_B, 3);

        assert_eq!(gate.take_context(CTX_B), vec![1, 3]);
        assert_eq!(gate.take_all(), vec![2]);
    }

    #[test]
    fn idle_transaction_expires() {
        let mut gate = TransactionGate::new();
        let start = Instant::now();
        gate.on_completed(GateOp::Begin, CARD_A, CTX_A, OK, start);
        gate.park(GateOp::Begin, CARD_B, CTX_B, 1);

        assert_eq!(gate.expire_idle(start + TRANSACTION_IDLE_TIMEOUT), None);
        assert_eq!(gate.owner_card(), Some(CARD_A));

        let later = start + TRANSACTION_IDLE_TIMEOUT + Duration::from_millis(1);
        assert_eq!(gate.expire_idle(later), Some(CARD_A));
        assert_eq!(gate.owner_card(), None);
        assert_eq!(gate.next_runnable().map(|r| r.3), Some(1));
    }

    #[test]
    fn owner_activity_postpones_expiry() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        let start = Instant::now();
        gate.on_completed(GateOp::Begin, CARD_A, CTX_A, OK, start);
        let busy = start + Duration::from_secs(4);
        gate.on_completed(GateOp::Transmit, CARD_A, CTX_A, OK, busy);

        assert_eq!(gate.expire_idle(start + Duration::from_secs(6)), None);
        assert_eq!(
            gate.time_to_expiry(start + Duration::from_secs(6)),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            gate.expire_idle(busy + TRANSACTION_IDLE_TIMEOUT + Duration::from_millis(1)),
            Some(CARD_A)
        );
    }

    #[test]
    fn activity_of_other_handles_does_not_postpone_expiry() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        let start = Instant::now();
        gate.on_completed(GateOp::Begin, CARD_A, CTX_A, OK, start);
        gate.on_completed(GateOp::Other, CARD_B, CTX_B, OK, start + Duration::from_secs(4));
        assert_eq!(
            gate.expire_idle(start + TRANSACTION_IDLE_TIMEOUT + Duration::from_millis(1)),
            Some(CARD_A)
        );
    }

    #[test]
    fn no_owner_means_no_expiry() {
        let mut gate: TransactionGate<u32> = TransactionGate::new();
        assert_eq!(gate.time_to_expiry(Instant::now()), None);
        assert_eq!(gate.expire_idle(Instant::now()), None);
    }
}
