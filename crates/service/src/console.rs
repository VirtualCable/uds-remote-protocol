// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Console-session ownership (§18.53): who holds the physical console and
//! how a new session takes it over.
//!
//! The detection runs in the service right after the ticket is validated
//! and BEFORE any session side effect (autologon, agent spawn): when the
//! console's interactive session belongs to a user other than the one in
//! the session credentials, the configured `ConsoleConflictPolicy`
//! applies — `take_over` (default: the ticket IS the authorization),
//! `deny` (reject with `SESSION_END_CONSOLE_BUSY`) or `ask` (the
//! CONNECTING client decides; the local console user has no voice — anyone
//! sitting in front of the machine could otherwise deny remote access).
//!
//! The eviction itself is `WTSDisconnectSession` (default: the evicted
//! session survives disconnected, keeping its apps) or `WTSLogoffSession`
//! (config `console_takeover = "logoff"`).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};
use transport::{EncryptedStream, ReliableTransport};
use udscredprov::logic::Credentials;
use windows::Win32::System::RemoteDesktop::{
    WTSDisconnectSession, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSLogoffSession,
    WTSQuerySessionInformationW, WTSUserName,
};

use protocol::ControlMessage;
use protocol::channel;
use protocol::{
    CONSOLE_STATE_ACTIVE, CONSOLE_STATE_LOCKED, SESSION_END_CONSOLE_BUSY,
    SESSION_END_DENIED_BY_USER,
};
/// How long to wait for an evicted console to actually flip before arming
/// the autologon (disconnect is asynchronous on the WTS side).
const EVICT_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Server-side conflict configuration, resolved from the standalone config
/// or the broker ticket payload.
pub struct ConflictConfig {
    /// What to do when the console owner differs from the session user.
    pub policy: server_config::ConsoleConflictPolicy,
    /// Whether the `ask` message may carry the console owner's name.
    pub reveal_user: bool,
    /// Ask timeout; without an explicit answer the connection is denied.
    pub timeout: Duration,
    /// Disconnect (keep apps) or logoff (kill apps) on takeover.
    pub takeover: server_config::TakeoverMode,
}

/// Snapshot of the interactive console session, when there is one.
pub struct ConsoleOwner {
    /// WTS session id.
    pub id: u32,
    /// Session user name (no domain), as reported by `WTSUserName`.
    pub user: String,
    /// True while the session shows the lock screen.
    pub locked: bool,
}

/// The interactive console session, or `None` at the logon screen.
fn active_console() -> Option<ConsoleOwner> {
    // SAFETY: plain query, no out-params.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == 0xFFFF_FFFF {
        return None;
    }
    let mut buf = windows::core::PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: valid out-params; on success the buffer is freed below.
    let queried = unsafe {
        WTSQuerySessionInformationW(None, id, WTSUserName, &raw mut buf, &raw mut bytes)
    };
    if queried.is_err() || buf.is_null() {
        warn!(session_id = id, "console session without a queryable user");
        return None;
    }
    // SAFETY: on success the buffer holds a null-terminated UTF-16 string.
    let user = unsafe { buf.to_string().unwrap_or_default() };
    unsafe { WTSFreeMemory(buf.0.cast()) };
    if user.is_empty() {
        return None;
    }
    Some(ConsoleOwner {
        id,
        user,
        locked: crate::agent::console_session_locked(id),
    })
}

