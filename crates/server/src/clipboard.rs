//! Clipboard bridge with the OS (M8): reports local clipboard changes and
//! applies remote ones. The session core stays portable — the Windows
//! backend (message-only window + `AddClipboardFormatListener`) lives behind
//! the `windows-capture` feature; elsewhere [`spawn`] returns `None` and the
//! channel stays silent.
//!
//! Anti-echo: setting the clipboard fires the same update notification we
//! listen to, so the backend remembers the last text it set itself and
//! suppresses that one echo (identical content arriving again later is a
//! genuine user copy and is reported).

use std::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedReceiver;

/// Local clipboard change observed by the OS backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardEvent {
    /// The clipboard text changed locally.
    TextChanged(String),
}

/// Command for the OS backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardCommand {
    /// Replace the local clipboard with this text.
    SetText(String),
}

/// Channel pair to talk with the clipboard backend thread.
pub struct ClipboardBridge {
    /// Events from the OS (local changes); unbounded so the listener never
    /// blocks the UI thread, and `tokio` so the session loop can `.await` it.
    pub events: UnboundedReceiver<ClipboardEvent>,
    /// Command queue to the OS backend.
    commands: Sender<ClipboardCommand>,
    /// Wake-up hook: posts a message to the listener window after queueing
    /// (no-op on platforms without a window loop).
    wake: Box<dyn Fn() + Send + Sync>,
}

impl ClipboardBridge {
    /// Queues a command and wakes the backend thread.
    pub fn send_command(&self, cmd: ClipboardCommand) {
        if self.commands.send(cmd).is_ok() {
            (self.wake)();
        }
    }
}

/// Spawns the platform clipboard backend. Returns `None` when the platform
/// has no backend (or the window could not be created): clipboard is then
/// simply inert for the session.
#[cfg(all(windows, feature = "windows-capture"))]
#[must_use]
pub fn spawn() -> Option<ClipboardBridge> {
    windows_impl::spawn()
}

/// Spawns the platform clipboard backend (no backend on this platform).
#[cfg(not(all(windows, feature = "windows-capture")))]
#[must_use]
pub fn spawn() -> Option<ClipboardBridge> {
    None
}

#[cfg(all(windows, feature = "windows-capture"))]
mod windows_impl {
    use super::{ClipboardBridge, ClipboardCommand, ClipboardEvent};
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use tokio::sync::mpsc::UnboundedSender;
    use tracing::{info, warn};
    use windows::Win32::Foundation::{GetLastError, GlobalFree, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, GetMessageW, HWND_MESSAGE, MSG, PostMessageW,
        RegisterClassW, WM_APP, WM_CLIPBOARDUPDATE, WNDCLASSW, WS_OVERLAPPED,
    };
    use windows::core::w;

    /// `CF_UNICODETEXT` clipboard format.
    const CF_UNICODETEXT: u32 = 13;
    /// Posted to the hidden window whenever a command is queued, to wake the
    /// message loop.
    const WM_COMMAND_READY: u32 = WM_APP + 1;
    /// Attempts to open the clipboard before giving up (another app may hold
    /// it briefly).
    const OPEN_RETRIES: u32 = 10;

    /// State shared with the backend thread: the text we last set ourselves
    /// (echo suppression) and the window handle for wake-ups. The handle is
    /// stored as `isize` so `Shared` stays `Send` (raw pointers are not).
    struct Shared {
        last_set: Mutex<Option<String>>,
        hwnd: Mutex<Option<isize>>,
    }

