//! Named-pipe client: connects from inside LogonUI to the `uds-rp` service
//! pipe (`\\.\pipe\udsrp-credprov`), receives credential requests and sends
//! back acknowledgements and logon results.
//!
//! The connection is persistent and self-healing: the read loop runs on its
//! own thread, and any pipe error (service restart, broken pipe) triggers a
//! reconnect with backoff until the provider is dropped.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::core::{HRESULT, PCWSTR};

use crate::imp::credentials::provider::UdsRpCredentialsProvider;
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

/// Creates an empty (disconnected) shared pipe endpoint.
pub fn new_shared_pipe() -> SharedPipe {
    Arc::new(Mutex::new(None))
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

/// Connects to the service pipe, retrying while it is busy or not yet
/// created. Returns `None` when `stop` was requested.
fn connect(stop: &AtomicBool) -> Option<SafeHandle> {
    let pipe_name = widestring::U16CString::from_str(logic::PIPE_NAME).ok()?;
    let pipe_name = PCWSTR::from_raw(pipe_name.as_ptr());

    while !stop.load(Ordering::Relaxed) {
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
                if !unsafe { WaitNamedPipeW(pipe_name, 1000) }.as_bool() && stop.load(Ordering::Relaxed)
                {
                    return None;
                }
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
    None
}

/// Dispatches one request and answers it over the pipe.
fn dispatch(provider: &UdsRpCredentialsProvider, pipe: &SharedPipe, request: &Request) {
    match request {
        Request::Ping => {
            debug_flow!("pipe: Ping received");
            send_response(pipe, &Response::Pong);
        }
        Request::SetCredentials(creds) => {
            debug_flow!("pipe: SetCredentials received for user {}", creds.username);
            let (accepted, detail) = match provider.on_set_credentials(creds.clone()) {
                Ok(()) => (true, "credentials stored".to_string()),
                Err(e) => {
                    error!("on_set_credentials failed: {e:?}");
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
            provider.on_clear_credentials();
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
/// connection breaks or `stop` is requested.
fn session(
    handle: SafeHandle,
    provider: &UdsRpCredentialsProvider,
    pipe: &SharedPipe,
    stop: &AtomicBool,
) {
    // Announce ourselves so the service knows which protocol we speak
    send_response(
        pipe,
        &Response::Hello {
            version: logic::PROTOCOL_VERSION,
        },
    );

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
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
                        Ok(Some(request)) => dispatch(provider, pipe, &request),
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

/// Reconnecting client loop; intended to run on its own thread until `stop`
/// is set. `pipe` is the shared write endpoint handed to the credential so
/// `ReportResult` can report logon outcomes.
pub fn run(provider: UdsRpCredentialsProvider, pipe: SharedPipe, stop: Arc<AtomicBool>) {
    debug_flow!("pipe: client loop starting");
    while !stop.load(Ordering::Relaxed) {
        let Some(handle) = connect(&stop) else {
            break;
        };
        *pipe.lock().unwrap() = Some(handle.clone());
        session(handle, &provider, &pipe, &stop);
        pipe.lock().unwrap().take();
    }
    debug_flow!("pipe: client loop stopped");
}
