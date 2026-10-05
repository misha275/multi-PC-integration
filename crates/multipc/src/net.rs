//! Networking: discovery beacons on the LAN, peer connections, per-peer tasks.

use crate::daemon::{Event, PeerHandle};
use crate::transfer::{Incoming, Transfers};
use anyhow::{bail, Context, Result};
use mpc_core::config::group_id;
use mpc_core::protocol::{Hello, Message, PROTOCOL_VERSION};
use mpc_core::transport::{handshake, Psk};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

const BEACON_MAGIC: &[u8; 4] = b"MPCB";
const BEACON_EVERY: Duration = Duration::from_secs(3);
const PING_EVERY: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(20);

pub struct NetCtx {
    pub psk: Psk,
    pub name: String,
    pub port: u16,
    /// Our current Hello, kept up to date by the engine.
    pub hello: Arc<Mutex<Hello>>,
    pub events: mpsc::UnboundedSender<Event>,
    pub download_dir: PathBuf,
    pub transfers: Transfers,
    next_conn: AtomicU64,
}

impl NetCtx {
    pub fn new(
        psk: Psk,
        name: String,
        port: u16,
        hello: Arc<Mutex<Hello>>,
        events: mpsc::UnboundedSender<Event>,
        download_dir: PathBuf,
        transfers: Transfers,
    ) -> Arc<Self> {
        Arc::new(Self { psk, name, port, hello, events, download_dir, transfers, next_conn: AtomicU64::new(1) })
    }
}

pub async fn listen(ctx: Arc<NetCtx>) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, ctx.port))
        .await
        .with_context(|| format!("port {} is busy (is MultiPC already running?)", ctx.port))?;
    tracing::info!("listening for peers on port {}", ctx.port);
    loop {
        let (stream, addr) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(ctx, stream, addr, false).await {
                tracing::debug!("incoming connection from {addr}: {e:#}");
            }
        });
    }
}

pub async fn dial(ctx: Arc<NetCtx>, addr: SocketAddr) -> Result<()> {
    let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await??;
    connection(ctx, stream, addr, true).await
}

async fn connection(ctx: Arc<NetCtx>, stream: TcpStream, addr: SocketAddr, initiator: bool) -> Result<()> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = tokio::time::timeout(Duration::from_secs(10), handshake(stream, &ctx.psk, initiator)).await??;

    let ours = ctx.hello.lock().unwrap().clone();
    writer.send(&Message::Hello(ours)).await?;
    let hello = match tokio::time::timeout(Duration::from_secs(10), reader.recv()).await?? {
        Message::Hello(h) => h,
        _ => bail!("peer did not introduce itself"),
    };
    if hello.protocol != PROTOCOL_VERSION {
        bail!("{} speaks protocol {}, we speak {}", hello.name, hello.protocol, PROTOCOL_VERSION);
    }
    if hello.name == ctx.name {
        bail!("peer uses our own name {:?}; give each PC a different name", ctx.name);
    }
    let name = hello.name.clone();
    let conn_id = ctx.next_conn.fetch_add(1, Ordering::Relaxed);
    let initiator_name = if initiator { ctx.name.clone() } else { name.clone() };
    tracing::info!("connected to {name} ({addr})");

    let (hi_tx, mut hi_rx) = mpsc::channel::<Message>(1024);
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Message>(16);
    let writer_task = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        let mut seq = 0u64;
        loop {
            let msg = tokio::select! {
                biased;
                Some(m) = hi_rx.recv() => m,
                Some(m) = bulk_rx.recv() => m,
                _ = ping.tick() => { seq += 1; Message::Ping(seq) }
                else => break,
            };
            if writer.send(&msg).await.is_err() {
                break;
            }
        }
    });

    let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<()>();
    let _ = ctx.events.send(Event::Connected(PeerHandle {
        name: name.clone(),
        conn_id,
        initiator: initiator_name,
        addr,
        hello,
        hi: hi_tx.clone(),
        bulk: bulk_tx,
        abort: Some(abort_tx),
    }));

    let mut incoming = Incoming::new(name.clone(), ctx.download_dir.clone(), ctx.transfers.clone());
    let result: Result<()> = async {
        loop {
            let msg = tokio::select! {
                r = tokio::time::timeout(READ_TIMEOUT, reader.recv()) => r.context("peer stopped responding")??,
                _ = &mut abort_rx => return Ok(()),
            };
            match msg {
                Message::Ping(n) => {
                    let _ = hi_tx.try_send(Message::Pong(n));
                }
                Message::Pong(_) | Message::Hello(_) => {}
                m @ (Message::FileOffer { .. } | Message::FileChunk { .. } | Message::FileEnd { .. } | Message::FileAbort { .. }) => {
                    if let Some(reply) = incoming.handle(m).await {
                        let _ = hi_tx.send(reply).await;
                    }
                }
                m => {
                    let _ = ctx.events.send(Event::FromPeer { name: name.clone(), conn_id, msg: m });
                }
            }
        }
    }
    .await;

    incoming.abort_all().await;
    writer_task.abort();
    let _ = ctx.events.send(Event::Disconnected { name: name.clone(), conn_id });
    tracing::info!("disconnected from {name}");
    result
}

