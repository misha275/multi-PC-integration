//! File transfer: sending files to a peer and receiving files from it.

use anyhow::{anyhow, bail, Result};
use mpc_core::files::{self, sanitize_rel_path, unique_path};
use mpc_core::protocol::Message;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const CHUNK: usize = 60 * 1024;
const KEEP_HISTORY: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct TransferStatus {
    pub id: u64,
    pub peer: String,
    pub outgoing: bool,
    pub name: String,
    pub size: u64,
    pub done: u64,
    /// "active", "done" or "failed: <reason>".
    pub state: String,
    /// Where a received file was saved.
    pub saved_to: Option<String>,
}

/// Recent transfers, shown in the control panel.
#[derive(Clone, Default)]
pub struct Transfers(Arc<Mutex<VecDeque<TransferStatus>>>);

impl Transfers {
    fn add(&self, t: TransferStatus) {
        let mut q = self.0.lock().unwrap();
        q.push_back(t);
        while q.len() > KEEP_HISTORY {
            q.pop_front();
        }
    }

    fn update(&self, id: u64, outgoing: bool, f: impl FnOnce(&mut TransferStatus)) {
        let mut q = self.0.lock().unwrap();
        if let Some(t) = q.iter_mut().rev().find(|t| t.id == id && t.outgoing == outgoing) {
            f(t);
        }
    }

    pub fn snapshot(&self) -> Vec<TransferStatus> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

/// Random id that survives a round trip through JavaScript numbers (53 bits).
pub fn js_safe_id() -> u64 {
    random_id() & ((1 << 53) - 1)
}

pub fn random_id() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).expect("system random generator");
    u64::from_le_bytes(b)
}

/// Send files and folders to a peer over its bulk channel.
pub async fn send_paths(peer: String, paths: Vec<PathBuf>, tx: mpsc::Sender<Message>, transfers: Transfers) -> Result<()> {
    let list = tokio::task::spawn_blocking(move || files::collect(&paths)).await??;
    if list.is_empty() {
        bail!("nothing to send");
    }
    let batch = random_id();
    for f in list {
        let id = random_id();
        transfers.add(TransferStatus {
            id,
            peer: peer.clone(),
            outgoing: true,
            name: f.rel_path.clone(),
            size: f.size,
            done: 0,
            state: "active".into(),
            saved_to: None,
        });
        let result = send_one(id, batch, &f, &tx, &transfers).await;
        if let Err(e) = &result {
            let _ = tx.send(Message::FileAbort { id, reason: e.to_string() }).await;
        }
        transfers.update(id, true, |t| {
            t.state = match &result {
                Ok(()) => "done".into(),
                Err(e) => format!("failed: {e}"),
            }
        });
        result?;
    }
    Ok(())
}

async fn send_one(id: u64, batch: u64, f: &files::OutgoingFile, tx: &mpsc::Sender<Message>, transfers: &Transfers) -> Result<()> {
    let closed = || anyhow!("connection closed");
    let mut file = tokio::fs::File::open(&f.path).await?;
    tx.send(Message::FileOffer { id, batch, rel_path: f.rel_path.clone(), size: f.size }).await.map_err(|_| closed())?;
    let mut hasher = Sha256::new();
    let mut sent = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        sent += n as u64;
        tx.send(Message::FileChunk { id, data: buf[..n].to_vec() }).await.map_err(|_| closed())?;
        transfers.update(id, true, |t| t.done = sent);
    }
    if sent != f.size {
        bail!("file changed while sending");
    }
    tx.send(Message::FileEnd { id, sha256: hasher.finalize().into() }).await.map_err(|_| closed())?;
    Ok(())
}

struct IncomingFile {
    file: tokio::fs::File,
    part: PathBuf,
    target: PathBuf,
    size: u64,
    received: u64,
    hasher: Sha256,
}

/// Files being received from one peer. Lives in that peer's connection task.
pub struct Incoming {
    peer: String,
    dir: PathBuf,
    transfers: Transfers,
    files: HashMap<u64, IncomingFile>,
}

impl Incoming {
    pub fn new(peer: String, dir: PathBuf, transfers: Transfers) -> Self {
        Self { peer, dir, transfers, files: HashMap::new() }
    }

    /// Handle a file message. Returns a message to send back, if any.
    pub async fn handle(&mut self, msg: Message) -> Option<Message> {
        let id = match &msg {
            Message::FileOffer { id, .. } | Message::FileChunk { id, .. } | Message::FileEnd { id, .. } | Message::FileAbort { id, .. } => {
                *id
            }
            _ => return None,
        };
        match self.step(msg).await {
            Ok(()) => None,
            Err(e) => {
                tracing::warn!("receiving file from {} failed: {e:#}", self.peer);
                self.fail(id, &e.to_string()).await;
                Some(Message::FileAbort { id, reason: e.to_string() })
            }
        }
    }

