// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Client storage state (M8, model B): upload/download state machines for
//! the explicit file-transfer channel.
//!
//! One upload and one download may be active at a time (plenty for a
//! remote-desktop file drop; the wire protocol itself imposes no limit).
//! Chunks are produced by [`StorageClient::pump`], called from the session
//! loop's presentation tick, so a large transfer never starves display or
//! input: at most `PUMP_CHUNKS` chunks leave per tick. Incoming messages
//! are applied with [`StorageClient::handle`]; both return events for the
//! consumer and queue wire messages in [`StorageClient::out`], which the
//! session loop drains onto the reliable stream.
//!
//! Downloads land in a `.part` sibling and are renamed on `GetDone`, so a
//! cancelled or broken session never leaves a plausible-looking truncated
//! file behind.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use protocol::{StorageMessage, validate_name};
use tracing::{info, warn};

use crate::events::{ClientEvent, TransferDirection};

/// Chunks read and queued per pump tick (16 x 64 KiB per 8 ms tick is
/// ~128 MiB/s of headroom; the stream's backpressure sets the real rate).
pub const PUMP_CHUNKS: usize = 16;

/// Minimum spacing between [`ClientEvent::TransferProgress`] events per
/// transfer: enough for a smooth progress UI without flooding the event
/// channel (an integration can always coalesce further).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// One active upload (local file → server spool).
struct Upload {
    /// Wire transfer id.
    id: u32,
    /// Open local file.
    file: File,
    /// Spool leaf name.
    name: String,
    /// Total size in bytes.
    size: u64,
    /// Next offset to read.
    offset: u64,
    /// `PutDone` already queued (waiting for the server's `PutAck`).
    done_sent: bool,
    /// Last progress event emission (throttled to `PROGRESS_INTERVAL`).
    last_progress: Instant,
}

/// One active download (server spool → local file).
struct Download {
    /// Wire transfer id.
    id: u32,
    /// Open `.part` file being written (`None` once closed for commit).
    file: Option<File>,
    /// Temporary path of `file`.
    tmp_path: PathBuf,
    /// Final destination (renamed on completion).
    dest: PathBuf,
    /// Spool leaf name (for events).
    name: String,
    /// Bytes written so far (expected next chunk offset).
    received: u64,
    /// Total size when known from a previous spool listing (progress %).
    total: Option<u64>,
    /// Last progress event emission (throttled to `PROGRESS_INTERVAL`).
    last_progress: Instant,
}

impl Drop for Download {
    fn drop(&mut self) {
        // `tmp_path` is reset on commit, so this only fires for aborted,
        // failed or session-interrupted downloads.
        if !self.tmp_path.as_os_str().is_empty()
            && self.tmp_path.exists()
            && let Err(e) = std::fs::remove_file(&self.tmp_path)
        {
            warn!(path = %self.tmp_path.display(), "cannot remove partial download: {e}");
        }
    }
}

/// Client-side storage session state.
#[derive(Default)]
pub struct StorageClient {
    /// Next transfer id to assign (wraps; collisions are unrealistic at
    /// one transfer per direction at a time).
    next_id: u32,
    upload: Option<Upload>,
    download: Option<Download>,
    /// Entries collected since the last `ListRequest`, emitted on `ListDone`.
    list_buf: Vec<(String, u64)>,
    /// Last known spool listing as name → size: lets downloads report a
    /// progress percentage (the wire carries no size in `GetChunk`).
    spool_sizes: HashMap<String, u64>,
    /// Wire messages queued for sending (drained by the session loop).
    pub out: Vec<StorageMessage>,
}

impl StorageClient {
    /// Fresh, empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Emits an error event (helper keeping call sites terse).
    fn err_event(id: u32, msg: impl Into<String>) -> ClientEvent {
        ClientEvent::TransferError {
            transfer_id: id,
            message: msg.into(),
        }
    }