    /// Spawns the listener thread and returns the bridge channels; `None` if
    /// the window setup fails (logged).
    pub fn spawn() -> Option<ClipboardBridge> {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = channel::<ClipboardCommand>();
        let (ready_tx, ready_rx) = channel::<bool>();
        let shared = std::sync::Arc::new(Shared {
            last_set: Mutex::new(None),
            hwnd: Mutex::new(None),
        });
        let thread_shared = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || thread_main(thread_shared, event_tx, cmd_rx, ready_tx));
        match ready_rx.recv() {
            Ok(true) => Some(ClipboardBridge {
                events: event_rx,
                commands: cmd_tx,
                wake: Box::new(move || {
                    if let Some(hwnd) = *shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) {
                        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
                        unsafe {
                            let _ = PostMessageW(Some(hwnd), WM_COMMAND_READY, WPARAM(0), LPARAM(0));
                        }
                    }
                }),
            }),
            Ok(false) | Err(_) => None,
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Listener thread: hidden message-only window, clipboard format
    /// listener, and a message loop that also drains the command queue.
    fn thread_main(
        shared: std::sync::Arc<Shared>,
        events: UnboundedSender<ClipboardEvent>,
        commands: Receiver<ClipboardCommand>,
        ready: Sender<bool>,
    ) {
        unsafe {
            let hinstance = HINSTANCE(GetModuleHandleW(None).map_or(std::ptr::null_mut(), |m| m.0));
            let class_name = w!("UDSRemoteClipboardListener");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinstance,
                lpszClassName: class_name,
                ..Default::default()
            };
            // ERROR_CLASS_ALREADY_EXISTS (1410) is fine: a previous session
            // registered it already.
            if RegisterClassW(&wc) == 0 && GetLastError().0 != 1410 {
                warn!(error = ?windows::core::Error::from_thread(), "clipboard: RegisterClassW failed");
                ready.send(false).ok();
                return;
            }
            let hwnd = match CreateWindowExW(
                Default::default(),
                class_name,
                w!("UDS Clipboard Listener"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(hinstance),
                None,
            ) {
                Ok(h) => h,
                Err(e) => {
                    warn!(error = ?e, "clipboard: CreateWindowExW failed");
                    ready.send(false).ok();
                    return;
                }
            };
            if AddClipboardFormatListener(hwnd).is_err() {
                warn!("clipboard: AddClipboardFormatListener failed");
                ready.send(false).ok();
                return;
            }
            *shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) = Some(hwnd.0 as isize);
            ready.send(true).ok();
            info!("clipboard: listener ready");

            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&mut msg, Some(hwnd), 0, 0);
                if ret.0 <= 0 {
                    break;
                }
                if msg.message == WM_CLIPBOARDUPDATE {
                    on_clipboard_update(&shared, &events);
                } else if msg.message == WM_COMMAND_READY {
                    while let Ok(cmd) = commands.try_recv() {
                        match cmd {
                            ClipboardCommand::SetText(text) => {
                                set_clipboard_text(&shared, &text);
                            }
                        }
                    }
                }
            }
            *shared.hwnd.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// The OS clipboard changed: read the text and report it unless it is
    /// the echo of our own last set.
    fn on_clipboard_update(shared: &Shared, events: &UnboundedSender<ClipboardEvent>) {
        let Some(text) = read_clipboard_text() else {
            return;
        };
        let last = shared.last_set.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if last.as_deref() == Some(text.as_str()) {
            return; // echo of our own set
        }
        if events.send(ClipboardEvent::TextChanged(text)).is_err() {
            info!("clipboard: session gone, stopping reports");
        }
    }

    /// Opens the clipboard with a short retry loop; returns whether it is
    /// open on return. Callers must close it on success.
    fn open_clipboard() -> bool {
        for attempt in 0..OPEN_RETRIES {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return true;
            }
            if attempt + 1 < OPEN_RETRIES {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        false
    }

    /// Reads the current `CF_UNICODETEXT` content, if any.
    fn read_clipboard_text() -> Option<String> {
        if !open_clipboard() {
            return None;
        }
        unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let hglobal = HGLOBAL(handle.0);
            let size = GlobalSize(hglobal);
            let ptr = GlobalLock(hglobal);
            if ptr.is_null() || size < 2 {
                CloseClipboard().ok();
                return None;
            }
            let units = std::slice::from_raw_parts(ptr as *const u16, size / 2);
            let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            let text = String::from_utf16_lossy(&units[..end]);
            let _ = GlobalUnlock(hglobal);
            CloseClipboard().ok();
            Some(text)
        }
    }

    /// Replaces the clipboard with `text` (`CF_UNICODETEXT`) and records it
    /// for echo suppression.
    fn set_clipboard_text(shared: &Shared, text: &str) {
        // UTF-16 units including the trailing NUL (the audio driver lesson:
        // Windows API strings must always carry their terminator).
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        let bytes = wide.len() * 2;
        let ok = open_clipboard() && unsafe {
            let result: windows::core::Result<()> = (|| {
                EmptyClipboard()?;
                let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
                let ptr = GlobalLock(hglobal);
                if ptr.is_null() {
                    let _ = GlobalFree(Some(hglobal));
                    return Err(windows::core::Error::from_thread());
                }
                std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, bytes);
                let _ = GlobalUnlock(hglobal);
                if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(windows::Win32::Foundation::HANDLE(hglobal.0))) {
                    let _ = GlobalFree(Some(hglobal));
                    return Err(e);
                }
                // Ownership of the global passed to the clipboard.
                CloseClipboard()
            })();
            result.is_ok()
        };
        if ok {
            *shared.last_set.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.to_string());
        } else {
            warn!("clipboard: SetClipboardData failed");
        }
    }
}
