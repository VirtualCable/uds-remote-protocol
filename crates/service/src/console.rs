// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Console-session queries and actions (§18.53/§18.55).
//!
//! This module is deliberately passive: it knows HOW to read the console
//! state and HOW to perform the primitive actions (evict, notify the
//! client), but not WHEN. All orchestration — conflict resolution, eviction
//! settling, autologon waits, auto-unlock — lives in the console state
//! machine (`crate::console_fsm`), the single owner of console side effects.
//!
//! Account comparison (§18.53): user name case-insensitive, domain
//! normalized — `""`, `"."` and the machine name all mean "local account".
//! `DOMAIN\alice` and `.\alice` are DIFFERENT accounts and must conflict.

use anyhow::{Context, Result};
use tracing::debug;
use transport::{EncryptedStream, ReliableTransport};
use udscredprov::logic::Credentials;
use windows::Win32::System::RemoteDesktop::{
    WTSDisconnectSession, WTSDomainName, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSLogoffSession, WTSQuerySessionInformationW, WTSUserName,
};

use protocol::ControlMessage;
use protocol::channel;

/// How long the state machine waits for an evicted console to actually
/// flip before giving up (logoff/disconnect are asynchronous on the WTS
/// side).
pub(crate) const EVICT_SETTLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Server-side conflict configuration, resolved from the standalone config
/// or the broker ticket payload.
pub struct ConflictConfig {
    /// What to do when the console owner differs from the session user.
    pub policy: server_config::ConsoleConflictPolicy,
    /// Whether the `ask` message may carry the console owner's name.
    pub reveal_user: bool,
    /// Ask timeout; without an explicit answer the connection is denied.
    pub timeout: std::time::Duration,
    /// Disconnect (keep apps) or logoff (kill apps, default) on takeover.
    pub takeover: server_config::TakeoverMode,
}

/// Full account identity of a session (§18.53): user name plus domain as
/// reported by `WTSUserName`/`WTSDomainName` (machine name for local
/// accounts, domain name for domain accounts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// User name (no domain).
    pub user: String,
    /// Domain (or machine name for local accounts).
    pub domain: String,
}

impl Account {
    /// `DOMAIN\user` display form.
    pub fn display(&self) -> String {
        format!("{}\\{}", self.domain, self.user)
    }
}

/// Service-side view of the interactive console (§18.55 state machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleState {
    /// No console session at all (`0xFFFF_FFFF`, a clean boot).
    NoSession,
    /// A console session id exists but has NO logged-on user: the logon
    /// screen, including the winlogon session left behind by a logoff or a
    /// console disconnect (§18.53 lesson of 2026-11-04).
    LogonScreen,
    /// A user owns the console. `locked` (secure attention / lock screen)
    /// is a PROPERTY of the session, not a state of its own.
    Session {
        /// WTS session id.
        id: u32,
        /// Owning account.
        account: Account,
        /// True while the session shows the lock screen.
        locked: bool,
    },
}

/// Queries one null-terminated UTF-16 string field of a WTS session.
fn query_session_string(session_id: u32, class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS) -> Option<String> {
    let mut buf = windows::core::PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: valid out-params; on success the buffer is freed below.
    let queried = unsafe {
        WTSQuerySessionInformationW(None, session_id, class, &raw mut buf, &raw mut bytes)
    };
    if queried.is_err() || buf.is_null() {
        return None;
    }
    // SAFETY: on success the buffer holds a null-terminated UTF-16 string.
    let value = unsafe { buf.to_string().unwrap_or_default() };
    unsafe { WTSFreeMemory(buf.0.cast()) };
    Some(value)
}

/// Snapshot of the interactive console. Cheap WTS queries; the state
/// machine's poller calls this on its cadence (500 ms).
pub(crate) fn console_state() -> ConsoleState {
    // SAFETY: plain query, no out-params.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == 0xFFFF_FFFF {
        return ConsoleState::NoSession;
    }
    let user = query_session_string(id, WTSUserName);
    if user.as_deref().unwrap_or_default().is_empty() {
        // Normal while the console sits at the logon/secure-attention
        // screen; callers poll this on a timer, so keep it out of the log.
        debug!(session_id = id, "console session without a queryable user");
        return ConsoleState::LogonScreen;
    }
    ConsoleState::Session {
        id,
        account: Account {
            user: user.unwrap_or_default(),
            domain: query_session_string(id, WTSDomainName).unwrap_or_default(),
        },
        locked: crate::agent::console_session_locked(id),
    }
}

/// Account comparison (§18.53): user name case-insensitive, domain
/// normalized — `""`, `"."` and the machine name all mean "local account".
pub(crate) fn same_account(owner: &Account, creds: &Credentials) -> bool {
    if !creds.username.eq_ignore_ascii_case(&owner.user) {
        return false;
    }
    let machine = std::env::var("COMPUTERNAME").unwrap_or_default();
    let is_local = |domain: &str| {
        let d = domain.trim();
        d.is_empty() || d == "." || d.eq_ignore_ascii_case(&machine)
    };
    is_local(&owner.domain) == is_local(&creds.domain)
        && (is_local(&owner.domain) || owner.domain.eq_ignore_ascii_case(&creds.domain))
}

/// Fires the eviction of a console session (§18.53): `WTSLogoffSession`
/// (default — a takeover reassigns the WHOLE machine) or
/// `WTSDisconnectSession`. Asynchronous on the WTS side: the caller (state
/// machine) waits for the console to flip with `EVICT_SETTLE_TIMEOUT`.
pub(crate) fn evict_start(session_id: u32, account: &Account, mode: server_config::TakeoverMode) -> Result<()> {
    match mode {
        server_config::TakeoverMode::Disconnect => {
            debug!(session_id, user = %account.display(), "disconnecting the console session (takeover)");
            // SAFETY: local server handle, valid session id.
            unsafe { WTSDisconnectSession(None, session_id, true) }.context("WTSDisconnectSession")?;
        }
        server_config::TakeoverMode::Logoff => {
            debug!(session_id, user = %account.display(), "LOGGING OFF the console session (takeover mode logoff)");
            // SAFETY: local server handle, valid session id.
            unsafe { WTSLogoffSession(None, session_id, true) }.context("WTSLogoffSession")?;
        }
    }
    Ok(())
}

/// Best-effort `SessionEnd` notification right before dropping a refused
/// connection (§18.53 reason codes).
pub(crate) async fn send_session_end<T: ReliableTransport>(stream: &mut EncryptedStream<T>, reason: u8) {
    let msg = ControlMessage::SessionEnd { reason };
    if let Ok(encoded) = msg.encode() {
        let _ = stream.send(channel::CONTROL, &encoded).await;
    }
}

/// Best-effort `SessionProgress` notice so the client can show what the
/// pre-session wait is about (§18.53 stage codes).
pub(crate) async fn send_progress<T: ReliableTransport>(stream: &mut EncryptedStream<T>, stage: u8) {
    let msg = ControlMessage::SessionProgress { stage };
    if let Ok(encoded) = msg.encode() {
        let _ = stream.send(channel::CONTROL, &encoded).await;
    }
}