    async fn step(&mut self, msg: Message) -> Result<()> {
        match msg {
            Message::FileOffer { id, rel_path, size, .. } => {
                let rel = sanitize_rel_path(&rel_path).ok_or_else(|| anyhow!("unsafe file name {rel_path:?}"))?;
                self.transfers.add(TransferStatus {
                    id,
                    peer: self.peer.clone(),
                    outgoing: false,
                    name: rel_path,
                    size,
                    done: 0,
                    state: "active".into(),
                    saved_to: None,
                });
                let target = unique_path(&self.dir, &rel);
                if let Some(parent) = target.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                let mut part = target.clone().into_os_string();
                part.push(".part");
                let part = PathBuf::from(part);
                let file = tokio::fs::File::create(&part).await?;
                self.files.insert(id, IncomingFile { file, part, target, size, received: 0, hasher: Sha256::new() });
            }
            Message::FileChunk { id, data } => {
                let f = self.files.get_mut(&id).ok_or_else(|| anyhow!("chunk for unknown file"))?;
                f.received += data.len() as u64;
                if f.received > f.size {
                    bail!("more data than announced");
                }
                f.hasher.update(&data);
                f.file.write_all(&data).await?;
                let done = f.received;
                self.transfers.update(id, false, |t| t.done = done);
            }
            Message::FileEnd { id, sha256 } => {
                let mut f = self.files.remove(&id).ok_or_else(|| anyhow!("end of unknown file"))?;
                f.file.flush().await?;
                drop(f.file);
                let ok = f.received == f.size && <[u8; 32]>::from(f.hasher.finalize()) == sha256;
                if !ok {
                    let _ = tokio::fs::remove_file(&f.part).await;
                    bail!("file arrived damaged");
                }
                // Another file may have taken the name meanwhile.
                let target = if f.target.exists() {
                    let name = PathBuf::from(f.target.file_name().unwrap_or_default());
                    unique_path(f.target.parent().unwrap_or(&self.dir), &name)
                } else {
                    f.target
                };
                tokio::fs::rename(&f.part, &target).await?;
                tracing::info!("received {} from {}", target.display(), self.peer);
                self.transfers.update(id, false, |t| {
                    t.state = "done".into();
                    t.saved_to = Some(target.display().to_string());
                });
            }
            Message::FileAbort { id, reason } => {
                self.fail(id, &format!("sender cancelled: {reason}")).await;
            }
            _ => {}
        }
        Ok(())
    }

    async fn fail(&mut self, id: u64, reason: &str) {
        if let Some(f) = self.files.remove(&id) {
            drop(f.file);
            let _ = tokio::fs::remove_file(&f.part).await;
        }
        self.transfers.update(id, false, |t| t.state = format!("failed: {reason}"));
    }

    /// Connection lost: delete unfinished files.
    pub async fn abort_all(&mut self) {
        let ids: Vec<u64> = self.files.keys().copied().collect();
        for id in ids {
            self.fail(id, "connection lost").await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_and_receive_folder() {
        let base = std::env::temp_dir().join(format!("mpc-transfer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src/Папка");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(src.join("big.bin"), &big).unwrap();
        std::fs::write(src.join("sub/empty.txt"), b"").unwrap();
        let dst = base.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("dummy"), b"").unwrap();

        let (tx, mut rx) = mpsc::channel(4);
        let sent = Transfers::default();
        let send = tokio::spawn(send_paths("b".into(), vec![src.clone()], tx, sent.clone()));
        let received = Transfers::default();
        let mut incoming = Incoming::new("a".into(), dst.clone(), received.clone());
        while let Some(msg) = rx.recv().await {
            assert!(incoming.handle(msg).await.is_none());
        }
        send.await.unwrap().unwrap();

        assert_eq!(std::fs::read(dst.join("Папка/big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(dst.join("Папка/sub/empty.txt")).unwrap(), b"");
        assert!(received.snapshot().iter().all(|t| t.state == "done"));
        assert!(sent.snapshot().iter().all(|t| t.state == "done" && t.done == t.size));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test]
    async fn rejects_path_escape_and_corruption() {
        let dst = std::env::temp_dir().join(format!("mpc-transfer-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dst).unwrap();
        let mut incoming = Incoming::new("a".into(), dst.clone(), Transfers::default());
        let reply = incoming.handle(Message::FileOffer { id: 1, batch: 1, rel_path: "../evil".into(), size: 1 }).await;
        assert!(matches!(reply, Some(Message::FileAbort { id: 1, .. })));

        incoming.handle(Message::FileOffer { id: 2, batch: 1, rel_path: "x.txt".into(), size: 3 }).await;
        incoming.handle(Message::FileChunk { id: 2, data: b"abc".to_vec() }).await;
        let reply = incoming.handle(Message::FileEnd { id: 2, sha256: [0; 32] }).await;
        assert!(matches!(reply, Some(Message::FileAbort { id: 2, .. })));
        assert!(!dst.join("x.txt").exists() && !dst.join("x.txt.part").exists());
        std::fs::remove_dir_all(&dst).unwrap();
    }
}
