// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Logging for the credential provider DLL.
//!
//! Adapted from the reference provider: a rotating file appender driven by
//! `tracing`, plus the `debug_flow!` / `debug_dev!` macros used to trace the
//! LogonUI call sequence. Unlike the reference, a log file that cannot be
//! opened is not fatal — the provider is loaded by LogonUI and a panic in
//! `DllMain` would break the logon screen, so the writer degrades to a
//! sink.

use std::{fs, fs::OpenOptions, io, path::PathBuf, sync::OnceLock};
use tracing_subscriber::{
    EnvFilter, Layer, filter::filter_fn, fmt, layer::SubscriberExt, util::SubscriberInitExt,
};

// Reexport so call sites do not need to name the `tracing` crate.
pub use tracing::{debug, error, info};

static LOGGER_INIT: OnceLock<()> = OnceLock::new();
/// Whether the (verbose) call-flow log is enabled.
pub static LOG_FLOW_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Sequence counter of the call-flow log.
pub static FLOW_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Timestamp of the last call-flow entry (for relative ms).
pub static LAST_STEP: std::sync::OnceLock<std::sync::RwLock<std::time::Instant>> =
    std::sync::OnceLock::new();

/// Log writer that degrades to a sink when the file cannot be opened.
pub struct MaybeFile(Option<fs::File>);

impl io::Write for MaybeFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.0 {
            Some(f) => f.write(buf),
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0 {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

struct RotatingWriter {
    path: PathBuf,
    max_size: u64,
    max_files: usize,
}

impl RotatingWriter {
    fn rotate_if_needed(&self) {
        if let Ok(meta) = fs::metadata(&self.path)
            && meta.len() >= self.max_size
        {
            if self.max_files > 1 {
                let last = self.path.with_extension(format!("log.{}", self.max_files));
                let _ = fs::remove_file(&last);
                for i in (1..self.max_files).rev() {
                    let src = self.path.with_extension(format!("log.{i}"));
                    let dst = self.path.with_extension(format!("log.{}", i + 1));
                    let _ = fs::rename(&src, &dst);
                }
                let rotated = self.path.with_extension("log.1");
                let _ = fs::rename(&self.path, rotated);
            } else {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

impl<'a> fmt::MakeWriter<'a> for RotatingWriter {
    type Writer = MaybeFile;

    fn make_writer(&'a self) -> Self::Writer {
        self.rotate_if_needed();
        MaybeFile(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok(),
        )
    }
}

/// Initializes logging (idempotent). Level and directory can be overridden
/// with the `UDSRPCP_LOG_LEVEL` / `UDSRPCP_LOG_PATH` environment variables;
/// the default directory is the process temp dir (`C:\Windows\Temp` for
/// LogonUI, which runs as SYSTEM).
pub fn setup_logging(level: &str) {
    let level = std::env::var("UDSRPCP_LOG_LEVEL").unwrap_or_else(|_| level.to_string());
    let log_path = std::env::var("UDSRPCP_LOG_PATH")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into());

    LOGGER_INIT.get_or_init(|| {
        let main_layer = fmt::layer()
            .with_writer(RotatingWriter {
                path: std::path::Path::new(&log_path).join("uds-rp-credprov.log"),
                max_size: 16 * 1024 * 1024,
                max_files: 2,
            })
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_filter(EnvFilter::new(format!("{level},flow=off")));

        LOG_FLOW_ENABLED.store(
            std::env::var("UDSRPCP_ENABLE_FLOW_LOG").unwrap_or_default() == "1",
            std::sync::atomic::Ordering::Relaxed,
        );

        if LOG_FLOW_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
            let flow_layer = fmt::layer()
                .with_writer(RotatingWriter {
                    path: std::path::Path::new(&log_path).join("uds-rp-credprov-flow.log"),
                    max_size: 16 * 1024 * 1024,
                    max_files: 2,
                })
                .with_ansi(false)
                .with_target(true)
                .with_level(true)
                .with_filter(filter_fn(|meta| meta.target() == "flow"));

            tracing_subscriber::registry()
                .with(main_layer)
                .with(flow_layer)
                .try_init()
                .ok();
        } else {
            tracing_subscriber::registry()
                .with(main_layer)
                .try_init()
                .ok();
        }

        info!("Logging initialized with level: {level}");
    });
}

/// Resets the call-flow counters (called when a new provider is created).
pub fn reset_flow_counter() {
    FLOW_COUNTER.store(0, std::sync::atomic::Ordering::Relaxed);
    LAST_STEP
        .set(std::sync::RwLock::new(std::time::Instant::now()))
        .ok();
}

/// Logs one numbered step of the LogonUI call sequence (enabled via
/// `UDSRPCP_ENABLE_FLOW_LOG=1`). Entries are prefixed with the process id:
/// several processes (multiple LogonUI instances) share the same log file.
#[macro_export]
macro_rules! debug_flow {
    ($($arg:tt)*) => {{
        if $crate::imp::utils::log::LOG_FLOW_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
            {
                let count = $crate::imp::utils::log::FLOW_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let last_guard = $crate::imp::utils::log::LAST_STEP.get_or_init(|| std::sync::RwLock::new(std::time::Instant::now()));
                if count == 1 {
                    tracing::info!(target: "flow", "----------------------------------------");
                }
                let s = format!($($arg)*);
                tracing::info!(target: "flow", "[pid {:>5}][#{count:5}][{:>5} ms] {}", std::process::id(), last_guard.read().unwrap().elapsed().as_millis(), s);
                *last_guard.write().unwrap() = std::time::Instant::now();
            }
    }};
}

/// Debug-build-only log line, prefixed with the process id (several
/// processes share the same log file).
#[macro_export]
macro_rules! debug_dev {
    ($($arg:tt)*) => {
        #[cfg(debug_assertions)]
        {
            let s = format!($($arg)*);
            tracing::info!(target: "dev", "[pid {:>5}] {}", std::process::id(), s);
        }
    };
}
