// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Local clipboard handle: event-driven watcher (clipboard-rs), text only.
//!
//! Replaces the two ad-hoc implementations the consumers grew (arboard
//! poll-on-focus in labclient/gui, polling thread in testclient) with the
//! uds-client model: one watcher thread firing a callback on every local
//! change. Echo suppression is built in: what we write via [`set_text`]
//! (remote clipboard applied locally) is not announced back, and repeated
//! values are announced once.
//!
//! [`set_text`]: ClipboardHandle::set_text

use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use clipboard_rs::{
    Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
};

/// Consumer notified when the LOCAL clipboard gains new text.
pub trait ClipboardCallback: Send + Sync {
    /// Called on the watcher thread with each new local clipboard text.
    fn text_available(&self, text: String);
}

/// Stops the watcher thread; boxed so the concrete shutdown-channel type
/// stays an implementation detail of `start`.
type StopWatch = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// Local clipboard access plus an optional change watcher. Cloneable: all
/// clones share the same OS context and watcher state.
#[derive(Clone)]
pub struct ClipboardHandle {
    context: Arc<RwLock<ClipboardContext>>,
    stop_watch: StopWatch,
    /// Last text written or announced: echo suppression.
    last: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for ClipboardHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardHandle").finish_non_exhaustive()
    }
}

struct Watcher {
    callback: Arc<dyn ClipboardCallback>,
    context: Arc<RwLock<ClipboardContext>>,
    last: Arc<Mutex<Option<String>>>,
}

impl ClipboardHandler for Watcher {
    fn on_clipboard_change(&mut self) {
        let Ok(text) = self.context.read().unwrap().get_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let mut last = self.last.lock().unwrap();
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        *last = Some(text.clone());
        drop(last);
        self.callback.text_available(text);
    }
}

impl ClipboardHandle {
    /// Opens the OS clipboard; `Err` where none is available (headless).
    pub fn new() -> Result<Self> {
        let context = ClipboardContext::new().map_err(|e| anyhow::anyhow!(e))?;
        Ok(Self {
            context: Arc::new(RwLock::new(context)),
            stop_watch: Arc::new(Mutex::new(None)),
            last: Arc::new(Mutex::new(None)),
        })
    }

    /// Starts the change watcher (replacing any previous one): the callback
    /// fires on a watcher thread for every new local text.
    pub fn start(&self, callback: Arc<dyn ClipboardCallback>) -> Result<()> {
        self.stop();
        let mut watcher = ClipboardWatcherContext::new().map_err(|e| anyhow::anyhow!(e))?;
        let shutdown = watcher
            .add_handler(Watcher {
                callback,
                context: Arc::clone(&self.context),
                last: Arc::clone(&self.last),
            })
            .get_shutdown_channel();
        std::thread::spawn(move || watcher.start_watch());
        *self.stop_watch.lock().unwrap() = Some(Box::new(move || shutdown.stop()));
        Ok(())
    }

    /// Stops the watcher, if running.
    pub fn stop(&self) {
        if let Some(stop) = self.stop_watch.lock().unwrap().take() {
            stop();
        }
    }

    /// Writes text (remote clipboard applied locally): not re-announced.
    pub fn set_text(&self, text: &str) -> Result<()> {
        *self.last.lock().unwrap() = Some(text.to_string());
        self.context
            .read()
            .unwrap()
            .set_text(text.to_string())
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Reads the current local text.
    pub fn get_text(&self) -> Result<String> {
        self.context
            .read()
            .unwrap()
            .get_text()
            .map_err(|e| anyhow::anyhow!(e))
    }
}

impl Drop for ClipboardHandle {
    fn drop(&mut self) {
        // Only the last clone stops the watcher; `stop()` is idempotent and
        // the watcher thread exits with the process anyway.
        if Arc::strong_count(&self.stop_watch) == 1 {
            self.stop();
        }
    }
}
