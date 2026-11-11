// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Storage spool (M8, model B): the server side of the explicit
//! file-transfer channel.
//!
//! The spool is a plain directory. Clients upload files into it (chunked,
//! committed atomically via a temporary name + rename on `PutDone`) and
//! list/download what is in it (chunked pull, resumable by offset). The
//! server never reaches into the client's filesystem — there is no wire
//! message for that — and every name is a validated leaf
//! ([`protocol::validate_name`]), so the spool cannot be escaped.
//!
//! Pure `std::fs`, synchronous: chunk sizes are small (64 KiB) and the
//! session loop calls [`Spool::handle`] / [`Spool::pump`] with tight
//! budgets, so blocking time stays negligible.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;

use protocol::{StorageMessage, validate_name};
use tracing::{info, warn};

/// One in-progress upload (C→S).
struct Upload {
    /// Temporary file being written (`.part` inside the spool).
    file: File,
    /// Path of `file`.
    tmp_path: PathBuf,
    /// Final destination inside the spool (committed on `PutDone`).
    final_path: PathBuf,
    /// Declared total size.
    size: u64,
    /// Bytes received so far (also the expected next chunk offset).
    received: u64,
}

impl Drop for Upload {
    fn drop(&mut self) {
        // Aborted or session-ended uploads never leave partial files.
        if self.tmp_path.exists()
            && let Err(e) = fs::remove_file(&self.tmp_path)
        {
            warn!(path = %self.tmp_path.display(), "cannot remove partial upload: {e}");
        }
    }
}

/// One in-progress download (S→C).
struct Download {
    /// Transfer identifier chosen by the client.
    id: u32,
    /// Open file being read.
    file: File,
    /// Next offset to read.
    offset: u64,
    /// Total file size (chunk generation stops here).
    size: u64,
}

/// Server spool: directory root plus the active transfers.
pub struct Spool {
    root: PathBuf,
    uploads: HashMap<u32, Upload>,
    downloads: VecDeque<Download>,
    /// Quota in bytes (§18.78): committed files plus in-progress declared
    /// sizes may never exceed it, so a client cannot fill the disk.
    quota_bytes: u64,
    /// Bytes committed in the spool right now.
    used: u64,
    /// Bytes reserved by in-progress uploads (their declared sizes).
    pending: u64,
}