    /// `StorageUpload` command: validates the leaf name, opens the file
    /// and queues the `PutStart`. The transfer id is returned in the
    /// events.
    pub fn start_upload(&mut self, path: &Path) -> Vec<ClientEvent> {
        if self.upload.is_some() {
            return vec![Self::err_event(0, "an upload is already active")];
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            return vec![Self::err_event(
                0,
                format!("{}: no valid leaf name", path.display()),
            )];
        };
        if let Err(e) = validate_name(&name) {
            return vec![Self::err_event(0, format!("{name}: {e}"))];
        }
        let open = File::open(path).and_then(|f| f.metadata().map(|m| (f, m.len())));
        let (file, size) = match open {
            Ok(v) => v,
            Err(e) => return vec![Self::err_event(0, format!("{}: {e}", path.display()))],
        };
        if size > protocol::MAX_FILE_SIZE {
            return vec![Self::err_event(
                0,
                format!("{name}: exceeds the 4 GiB limit"),
            )];
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        info!(id, name, size, "upload started");
        self.out.push(StorageMessage::PutStart {
            transfer_id: id,
            name: name.clone(),
            size,
        });
        self.upload = Some(Upload {
            id,
            file,
            name,
            size,
            offset: 0,
            done_sent: false,
            last_progress: Instant::now(),
        });
        Vec::new()
    }

    /// `StorageDownload` command: validates the name, opens the `.part`
    /// file and queues the `GetStart` (offset 0; resume is a wire
    /// capability the simple consumer API does not expose yet).
    pub fn start_download(&mut self, name: &str, dest: &Path) -> Vec<ClientEvent> {
        if self.download.is_some() {
            return vec![Self::err_event(0, "a download is already active")];
        }
        if let Err(e) = validate_name(name) {
            return vec![Self::err_event(0, format!("{name}: {e}"))];
        }
        let tmp_path = dest.with_extension("part");
        match File::create(&tmp_path) {
            Ok(file) => {
                self.next_id = self.next_id.wrapping_add(1);
                let id = self.next_id;
                info!(id, name, dest = %dest.display(), "download started");
                self.out.push(StorageMessage::GetStart {
                    transfer_id: id,
                    name: name.to_owned(),
                    offset: 0,
                });
                self.download = Some(Download {
                    id,
                    file: Some(file),
                    tmp_path,
                    dest: dest.to_path_buf(),
                    name: name.to_owned(),
                    received: 0,
                    total: self.spool_sizes.get(name).copied(),
                    last_progress: Instant::now(),
                });
                Vec::new()
            }
            Err(e) => vec![Self::err_event(0, format!("{}: {e}", tmp_path.display()))],
        }
    }

    /// `StorageAbort` command: cancels whichever transfer carries the id.
    pub fn abort(&mut self, transfer_id: u32) {
        if self.upload.as_ref().is_some_and(|u| u.id == transfer_id) {
            self.upload = None;
            self.out.push(StorageMessage::PutAbort { transfer_id });
        }
        if self.download.as_ref().is_some_and(|d| d.id == transfer_id) {
            self.download = None; // Drop removes the .part file.
            self.out.push(StorageMessage::GetAbort { transfer_id });
        }
    }

    /// `StorageList` command: queues the request and resets the collector.
    pub fn list(&mut self) {
        self.list_buf.clear();
        self.spool_sizes.clear();
        self.out.push(StorageMessage::ListRequest);
    }

    /// Builds a throttled progress event: `Some` at most once per
    /// `PROGRESS_INTERVAL` per transfer.
    fn progress_event(
        last_progress: &mut Instant,
        transfer_id: u32,
        direction: TransferDirection,
        done_bytes: u64,
        total_bytes: Option<u64>,
    ) -> Option<ClientEvent> {
        if last_progress.elapsed() < PROGRESS_INTERVAL {
            return None;
        }
        *last_progress = Instant::now();
        Some(ClientEvent::TransferProgress {
            transfer_id,
            direction,
            done_bytes,
            total_bytes,
        })
    }

    /// Upload pump: reads up to `PUMP_CHUNKS` chunks from the active
    /// upload into the out queue. Returns events (read errors only;
    /// completion events come from the server's `PutAck`).
    pub fn pump(&mut self) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        for _ in 0..PUMP_CHUNKS {
            let Some(up) = self.upload.as_mut() else {
                break;
            };
            if up.offset >= up.size {
                if !up.done_sent {
                    up.done_sent = true;
                    self.out
                        .push(StorageMessage::PutDone { transfer_id: up.id });
                }
                // Keep the state until the server acks (or errors).
                break;
            }
            let want =
                usize::try_from((up.size - up.offset).min(protocol::STORAGE_CHUNK_SIZE as u64))
                    .unwrap_or(protocol::STORAGE_CHUNK_SIZE);
            let mut data = vec![0u8; want];
            match up.file.read(&mut data) {
                Ok(0) => {
                    // Local file truncated under us: abort.
                    let up = self.upload.take().expect("checked");
                    self.out
                        .push(StorageMessage::PutAbort { transfer_id: up.id });
                    events.push(Self::err_event(
                        up.id,
                        format!("{}: truncated while reading", up.name),
                    ));
                    break;
                }
                Ok(n) => {
                    data.truncate(n);
                    let offset = up.offset;
                    up.offset += n as u64;
                    self.out.push(StorageMessage::PutChunk {
                        transfer_id: up.id,
                        offset,
                        data,
                    });
                }
                Err(e) => {
                    let up = self.upload.take().expect("checked");
                    self.out
                        .push(StorageMessage::PutAbort { transfer_id: up.id });
                    events.push(Self::err_event(up.id, format!("{}: {e}", up.name)));
                    break;
                }
            }
        }
        if let Some(up) = self.upload.as_mut()
            && up.offset > 0
            && let Some(ev) = Self::progress_event(
                &mut up.last_progress,
                up.id,
                TransferDirection::Upload,
                up.offset,
                Some(up.size),
            )
        {
            events.push(ev);
        }
        events
    }