/// Announce ourselves on the LAN and report other machines of our group.
pub async fn discovery(ctx: Arc<NetCtx>) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ctx.port)).await?;
    socket.set_broadcast(true)?;
    let group = group_id(&ctx.psk);
    let mut beacon = Vec::new();
    beacon.extend_from_slice(BEACON_MAGIC);
    beacon.extend_from_slice(&group);
    beacon.extend_from_slice(&ctx.port.to_be_bytes());
    beacon.extend_from_slice(ctx.name.as_bytes());

    let mut tick = tokio::time::interval(BEACON_EVERY);
    let mut buf = [0u8; 512];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let _ = socket.send_to(&beacon, (Ipv4Addr::BROADCAST, ctx.port)).await;
            }
            r = socket.recv_from(&mut buf) => {
                let Ok((n, src)) = r else { continue };
                if let Some((name, port)) = parse_beacon(&buf[..n], &group) {
                    if name != ctx.name {
                        let _ = ctx.events.send(Event::Beacon { name, addr: SocketAddr::new(src.ip(), port) });
                    }
                }
            }
        }
    }
}

fn parse_beacon(data: &[u8], group: &[u8; 8]) -> Option<(String, u16)> {
    if data.len() < 15 || &data[..4] != BEACON_MAGIC || &data[4..12] != group {
        return None;
    }
    let port = u16::from_be_bytes([data[12], data[13]]);
    let name = std::str::from_utf8(&data[14..]).ok()?.to_string();
    Some((name, port))
}

/// "host", "host:port" or "ip:port" to a socket address.
pub async fn resolve(peer: &str, default_port: u16) -> Option<SocketAddr> {
    let with_port = if peer.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        peer.to_string()
    } else {
        format!("{peer}:{default_port}")
    };
    tokio::net::lookup_host(with_port).await.ok()?.find(|a| a.is_ipv4())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_parsing() {
        let group = [1u8; 8];
        let mut b = b"MPCB".to_vec();
        b.extend_from_slice(&group);
        b.extend_from_slice(&47800u16.to_be_bytes());
        b.extend_from_slice("ПК-1".as_bytes());
        assert_eq!(parse_beacon(&b, &group), Some(("ПК-1".into(), 47800)));
        assert_eq!(parse_beacon(&b, &[2u8; 8]), None);
        assert_eq!(parse_beacon(&b[..10], &group), None);
    }

    #[tokio::test]
    async fn resolve_addresses() {
        assert_eq!(resolve("127.0.0.1", 47800).await, Some("127.0.0.1:47800".parse().unwrap()));
        assert_eq!(resolve("127.0.0.1:9000", 47800).await, Some("127.0.0.1:9000".parse().unwrap()));
    }
}