impl Spool {
    /// Opens (creating if needed) the spool directory with a quota in MiB
    /// (§18.78); existing files count against it from the start.
    pub fn open(root: PathBuf, quota_mb: u32) -> std::io::Result<Self> {
        fs::create_dir_all(&root)?;
        let used = fs::read_dir(&root)?
            .flatten()
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                let name = entry.file_name();
                (meta.is_file() && validate_name(name.to_str()?).is_ok()).then_some(meta.len())
            })
            .sum();
        Ok(Self {
            root,
            uploads: HashMap::new(),
            downloads: VecDeque::new(),
            quota_bytes: u64::from(quota_mb) * 1024 * 1024,
            used,
            pending: 0,
        })
    }

    /// Default spool location: `%USERPROFILE%\UDS-Transfers` on Windows,
    /// `$HOME/uds-transfers` elsewhere, falling back to the temp dir.
    #[must_use]
    pub fn default_root() -> PathBuf {
        let base = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, PathBuf::from);
        base.join(if cfg!(windows) {
            "UDS-Transfers"
        } else {
            "uds-transfers"
        })
    }

    /// True while downloads are waiting to be pumped.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.downloads.is_empty()
    }

    /// Maps an I/O error into a wire `Error` message.
    fn io_error(transfer_id: u32, e: &std::io::Error) -> StorageMessage {
        StorageMessage::Error {
            transfer_id,
            code: protocol::ERR_IO,
            message: e.to_string(),
        }
    }

    /// Handles one client message; returns the messages to send back
    /// immediately (chunk streams are produced by [`Spool::pump`] instead).
    pub fn handle(&mut self, msg: &StorageMessage) -> Vec<StorageMessage> {
        match msg {
            StorageMessage::ListRequest => self.list(),
            StorageMessage::PutStart {
                transfer_id,
                name,
                size,
            } => self.put_start(*transfer_id, name, *size),
            StorageMessage::PutChunk {
                transfer_id,
                offset,
                data,
            } => self.put_chunk(*transfer_id, *offset, data),
            StorageMessage::PutDone { transfer_id } => self.put_done(*transfer_id),
            StorageMessage::PutAbort { transfer_id } => {
                self.drop_upload(*transfer_id);
                Vec::new()
            }
            StorageMessage::GetStart {
                transfer_id,
                name,
                offset,
            } => self.get_start(*transfer_id, name, *offset),
            StorageMessage::GetAbort { transfer_id } => {
                self.downloads.retain(|d| d.id != *transfer_id);
                Vec::new()
            }
            other => {
                // S→C messages received from the client: protocol violation,
                // logged and ignored (never fatal to the session).
                warn!(?other, "unexpected storage message from client");
                Vec::new()
            }
        }
    }

    /// Sends up to `max_chunks` chunks of the oldest pending download.
    /// Called from the session loop's pump tick.
    pub fn pump(&mut self, max_chunks: usize) -> Vec<StorageMessage> {
        let mut out = Vec::new();
        for _ in 0..max_chunks {
            let Some(dl) = self.downloads.front_mut() else {
                break;
            };
            if dl.offset >= dl.size {
                let done = self.downloads.pop_front().expect("front checked");
                info!(transfer_id = done.id, "download finished");
                out.push(StorageMessage::GetDone {
                    transfer_id: done.id,
                });
                continue;
            }
            let want =
                usize::try_from((dl.size - dl.offset).min(protocol::STORAGE_CHUNK_SIZE as u64))
                    .unwrap_or(protocol::STORAGE_CHUNK_SIZE);
            let mut data = vec![0u8; want];
            match dl.file.read(&mut data) {
                Ok(0) => {
                    // Unexpected EOF (file truncated under us): finish.
                    let done = self.downloads.pop_front().expect("front checked");
                    out.push(StorageMessage::GetDone {
                        transfer_id: done.id,
                    });
                }
                Ok(n) => {
                    data.truncate(n);
                    let offset = dl.offset;
                    dl.offset += n as u64;
                    out.push(StorageMessage::GetChunk {
                        transfer_id: dl.id,
                        offset,
                        data,
                    });
                }
                Err(e) => {
                    let dl = self.downloads.pop_front().expect("front checked");
                    out.push(Self::io_error(dl.id, &e));
                }
            }
        }
        out
    }

    /// `ListRequest`: one `ListEntry` per valid file in the spool + `ListDone`.
    fn list(&self) -> Vec<StorageMessage> {
        let mut out = Vec::new();
        match fs::read_dir(&self.root) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let Ok(meta) = entry.metadata() else { continue };
                    if !meta.is_file() {
                        continue;
                    }
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    // Skip temp files and anything we would not serve.
                    if validate_name(&name).is_err() {
                        continue;
                    }
                    out.push(StorageMessage::ListEntry {
                        name,
                        size: meta.len(),
                    });
                }
            }
            Err(e) => out.push(Self::io_error(0, &e)),
        }
        out.push(StorageMessage::ListDone);
        out
    }

    /// `PutStart`: opens the temporary file for a new upload.
    fn put_start(&mut self, transfer_id: u32, name: &str, size: u64) -> Vec<StorageMessage> {
        if self.uploads.contains_key(&transfer_id) {
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_BAD_TRANSFER,
                message: "transfer id already in use".into(),
            }];
        }
        // Quota (§18.78): committed bytes plus every in-progress declared
        // size must leave room for this upload; the reservation is released
        // if the transfer aborts or errors out.
        if self.used + self.pending + size > self.quota_bytes {
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_QUOTA_EXCEEDED,
                message: "spool quota exceeded".into(),
            }];
        }
        let final_path = self.root.join(name);
        let tmp_path = self.root.join(format!(".{name}.{transfer_id}.part"));
        match File::create(&tmp_path) {
            Ok(file) => {
                self.pending += size;
                self.uploads.insert(
                    transfer_id,
                    Upload {
                        file,
                        tmp_path,
                        final_path,
                        size,
                        received: 0,
                    },
                );
                info!(transfer_id, name, size, "upload started");
                Vec::new()
            }
            Err(e) => vec![Self::io_error(transfer_id, &e)],
        }
    }

    /// Removes an upload and releases its quota reservation (Drop on the
    /// `Upload` cleans the `.part` file).
    fn drop_upload(&mut self, transfer_id: u32) {
        if let Some(up) = self.uploads.remove(&transfer_id) {
            self.pending = self.pending.saturating_sub(up.size);
        }
    }

    /// `PutChunk`: appends one chunk; offsets must be sequential.
    fn put_chunk(&mut self, transfer_id: u32, offset: u64, data: &[u8]) -> Vec<StorageMessage> {
        let Some(up) = self.uploads.get_mut(&transfer_id) else {
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_BAD_TRANSFER,
                message: "no active upload with this id".into(),
            }];
        };
        if offset != up.received {
            let got = up.received;
            self.drop_upload(transfer_id);
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_BAD_OFFSET,
                message: format!("expected offset {got}, got {offset}"),
            }];
        }
        if up.received + data.len() as u64 > up.size {
            self.drop_upload(transfer_id);
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_TOO_LARGE,
                message: "chunk exceeds the declared size".into(),
            }];
        }
        match up.file.write_all(data) {
            Ok(()) => {
                up.received += data.len() as u64;
                Vec::new()
            }
            Err(e) => {
                let msg = e.to_string();
                self.drop_upload(transfer_id);
                vec![StorageMessage::Error {
                    transfer_id,
                    code: protocol::ERR_IO,
                    message: msg,
                }]
            }
        }
    }

    /// `PutDone`: verifies the byte count and commits with an atomic rename.
    fn put_done(&mut self, transfer_id: u32) -> Vec<StorageMessage> {
        let Some(up) = self.uploads.remove(&transfer_id) else {
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_BAD_TRANSFER,
                message: "no active upload with this id".into(),
            }];
        };
        self.pending = self.pending.saturating_sub(up.size);
        if up.received != up.size {
            return vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_IO,
                message: format!("size mismatch: {} of {} bytes", up.received, up.size),
            }];
        }
        // Keep the Upload alive until the rename succeeded (Drop cleans tmp).
        let final_path = up.final_path.clone();
        match fs::rename(&up.tmp_path, &up.final_path) {
            Ok(()) => {
                self.used += up.size;
                drop(up); // tmp no longer exists: Drop has nothing to clean.
                info!(transfer_id, path = %final_path.display(), "upload committed");
                vec![StorageMessage::PutAck { transfer_id }]
            }
            Err(e) => vec![Self::io_error(transfer_id, &e)],
        }
    }

    /// `GetStart`: opens the spool file and queues the download; chunks
    /// flow from [`Spool::pump`]. An `offset` past the end is rejected, so
    /// resume requests for a file that shrank fail loudly.
    fn get_start(&mut self, transfer_id: u32, name: &str, offset: u64) -> Vec<StorageMessage> {
        let path = self.root.join(name);
        let open = File::open(&path).and_then(|mut f| {
            let size = f.metadata()?.len();
            if offset > size {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("resume offset {offset} past end of file ({size})"),
                ));
            }
            f.seek(SeekFrom::Start(offset))?;
            Ok((f, size))
        });
        match open {
            Ok((file, size)) => {
                info!(transfer_id, name, offset, size, "download started");
                self.downloads.push_back(Download {
                    id: transfer_id,
                    file,
                    offset,
                    size,
                });
                Vec::new()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_NOT_FOUND,
                message: format!("{name}: not in the spool"),
            }],
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => vec![StorageMessage::Error {
                transfer_id,
                code: protocol::ERR_BAD_OFFSET,
                message: e.to_string(),
            }],
            Err(e) => vec![Self::io_error(transfer_id, &e)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spool() -> (tempfile::TempDir, Spool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Spool::open(dir.path().to_path_buf(), 32).expect("spool");
        (dir, s)
    }

    /// Drains the pump until the download queue is empty.
    fn drain(s: &mut Spool) -> Vec<StorageMessage> {
        let mut all = Vec::new();
        while s.has_pending() {
            all.extend(s.pump(4));
        }
        all
    }

    #[test]
    fn upload_cycle_commits() {
        let (dir, mut s) = spool();
        let msgs = s.handle(&StorageMessage::PutStart {
            transfer_id: 1,
            name: "subida.bin".into(),
            size: 6,
        });
        assert_eq!(msgs, Vec::new());
        for (i, chunk) in [b"ab".as_slice(), b"cd", b"ef"].iter().enumerate() {
            let msgs = s.handle(&StorageMessage::PutChunk {
                transfer_id: 1,
                offset: (i * 2) as u64,
                data: chunk.to_vec(),
            });
            assert!(msgs.is_empty(), "chunk {i}: {msgs:?}");
        }
        let msgs = s.handle(&StorageMessage::PutDone { transfer_id: 1 });
        assert_eq!(msgs, vec![StorageMessage::PutAck { transfer_id: 1 }]);
        assert_eq!(
            fs::read(dir.path().join("subida.bin")).expect("committed"),
            b"abcdef"
        );
        // No temp file left behind.
        assert_eq!(fs::read_dir(dir.path()).expect("dir").count(), 1);
    }

    #[test]
    fn upload_bad_offset_aborts_and_cleans() {
        let (dir, mut s) = spool();
        let _ = s.handle(&StorageMessage::PutStart {
            transfer_id: 1,
            name: "x.bin".into(),
            size: 4,
        });
        let msgs = s.handle(&StorageMessage::PutChunk {
            transfer_id: 1,
            offset: 99,
            data: b"zz".to_vec(),
        });
        assert!(matches!(
            msgs.as_slice(),
            [StorageMessage::Error { code, .. }] if *code == protocol::ERR_BAD_OFFSET
        ));
        assert_eq!(fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn upload_size_mismatch_fails_on_done() {
        let (_dir, mut s) = spool();
        let _ = s.handle(&StorageMessage::PutStart {
            transfer_id: 1,
            name: "x.bin".into(),
            size: 10,
        });
        let _ = s.handle(&StorageMessage::PutChunk {
            transfer_id: 1,
            offset: 0,
            data: b"short".to_vec(),
        });
        let msgs = s.handle(&StorageMessage::PutDone { transfer_id: 1 });
        assert!(matches!(
            msgs.as_slice(),
            [StorageMessage::Error { code, .. }] if *code == protocol::ERR_IO
        ));
    }

    #[test]
    fn download_cycle_and_list() {
        let (dir, mut s) = spool();
        fs::write(dir.path().join("bajada.bin"), b"contenido").expect("seed");
        let list = s.handle(&StorageMessage::ListRequest);
        assert!(matches!(
            list.as_slice(),
            [StorageMessage::ListEntry { name, size }, StorageMessage::ListDone]
                if name == "bajada.bin" && *size == 9
        ));

        let msgs = s.handle(&StorageMessage::GetStart {
            transfer_id: 7,
            name: "bajada.bin".into(),
            offset: 0,
        });
        assert_eq!(msgs, Vec::new());
        let chunks = drain(&mut s);
        let mut data = Vec::new();
        let mut done = false;
        let mut expected = 0u64;
        for msg in chunks {
            match msg {
                StorageMessage::GetChunk {
                    offset, data: d, ..
                } => {
                    assert_eq!(offset, expected);
                    expected += d.len() as u64;
                    data.extend_from_slice(&d);
                }
                StorageMessage::GetDone { transfer_id } => {
                    assert_eq!(transfer_id, 7);
                    done = true;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(done);
        assert_eq!(data, b"contenido");
    }

    #[test]
    fn download_resume_from_offset() {
        let (dir, mut s) = spool();
        fs::write(dir.path().join("f.bin"), b"0123456789").expect("seed");
        let _ = s.handle(&StorageMessage::GetStart {
            transfer_id: 1,
            name: "f.bin".into(),
            offset: 5,
        });
        let chunks = drain(&mut s);
        let data: Vec<u8> = chunks
            .iter()
            .filter_map(|m| match m {
                StorageMessage::GetChunk { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(data, b"56789");
    }

    #[test]
    fn download_missing_and_bad_offset_fail() {
        let (dir, mut s) = spool();
        fs::write(dir.path().join("f.bin"), b"abc").expect("seed");
        let msgs = s.handle(&StorageMessage::GetStart {
            transfer_id: 1,
            name: "nope.bin".into(),
            offset: 0,
        });
        assert!(matches!(
            msgs.as_slice(),
            [StorageMessage::Error { code, .. }] if *code == protocol::ERR_NOT_FOUND
        ));
        let msgs = s.handle(&StorageMessage::GetStart {
            transfer_id: 1,
            name: "f.bin".into(),
            offset: 99,
        });
        assert!(matches!(
            msgs.as_slice(),
            [StorageMessage::Error { code, .. }] if *code == protocol::ERR_BAD_OFFSET
        ));
    }

    #[test]
    fn put_abort_cleans_temp() {
        let (dir, mut s) = spool();
        let _ = s.handle(&StorageMessage::PutStart {
            transfer_id: 1,
            name: "x.bin".into(),
            size: 100,
        });
        let _ = s.handle(&StorageMessage::PutChunk {
            transfer_id: 1,
            offset: 0,
            data: b"partial".to_vec(),
        });
        let msgs = s.handle(&StorageMessage::PutAbort { transfer_id: 1 });
        assert_eq!(msgs, Vec::new());
        assert_eq!(fs::read_dir(dir.path()).expect("dir").count(), 0);
    }

    #[test]
    fn quota_blocks_oversized_uploads() {
        // 1 MiB quota; existing files count against it from open.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("old.bin"), vec![0u8; 500 * 1024]).expect("seed");
        let mut s = Spool::open(dir.path().to_path_buf(), 1).expect("spool");
        // 400 KiB fits (500 + 400 < 1024).
        let msgs = s.handle(&StorageMessage::PutStart {
            transfer_id: 1,
            name: "ok.bin".into(),
            size: 400 * 1024,
        });
        assert_eq!(msgs, Vec::new());
        // A second one reserving past the quota is rejected.
        let msgs = s.handle(&StorageMessage::PutStart {
            transfer_id: 2,
            name: "big.bin".into(),
            size: 400 * 1024,
        });
        assert!(matches!(
            msgs.as_slice(),
            [StorageMessage::Error { code, .. }] if *code == protocol::ERR_QUOTA_EXCEEDED
        ));
        // Aborting the first releases the reservation; the retry fits.
        let _ = s.handle(&StorageMessage::PutAbort { transfer_id: 1 });
        let msgs = s.handle(&StorageMessage::PutStart {
            transfer_id: 2,
            name: "big.bin".into(),
            size: 400 * 1024,
        });
        assert_eq!(msgs, Vec::new());
    }
}