/// Applies the conflict policy before any session side effect. `creds` are
/// the session's auto-logon credentials; without them there is no identity
/// to compare and the existing console is simply mirrored (no conflict).
///
/// Sends `SessionEnd` and bails on refusal; on takeover evicts the console
/// owner and returns so the caller's autologon path brings up the new
/// user's session.
pub async fn resolve_conflict<T: ReliableTransport>(
    stream: &mut EncryptedStream<T>,
    creds: Option<&Credentials>,
    config: &ConflictConfig,
) -> Result<()> {
    let Some(owner) = active_console() else {
        return Ok(());
    };
    let Some(creds) = creds else {
        return Ok(());
    };
    if creds.username.eq_ignore_ascii_case(&owner.user) {
        return Ok(());
    }
    info!(
        console_user = %owner.user,
        session_user = %creds.username,
        locked = owner.locked,
        policy = ?config.policy,
        "console conflict: another user owns the console"
    );
    let take_over = match config.policy {
        server_config::ConsoleConflictPolicy::TakeOver => true,
        server_config::ConsoleConflictPolicy::Deny => {
            send_session_end(stream, SESSION_END_CONSOLE_BUSY).await;
            bail!("console busy (owned by another user); policy is deny");
        }
        server_config::ConsoleConflictPolicy::Ask => {
            ask_client(stream, &owner, config).await?
        }
    };
    if !take_over {
        send_session_end(stream, SESSION_END_DENIED_BY_USER).await;
        bail!("console takeover refused by the connecting client");
    }
    evict(&owner, config.takeover)
}

/// Policy `ask`: queries the CONNECTING client (never the local console)
/// and waits for its `ConsoleConflictReply` up to the configured timeout.
/// Timeout or a negative answer are both a refusal (fail-closed).
async fn ask_client<T: ReliableTransport>(
    stream: &mut EncryptedStream<T>,
    owner: &ConsoleOwner,
    config: &ConflictConfig,
) -> Result<bool> {
    let query = ControlMessage::ConsoleConflict {
        user: config.reveal_user.then(|| owner.user.clone()),
        state: if owner.locked {
            CONSOLE_STATE_LOCKED
        } else {
            CONSOLE_STATE_ACTIVE
        },
    };
    stream.send(channel::CONTROL, &query.encode()?).await?;
    let mut buf = [0u8; 2048];
    let reply = tokio::time::timeout(config.timeout, async {
        loop {
            let (ch, n) = stream.recv_into(&mut buf).await?;
            if ch != channel::CONTROL {
                continue;
            }
            if let Ok(ControlMessage::ConsoleConflictReply { take_over }) =
                ControlMessage::decode(&buf[..n])
            {
                return Ok::<bool, anyhow::Error>(take_over);
            }
        }
    })
    .await;
    match reply {
        Ok(Ok(take_over)) => {
            info!(take_over, "console conflict answered by the client");
            Ok(take_over)
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            info!(timeout = ?config.timeout, "console conflict ask timed out; denying");
            Ok(false)
        }
    }
}

/// Evicts the console owner per the takeover mode and waits for the
/// console to actually flip before returning (the autologon path keys on
/// "no active console session").
fn evict(owner: &ConsoleOwner, mode: server_config::TakeoverMode) -> Result<()> {
    match mode {
        server_config::TakeoverMode::Disconnect => {
            info!(session_id = owner.id, user = %owner.user, "disconnecting the console session (takeover)");
            // SAFETY: local server handle, valid session id.
            unsafe { WTSDisconnectSession(None, owner.id, true) }.context("WTSDisconnectSession")?;
        }
        server_config::TakeoverMode::Logoff => {
            warn!(session_id = owner.id, user = %owner.user, "LOGGING OFF the console session (takeover mode logoff)");
            // SAFETY: local server handle, valid session id.
            unsafe { WTSLogoffSession(None, owner.id, true) }.context("WTSLogoffSession")?;
        }
    }
    let start = std::time::Instant::now();
    loop {
        // SAFETY: plain query, no out-params.
        let current = unsafe { WTSGetActiveConsoleSessionId() };
        if current != owner.id {
            return Ok(());
        }
        if start.elapsed() >= EVICT_SETTLE_TIMEOUT {
            bail!("console session {} still active {EVICT_SETTLE_TIMEOUT:?} after the eviction", owner.id);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Best-effort `SessionEnd` notification right before dropping a refused
/// connection (§18.53 reason codes).
async fn send_session_end<T: ReliableTransport>(stream: &mut EncryptedStream<T>, reason: u8) {
    let msg = ControlMessage::SessionEnd { reason };
    if let Ok(encoded) = msg.encode() {
        let _ = stream.send(channel::CONTROL, &encoded).await;
    }
}
