//! Socket handoff (§18.32): rebuilds in this process a TCP socket that the
//! orchestrator service accepted, validated and duplicated with
//! `WSADuplicateSocketW`, so the session agent continues the connection the
//! service established.

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use windows::Win32::Networking::WinSock::{
    AF_INET, IPPROTO_TCP, SOCK_STREAM, WSA_FLAG_OVERLAPPED, WSAPROTOCOL_INFOW, WSASocketW,
    WSAStartup, WSADATA,
};

/// Rebuilds the connected socket from the opaque `WSAPROTOCOL_INFO` blob the
/// service passed in the agent config frame.
pub fn reconstruct_tcp(blob: &[u8]) -> Result<TcpStream> {
    if blob.len() != size_of::<WSAPROTOCOL_INFOW>() {
        bail!(
            "socket info blob is {} bytes, expected {}",
            blob.len(),
            size_of::<WSAPROTOCOL_INFOW>()
        );
    }
    // WSAPROTOCOL_INFOW is a plain POD struct; the blob was produced by
    // WSADuplicateSocketW in the service for this exact process.
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: `info` is sized exactly as the blob (checked above) and the
    // struct has no padding-dependent invariants we rely on: the OS only
    // reads the documented fields.
    unsafe {
        std::ptr::copy_nonoverlapping(
            blob.as_ptr(),
            &mut info as *mut WSAPROTOCOL_INFOW as *mut u8,
            blob.len(),
        );
    }

    // WSASocket requires WSAStartup to have run in this process.
    let mut data = WSADATA::default();
    // SAFETY: `data` is a valid out-pointer; WSAStartup is process-global
    // and safe to call more than once (refcounted).
    let rc = unsafe { WSAStartup(0x0202, &mut data) };
    if rc != 0 {
        bail!("WSAStartup failed: {rc}");
    }

    // SAFETY: `info` describes a socket duplicated into this process by the
    // service; WSA_FLAG_OVERLAPPED matches how tokio uses sockets.
    let socket = unsafe {
        WSASocketW(
            AF_INET.0 as i32,
            SOCK_STREAM.0,
            IPPROTO_TCP.0,
            Some(&info),
            0,
            WSA_FLAG_OVERLAPPED,
        )
    }
    .context("WSASocketW from duplicated info")?;

    // Into std then tokio. The std TcpStream takes ownership of the socket.
    use std::os::windows::io::FromRawSocket;
    // SAFETY: `socket` is a live, connected socket owned by this process.
    let std_stream = unsafe { std::net::TcpStream::from_raw_socket(socket.0 as u64) };
    std_stream.set_nonblocking(true).context("set_nonblocking")?;
    TcpStream::from_std(std_stream).context("tokio TcpStream from std")
}