    /// Applies one server message; returns events for the consumer.
    pub fn handle(&mut self, msg: &StorageMessage) -> Vec<ClientEvent> {
        match msg {
            StorageMessage::PutAck { transfer_id } => {
                match self.upload.take() {
                    Some(up) if up.id == *transfer_id => {
                        info!(
                            transfer_id,
                            name = up.name,
                            bytes = up.size,
                            "upload committed"
                        );
                        vec![ClientEvent::TransferDone {
                            transfer_id: up.id,
                            direction: TransferDirection::Upload,
                            name: up.name,
                            bytes: up.size,
                        }]
                    }
                    // Ack for an aborted/unknown upload: put back whatever
                    // we took (a different active upload, if any) and ignore.
                    other => {
                        self.upload = other;
                        Vec::new()
                    }
                }
            }
            StorageMessage::GetChunk {
                transfer_id,
                offset,
                data,
            } => self.get_chunk(*transfer_id, *offset, data),
            StorageMessage::GetDone { transfer_id } => self.get_done(*transfer_id),
            StorageMessage::ListEntry { name, size } => {
                self.list_buf.push((name.clone(), *size));
                self.spool_sizes.insert(name.clone(), *size);
                Vec::new()
            }
            StorageMessage::ListDone => {
                vec![ClientEvent::StorageList {
                    entries: std::mem::take(&mut self.list_buf),
                }]
            }
            StorageMessage::Error {
                transfer_id,
                message,
                ..
            } => {
                warn!(transfer_id, message, "storage error from server");
                if self.upload.as_ref().is_some_and(|u| u.id == *transfer_id) {
                    self.upload = None;
                }
                if self.download.as_ref().is_some_and(|d| d.id == *transfer_id) {
                    self.download = None; // Drop removes the .part file.
                }
                vec![Self::err_event(*transfer_id, message.clone())]
            }
            other => {
                warn!(?other, "unexpected storage message from server");
                Vec::new()
            }
        }
    }

    /// One download chunk: written only when the offset chains (the
    /// reliable stream guarantees order, so a mismatch means a bug or a
    /// confused peer — abort defensively).
    fn get_chunk(&mut self, transfer_id: u32, offset: u64, data: &[u8]) -> Vec<ClientEvent> {
        let matches = self
            .download
            .as_ref()
            .is_some_and(|d| d.id == transfer_id && d.received == offset);
        if !matches {
            if self.download.as_ref().is_some_and(|d| d.id == transfer_id) {
                self.download = None; // Drop removes the .part file.
                self.out.push(StorageMessage::GetAbort { transfer_id });
                return vec![Self::err_event(
                    transfer_id,
                    "out-of-order chunk; download aborted",
                )];
            }
            return Vec::new();
        }
        let dl = self.download.as_mut().expect("checked");
        let file = dl.file.as_mut().expect("closed only at commit");
        match file.write_all(data) {
            Ok(()) => {
                dl.received += data.len() as u64;
                Self::progress_event(
                    &mut dl.last_progress,
                    dl.id,
                    TransferDirection::Download,
                    dl.received,
                    dl.total,
                )
                .into_iter()
                .collect()
            }
            Err(e) => {
                let dl = self.download.take().expect("checked");
                self.out.push(StorageMessage::GetAbort { transfer_id });
                vec![Self::err_event(transfer_id, format!("{}: {e}", dl.name))]
            }
        }
    }

