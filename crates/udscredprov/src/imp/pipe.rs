//! Named-pipe client: connects from inside LogonUI to the `uds-rp` service
//! pipe (`\\.\pipe\udsrp-credprov`), receives credential requests and sends
//! back acknowledgements and logon results.
//!
//! The connection is persistent, self-healing and process-global: LogonUI
//! creates a new provider instance per enumeration cycle, so the pipe client
//! thread and its endpoint live in a singleton shared by every instance
//! (exactly like the credential store), pinned for the process lifetime so
//! the DLL is never unloaded while the thread runs. Any pipe error (service
//! restart, broken pipe) triggers a reconnect with backoff.

use std::sync::{Arc, Mutex, OnceLock};

use std::time::Duration;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HMODULE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GetModuleHandleExW};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::{HRESULT, PCWSTR};

use crate::imp::credentials::{provider, store};
use crate::imp::utils::log::{debug, error, info};
use crate::imp::utils::safe::SafeHandle;
use crate::logic::{
    self, FrameDecoder, Request, RequestKind, Response,
};
use crate::{debug_dev, debug_flow};

/// Shared write endpoint: `Some` while a pipe connection is established.
/// Both the read loop (acks, pongs) and `ReportResult` (logon results)
/// serialize their writes through the mutex.
pub type SharedPipe = Arc<Mutex<Option<SafeHandle>>>;

/// The process-global pipe endpoint shared by every provider/credential
/// instance of this DLL.
pub fn shared_pipe() -> &'static SharedPipe {
    static PIPE: OnceLock<SharedPipe> = OnceLock::new();
    PIPE.get_or_init(|| Arc::new(Mutex::new(None)))
}

/// Starts the singleton pipe client thread (idempotent). The thread runs
/// until the process exits; the DLL is pinned so it cannot be unloaded
/// from under the running thread.
pub fn ensure_started() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        pin_dll();
        let pipe = shared_pipe().clone();
        std::thread::spawn(move || run(pipe));
    });
}

/// Pins this DLL in the process: bumps the module reference count so
/// `FreeLibrary` (via `CoFreeUnusedLibraries`) can never unmap the code the
/// pipe thread is executing.
fn pin_dll() {
    let mut module = HMODULE::default();
    let _ = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(pin_dll as *const () as usize as *const u16),
            &mut module,
        )
    };
}

/// Sends a response frame if connected. Errors are logged and swallowed:
/// the pipe is a reporting channel and must never break the logon flow.
pub fn send_response(pipe: &SharedPipe, response: &Response) {
    let frame = match logic::encode_response(response) {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to encode pipe response: {e}");
            return;
        }
    };
    let handle = pipe.lock().unwrap().clone();
    if let Some(handle) = handle {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(handle.get(), Some(&frame), Some(&mut written), None).is_ok()
                && written as usize == frame.len()
        };
        if !ok {
            error!("Failed to write response to pipe; dropping the connection");
            // Force the read loop into its reconnect path
            handle.clear();
            pipe.lock().unwrap().take();
        }
    } else {
        debug_dev!("No pipe connection; dropping response {:?}", kind_of(response));
    }
}

fn kind_of(response: &Response) -> &'static str {
    match response {
        Response::Hello { .. } => "Hello",
        Response::Ack { .. } => "Ack",
        Response::LogonResult { .. } => "LogonResult",
        Response::Pong => "Pong",
    }
}

/// Connects to the service pipe, retrying forever while it is busy or not
/// yet created (the service may start after LogonUI). Returns `None` only if
/// the pipe name is malformed.
fn connect() -> Option<SafeHandle> {
    let pipe_name = widestring::U16CString::from_str(logic::PIPE_NAME).ok()?;
    let pipe_name = PCWSTR::from_raw(pipe_name.as_ptr());

    loop {
        let result = unsafe {
            CreateFileW(
                pipe_name,
                GENERIC_READ.0 | GENERIC_WRITE.0,
                windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        match result {
            Ok(handle) => {
                info!("Connected to {}", logic::PIPE_NAME);
                return Some(SafeHandle::new(handle));
            }
            Err(e) if e.code() == HRESULT::from_win32(ERROR_PIPE_BUSY.0) => {
                // Another client holds the pipe; wait for a free instance
                let _ = unsafe { WaitNamedPipeW(pipe_name, 1000) };
            }
            Err(e) if e.code() == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0) => {
                // Service not listening yet; back off and retry
                std::thread::sleep(Duration::from_millis(1000));
            }
            Err(e) => {
                error!("CreateFileW({}) failed: {e:?}", logic::PIPE_NAME);
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    }
}

/// Dispatches one request and answers it over the pipe.
fn dispatch(pipe: &SharedPipe, request: &Request) {
    match request {
        Request::Ping => {
            debug_flow!("pipe: Ping received");
            send_response(pipe, &Response::Pong);
        }
        Request::SetCredentials(creds) => {
            debug_flow!("pipe: SetCredentials received for user {}", creds.username);
            debug_dev!("Storing pipe credentials for {}", creds.username);
            store::set(creds.clone());
            // Ask LogonUI to re-enumerate, which triggers the auto-logon path
            let (accepted, detail) = match provider::fire_credentials_changed() {
                Ok(()) => (true, "credentials stored".to_string()),
                Err(e) => {
                    error!("fire_credentials_changed failed: {e:?}");
                    (false, format!("{e}"))
                }
            };
            send_response(
                pipe,
                &Response::Ack {
                    request_kind: RequestKind::SetCredentials as u16,
                    accepted,
                    detail,
                },
            );
        }
        Request::ClearCredentials => {
            debug_flow!("pipe: ClearCredentials received");
            debug_dev!("Clearing pipe credentials");
            store::clear();
            send_response(
                pipe,
                &Response::Ack {
                    request_kind: RequestKind::ClearCredentials as u16,
                    accepted: true,
                    detail: "credentials cleared".to_string(),
                },
            );
        }
    }
}

/// Read/dispatch loop for one established connection. Returns when the
/// connection breaks.
fn session(handle: SafeHandle, pipe: &SharedPipe) {
    // Announce ourselves so the service knows which protocol we speak
    send_response(
        pipe,
        &Response::Hello {
            version: logic::PROTOCOL_VERSION,
        },
    );

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 2048];
    loop {
        let mut read = 0u32;
        let result = unsafe { ReadFile(handle.get(), Some(&mut buf), Some(&mut read), None) };
        match result {
            Ok(()) => {
                if read == 0 {
                    continue;
                }
                if let Err(e) = decoder.feed(&buf[..read as usize]) {
                    error!("Pipe protocol error: {e}; reconnecting");
                    break;
                }
                loop {
                    match decoder.next_request() {
                        Ok(Some(request)) => dispatch(pipe, &request),
                        Ok(None) => break,
                        Err(e) => {
                            error!("Pipe frame decode error: {e}; reconnecting");
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Pipe read failed ({e:?}); reconnecting");
                break;
            }
        }
    }
}

/// Reconnecting client loop; runs on the singleton pipe thread for the whole
/// process lifetime. `pipe` is the shared write endpoint handed to the
/// credential so `ReportResult` can report logon outcomes.
pub fn run(pipe: SharedPipe) {
    debug_flow!("pipe: client loop starting");
    loop {
        let Some(handle) = connect() else {
            break;
        };
        *pipe.lock().unwrap() = Some(handle.clone());
        session(handle, &pipe);
        pipe.lock().unwrap().take();
    }
    debug_flow!("pipe: client loop stopped");
}
