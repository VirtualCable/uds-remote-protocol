//! Hardware cursor tracking: `EvtIddCxAdapterCommitModes` re-enables hardware
//! cursor on every active path (a mode commit resets the path to software
//! cursor — the IddCx default), and a dedicated thread waits on the update
//! event the OS signals to pull position / visibility / shape through
//! `IddCxMonitorQueryHardwareCursor`.
//!
//! The tracked state is what the captured session needs to paint the cursor
//! client-side (the DXGI duplication channel reports pointer position and
//! shape on its own; this gives the driver its own ground truth) and is
//! exposed to `udsdisp-ctl cursor` through `IOCTL_GET_CURSOR`.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows::core::PCWSTR;

use super::{adapter::size32, iddcx, iddcx::bindings, log_line, shared};
use crate::logic;

/// Maximum cursor dimension the driver advertises (`IDDCX_CURSOR_CAPS.MaxX/Y`);
/// 128x128 covers every standard and accessibility-scaled cursor.
const MAX_CURSOR_SIZE: u32 = 128;

/// Shape buffer handed to `IddCxMonitorQueryHardwareCursor`: one 32-bpp pixel
/// per cursor pixel, doubled for masked-color shapes (color plane + XOR mask).
const SHAPE_BUFFER_LEN: usize = (MAX_CURSOR_SIZE * MAX_CURSOR_SIZE * 4 * 2) as usize;

/// Position changes are logged at most this often (they fire per mouse event;
/// the log is for verification, not telemetry).
const POSITION_LOG_INTERVAL: Duration = Duration::from_millis(500);

/// Last known cursor state of a monitor (plain data; lives in the shared
/// monitor entry and is read by the control channel).
pub struct State {
    /// The wire-reported status (see [`logic::CursorStatus`]).
    pub status: logic::CursorStatus,
    /// Successful queries since the tracking task started.
    pub queries: u64,
}

/// A running cursor-tracking task (one per monitor).
pub struct Task {
    /// Stop signal for the tracking thread (Win32 event as `usize`).
    pub stop_event: usize,
    /// Data-available signal registered with `IddCxMonitorSetupHardwareCursor`.
    pub data_event: usize,
    /// Tracking thread; joined on stop.
    pub thread: Option<std::thread::JoinHandle<()>>,
}

/// Enables hardware cursor on `monitor`'s path: creates the tracking task on
/// first call, then (re-)registers the data event with IddCx. Called from
/// `EvtIddCxAdapterCommitModes` for every active path — a mode commit resets
/// the path to software cursor, so this must run on every commit, and it must
/// NOT run with the shared state lock held (it locks internally).
pub fn enable(monitor: bindings::IDDCX_MONITOR) {
    let data_event = {
        let mut state = shared::lock();
        let Some(entry) = state.monitors.iter_mut().find(|m| m.handle == monitor as usize) else {
            log_line(&format!("cursor enable for unknown monitor {monitor:?}"));
            return;
        };
        if entry.cursor_task.is_none() {
            match start_task(monitor) {
                Some(task) => {
                    entry.cursor = Some(State {
                        status: logic::CursorStatus::default(),
                        queries: 0,
                    });
                    let data_event = task.data_event;
                    entry.cursor_task = Some(task);
                    data_event
                }
                None => return,
            }
        } else {
            entry.cursor_task.as_ref().map_or(0, |t| t.data_event)
        }
    };

    let in_args = bindings::IDARG_IN_SETUP_HWCURSOR {
        CursorInfo: bindings::IDDCX_CURSOR_CAPS {
            Size: size32::<bindings::IDDCX_CURSOR_CAPS>(),
            MaxX: MAX_CURSOR_SIZE,
            MaxY: MAX_CURSOR_SIZE,
            AlphaCursorSupport: 1,
            ColorXorCursorSupport:
                bindings::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_FULL,
        },
        hNewCursorDataAvailable: data_event as *mut core::ffi::c_void,
    };
    // SAFETY: `monitor` is a live, arrived monitor with an active path (the
    // caller is CommitModes); `data_event` stays open until `stop` runs, which
    // only happens at departure (after the path is gone).
    let status = unsafe { iddcx::monitor_setup_hardware_cursor(monitor, &in_args) };
    log_line(&format!("IddCxMonitorSetupHardwareCursor monitor={monitor:?} -> {status:#010x}"));
}

/// Stops the tracking task of `monitor` (raw handle as `usize`) and closes its
/// events. Idempotent: the task is an `Option::take`.
pub(crate) fn stop(monitor: usize) {
    let task = shared::lock()
        .monitors
        .iter_mut()
        .find(|m| m.handle == monitor)
        .and_then(|m| m.cursor_task.take());
    if let Some(task) = task {
        // SAFETY: both events are ours and still open; they are closed after
        // the join so the thread can never observe a closed handle.
        unsafe {
            let _ = SetEvent(HANDLE(task.stop_event as *mut core::ffi::c_void));
        }
        if let Some(thread) = task.thread {
            let _ = thread.join();
        }
        // SAFETY: the thread is joined; nobody references the events anymore
        // (the path is gone by the time departure runs this).
        unsafe {
            let _ = CloseHandle(HANDLE(task.stop_event as *mut core::ffi::c_void));
            let _ = CloseHandle(HANDLE(task.data_event as *mut core::ffi::c_void));
        }
        log_line("cursor task stopped");
    }
}