    /// Download finished: flush, commit with a rename, report.
    fn get_done(&mut self, transfer_id: u32) -> Vec<ClientEvent> {
        let Some(mut dl) = self.download.take() else {
            return Vec::new();
        };
        if dl.id != transfer_id {
            self.download = Some(dl);
            return Vec::new();
        }
        let bytes = dl.received;
        let dest = dl.dest.clone();
        let name = dl.name.clone();
        let tmp = dl.tmp_path.clone();
        // Close the handle before the rename (Windows refuses to rename
        // open files) and disarm the Drop cleanup only if the rename
        // succeeded — a failed rename must still remove the .part file.
        drop(dl.file.take());
        match std::fs::rename(&tmp, &dest) {
            Ok(()) => {
                dl.tmp_path = PathBuf::new();
                info!(transfer_id, name, bytes, dest = %dest.display(), "download committed");
                vec![ClientEvent::TransferDone {
                    transfer_id,
                    direction: TransferDirection::Download,
                    name,
                    bytes,
                }]
            }
            Err(e) => vec![Self::err_event(
                transfer_id,
                format!("{}: {e}", dest.display()),
            )],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, data: &[u8]) {
        std::fs::write(path, data).expect("seed file");
    }

    #[test]
    fn upload_pump_produces_sequential_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("datos.bin");
        let payload: Vec<u8> = (0..150_000u32).map(|i| (i % 251) as u8).collect();
        write(&path, &payload);

        let mut c = StorageClient::new();
        assert_eq!(c.start_upload(&path), Vec::new());
        assert!(matches!(
            c.out.first(),
            Some(StorageMessage::PutStart { name, size, .. })
                if name == "datos.bin" && *size == payload.len() as u64
        ));
        c.out.clear();

        // Drain the pump until the PutDone is queued.
        let mut received = Vec::new();
        let mut expected = 0u64;
        loop {
            assert_eq!(c.pump(), Vec::new());
            let mut done = false;
            for msg in c.out.drain(..) {
                match msg {
                    StorageMessage::PutChunk { offset, data, .. } => {
                        assert_eq!(offset, expected);
                        expected += data.len() as u64;
                        received.extend_from_slice(&data);
                    }
                    StorageMessage::PutDone { .. } => done = true,
                    other => panic!("unexpected {other:?}"),
                }
            }
            if done {
                break;
            }
        }
        assert_eq!(received, payload);
    }