/// Creates the two events and spawns the tracking thread. `None` (with a log
/// line) when event creation fails.
fn start_task(monitor: bindings::IDDCX_MONITOR) -> Option<Task> {
    // SAFETY: standard event creation; unnamed, auto-reset, initially unset.
    let stop_event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
        Ok(event) => event,
        Err(e) => {
            log_line(&format!("cursor stop CreateEventW failed: {e}"));
            return None;
        }
    };
    // SAFETY: same.
    let data_event = match unsafe { CreateEventW(None, false, false, PCWSTR::null()) } {
        Ok(event) => event,
        Err(e) => {
            log_line(&format!("cursor data CreateEventW failed: {e}"));
            // SAFETY: ours, not shared yet.
            unsafe {
                let _ = CloseHandle(stop_event);
            }
            return None;
        }
    };
    let monitor_handle = monitor as usize;
    let (stop, data) = (stop_event.0 as usize, data_event.0 as usize);
    let thread = std::thread::spawn(move || tracking_loop(monitor_handle, data, stop));
    Some(Task {
        stop_event: stop,
        data_event: data,
        thread: Some(thread),
    })
}

/// The tracking loop, on its own thread: wait for the OS to signal cursor data
/// (or the stop event), query the cursor, and update the shared state.
fn tracking_loop(monitor: usize, data_event: usize, stop_event: usize) {
    let monitor = monitor as bindings::IDDCX_MONITOR;
    let mut shape_buffer = vec![0u8; SHAPE_BUFFER_LEN];
    let mut last_shape_id: u32 = 0;
    let mut last_position_log = Instant::now() - Duration::from_secs(60);
    loop {
        let handles = [
            HANDLE(data_event as *mut core::ffi::c_void),
            HANDLE(stop_event as *mut core::ffi::c_void),
        ];
        // SAFETY: both handles are valid Win32 events, closed only after this
        // thread exits (stop() joins before closing).
        let wait = unsafe { WaitForMultipleObjects(&handles, false, u32::MAX) };
        if wait == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
            break; // stop requested
        }
        if wait != WAIT_OBJECT_0 {
            log_line(&format!("cursor WaitForMultipleObjects -> {wait:?}, exiting"));
            break;
        }

        let in_args = bindings::IDARG_IN_QUERY_HWCURSOR {
            LastShapeId: last_shape_id,
            ShapeBufferSizeInBytes: {
                const { assert!(SHAPE_BUFFER_LEN <= u32::MAX as usize) }
                #[allow(clippy::cast_possible_truncation)]
                {
                    SHAPE_BUFFER_LEN as u32
                }
            },
            pShapeBuffer: shape_buffer.as_mut_ptr().cast(),
        };
        // SAFETY: POD out-struct.
        let mut out: bindings::IDARG_OUT_QUERY_HWCURSOR = unsafe { core::mem::zeroed() };
        // SAFETY: `monitor` is live (the task is stopped before departure) and
        // `shape_buffer` is a writable SHAPE_BUFFER_LEN buffer.
        let status = unsafe { iddcx::monitor_query_hardware_cursor(monitor, &in_args, &mut out) };
        if status < 0 {
            log_line(&format!("IddCxMonitorQueryHardwareCursor -> {status:#010x}"));
            continue;
        }

        let shape_updated = out.IsCursorShapeUpdated != 0;
        if shape_updated {
            last_shape_id = out.CursorShapeInfo.ShapeId;
        }
        let (visible, x, y, shape) = (
            out.IsCursorVisible != 0,
            out.X,
            out.Y,
            &out.CursorShapeInfo,
        );

        let (prev_visible, prev_shape_id, queries) = {
            let mut state = shared::lock();
            if let Some(cursor) = state
                .monitors
                .iter_mut()
                .find(|m| m.handle == monitor as usize)
                .and_then(|m| m.cursor.as_mut())
            {
                let prev = (cursor.status.visible, cursor.status.shape_id);
                cursor.status = logic::CursorStatus {
                    known: true,
                    visible,
                    x,
                    y,
                    shape_id: last_shape_id,
                    width: if shape_updated { shape.Width } else { cursor.status.width },
                    height: if shape_updated { shape.Height } else { cursor.status.height },
                    pitch: if shape_updated { shape.Pitch } else { cursor.status.pitch },
                    shape_type: if shape_updated { shape.CursorType } else { cursor.status.shape_type },
                };
                cursor.queries += 1;
                (prev.0, prev.1, cursor.queries)
            } else {
                // Monitor gone mid-flight: nothing to update, leave the loop.
                break;
            }
        };

        if visible != prev_visible {
            log_line(&format!("cursor visibility: {prev_visible} -> {visible} at ({x},{y})"));
        }
        if shape_updated && last_shape_id != prev_shape_id {
            log_line(&format!(
                "cursor shape id={} type={} {}x{} pitch={}",
                shape.ShapeId, shape.CursorType, shape.Width, shape.Height, shape.Pitch
            ));
        }
        if last_position_log.elapsed() >= POSITION_LOG_INTERVAL {
            log_line(&format!("cursor at ({x},{y}) visible={visible} queries={queries}"));
            last_position_log = Instant::now();
        }
    }
    log_line("cursor tracking loop exit");
}