    #[test]
    fn upload_completes_on_put_ack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.bin");
        write(&path, b"hola");
        let mut c = StorageClient::new();
        let _ = c.start_upload(&path);
        let id = c.upload.as_ref().expect("active").id;
        let events = c.handle(&StorageMessage::PutAck { transfer_id: id });
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferDone {
                direction: TransferDirection::Upload,
                bytes: 4,
                ..
            }]
        ));
        assert!(c.upload.is_none());
    }

    #[test]
    fn download_cycle_commits_and_lists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("bajada.bin");
        let mut c = StorageClient::new();
        assert_eq!(c.start_download("remoto.bin", &dest), Vec::new());
        let id = c.download.as_ref().expect("active").id;
        c.out.clear();

        for (i, chunk) in [b"12".as_slice(), b"34", b"56"].iter().enumerate() {
            let events = c.handle(&StorageMessage::GetChunk {
                transfer_id: id,
                offset: (i * 2) as u64,
                data: chunk.to_vec(),
            });
            assert_eq!(events, Vec::new());
        }
        let events = c.handle(&StorageMessage::GetDone { transfer_id: id });
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferDone {
                direction: TransferDirection::Download,
                bytes: 6,
                ..
            }]
        ));
        assert_eq!(std::fs::read(&dest).expect("committed"), b"123456");
        // No .part left behind.
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1);

        // Listing collector.
        c.list();
        let _ = c.handle(&StorageMessage::ListEntry {
            name: "f".into(),
            size: 1,
        });
        let events = c.handle(&StorageMessage::ListDone);
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::StorageList { entries }] if entries.len() == 1
        ));
    }

    #[test]
    fn download_out_of_order_aborts_and_cleans() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("x.bin");
        let mut c = StorageClient::new();
        let _ = c.start_download("x.bin", &dest);
        let id = c.download.as_ref().expect("active").id;
        let events = c.handle(&StorageMessage::GetChunk {
            transfer_id: id,
            offset: 99,
            data: b"zz".to_vec(),
        });
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferError { .. }]
        ));
        assert!(c.download.is_none());
        assert!(matches!(
            c.out.last(),
            Some(StorageMessage::GetAbort { .. })
        ));
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn server_error_clears_transfers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("y.bin");
        let mut c = StorageClient::new();
        let _ = c.start_download("y.bin", &dest);
        let id = c.download.as_ref().expect("active").id;
        let events = c.handle(&StorageMessage::Error {
            transfer_id: id,
            code: protocol::ERR_NOT_FOUND,
            message: "not in the spool".into(),
        });
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferError { transfer_id: tid, .. }] if *tid == id
        ));
        assert!(c.download.is_none());
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn busy_rejects_second_transfer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.bin");
        write(&path, b"x");
        let mut c = StorageClient::new();
        assert_eq!(c.start_upload(&path), Vec::new());
        let events = c.start_upload(&path);
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferError { .. }]
        ));
        let events = c.start_download("a.bin", &dir.path().join("b.bin"));
        assert_eq!(events, Vec::new());
        let events = c.start_download("a.bin", &dir.path().join("c.bin"));
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferError { .. }]
        ));
    }
    #[test]
    fn upload_progress_is_throttled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.bin");
        write(&path, &vec![7u8; 300_000]);
        let mut c = StorageClient::new();
        assert_eq!(c.start_upload(&path), Vec::new());
        // First pump: no progress yet (interval not elapsed).
        assert_eq!(c.pump(), Vec::new());
        // Force the interval and pump again: exactly one progress event.
        c.upload.as_mut().expect("active").last_progress = Instant::now()
            .checked_sub(PROGRESS_INTERVAL + Duration::from_millis(1))
            .expect("test clock");
        let events = c.pump();
        let progress: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ClientEvent::TransferProgress { .. }))
            .collect();
        assert_eq!(progress.len(), 1);
        assert!(matches!(
            progress[0],
            ClientEvent::TransferProgress {
                direction: TransferDirection::Upload,
                done_bytes,
                total_bytes: Some(300_000),
                ..
            } if *done_bytes > 0
        ));
        // And the next pump is throttled again.
        assert_eq!(c.pump(), Vec::new());
    }

    #[test]
    fn download_progress_uses_the_spool_listing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut c = StorageClient::new();
        c.list();
        c.out.clear();
        c.handle(&StorageMessage::ListEntry {
            name: "y.bin".into(),
            size: 100,
        });
        c.handle(&StorageMessage::ListDone);
        let dest = dir.path().join("y.bin");
        assert_eq!(c.start_download("y.bin", &dest), Vec::new());
        let id = c.download.as_ref().expect("active").id;
        c.download.as_mut().expect("active").last_progress = Instant::now()
            .checked_sub(PROGRESS_INTERVAL + Duration::from_millis(1))
            .expect("test clock");
        let events = c.handle(&StorageMessage::GetChunk {
            transfer_id: id,
            offset: 0,
            data: vec![1u8; 40],
        });
        assert!(matches!(
            events.as_slice(),
            [ClientEvent::TransferProgress {
                direction: TransferDirection::Download,
                done_bytes: 40,
                total_bytes: Some(100),
                ..
            }]
        ));
        // A name absent from the listing reports an unknown total.
        let dest2 = dir.path().join("z.bin");
        let _ = dest2; // (no second download possible while one is active)
    }
}
