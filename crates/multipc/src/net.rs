//! Networking: discovery beacons on the LAN, peer connections, per-peer tasks.

use crate::daemon::{Event, PeerHandle};
use crate::transfer::{Incoming, Transfers};
use anyhow::{bail, Context, Result};
use mpc_core::config::group_id;
use mpc_core::protocol::{Beacon, Hello, Message, PairMessage, Platform, PROTOCOL_VERSION};
use mpc_core::transport::{self, Kind, Psk, Session};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;

const BEACON_EVERY: Duration = Duration::from_secs(3);
const PING_EVERY: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(20);

pub struct NetCtx {
    psk: RwLock<Psk>,
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
        Arc::new(Self { psk: RwLock::new(psk), name, port, hello, events, download_dir, transfers, next_conn: AtomicU64::new(1) })
    }

    pub fn psk(&self) -> Psk {
        *self.psk.read().unwrap()
    }

    /// Switch to another group's key (after joining it).
    pub fn set_psk(&self, psk: Psk) {
        *self.psk.write().unwrap() = psk;
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
            let result = async {
                let mut stream = stream;
                let kind = tokio::time::timeout(Duration::from_secs(10), transport::read_kind(&mut stream)).await??;
                match kind {
                    Kind::Group => connection(ctx, stream, addr, false).await,
                    Kind::Pairing => pairing_incoming(ctx, stream, addr).await,
                }
            }
            .await;
            if let Err(e) = result {
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
    let psk = ctx.psk();
    let session = if initiator {
        tokio::time::timeout(Duration::from_secs(10), transport::connect(stream, Kind::Group, &psk)).await??
    } else {
        tokio::time::timeout(Duration::from_secs(10), transport::accept(stream, Kind::Group, &psk)).await??
    };
    let Session { mut reader, mut writer, .. } = session;

    let ours = ctx.hello.lock().unwrap().clone();
    writer.send(&Message::Hello(ours)).await?;
    let hello = match tokio::time::timeout(Duration::from_secs(10), reader.recv::<Message>()).await?? {
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
                r = tokio::time::timeout(READ_TIMEOUT, reader.recv::<Message>()) => r.context("peer stopped responding")??,
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

/// Someone asks to join our group. Ask the user (through the engine) and, if
/// they agree, hand over the group key on the encrypted pairing channel.
async fn pairing_incoming(ctx: Arc<NetCtx>, stream: TcpStream, addr: SocketAddr) -> Result<()> {
    let Session { mut reader, mut writer, code } =
        tokio::time::timeout(Duration::from_secs(10), transport::accept(stream, Kind::Pairing, &[0; 32])).await??;
    let (name, platform) = match tokio::time::timeout(Duration::from_secs(10), reader.recv::<PairMessage>()).await?? {
        PairMessage::Request { name, platform } => (name, platform),
        _ => bail!("unexpected pairing message"),
    };
    let id = crate::transfer::js_safe_id();
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel::<bool>();
    let _ = ctx.events.send(Event::PairIncoming { id, name: name.clone(), platform, addr, code, answer: answer_tx });
    let answer = tokio::select! {
        a = tokio::time::timeout(PAIR_TIMEOUT, answer_rx) => matches!(a, Ok(Ok(true))),
        // The asking side gave up or went away.
        _ = reader.recv::<PairMessage>() => false,
    };
    let _ = ctx.events.send(Event::PairIncomingDone { id });
    let reply = if answer {
        tracing::info!("{name} ({addr}) joins our group");
        PairMessage::Accept { name: ctx.name.clone(), key: hex::encode(ctx.psk()) }
    } else {
        PairMessage::Decline
    };
    writer.send(&reply).await?;
    Ok(())
}

pub const PAIR_TIMEOUT: Duration = Duration::from_secs(120);

/// How an outgoing join request is going.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PairProgress {
    Connecting,
    Waiting { code: String },
    Accepted { name: String },
    Declined,
    Failed { error: String },
}

/// Ask the machine at `addr` to let us into its group.
pub async fn pair_outgoing(ctx: Arc<NetCtx>, id: u64, addr: SocketAddr) {
    let report = |p: PairProgress| {
        let _ = ctx.events.send(Event::PairOutgoing { id, progress: p });
    };
    let result: Result<PairProgress> = async {
        let stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .context("no answer: is MultiPC running there, and allowed through the firewall?")??;
        let Session { mut reader, mut writer, code } =
            tokio::time::timeout(Duration::from_secs(10), transport::connect(stream, Kind::Pairing, &[0; 32])).await??;
        writer.send(&PairMessage::Request { name: ctx.name.clone(), platform: Platform::current() }).await?;
        report(PairProgress::Waiting { code });
        match tokio::time::timeout(PAIR_TIMEOUT + Duration::from_secs(5), reader.recv::<PairMessage>()).await {
            Err(_) => bail!("no answer in time"),
            Ok(Err(_)) => Ok(PairProgress::Declined),
            Ok(Ok(PairMessage::Accept { name, key })) => {
                let _ = ctx.events.send(Event::PairJoined { name: name.clone(), key, addr });
                Ok(PairProgress::Accepted { name })
            }
            Ok(Ok(_)) => Ok(PairProgress::Declined),
        }
    }
    .await;
    report(result.unwrap_or_else(|e| PairProgress::Failed { error: format!("{e:#}") }));
}

/// Broadcast addresses of every IPv4 network this PC is on, plus 255.255.255.255.
/// Sending to each one gets beacons out of every adapter (Wi-Fi and Ethernet,
/// VPNs, virtual adapters), which a single limited broadcast does not on Windows.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut v = vec![Ipv4Addr::BROADCAST];
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for i in ifs {
            if let if_addrs::IfAddr::V4(a) = i.addr {
                if a.ip.is_loopback() {
                    continue;
                }
                if let Some(b) = a.broadcast {
                    if !v.contains(&b) {
                        v.push(b);
                    }
                }
            }
        }
    }
    v
}

/// Announce ourselves on the LAN and report every MultiPC heard.
pub async fn discovery(ctx: Arc<NetCtx>) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ctx.port)).await?;
    socket.set_broadcast(true)?;
    let mut tick = tokio::time::interval(BEACON_EVERY);
    let mut targets = broadcast_targets();
    let mut rounds = 0u32;
    let mut buf = [0u8; 1024];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                rounds += 1;
                if rounds.is_multiple_of(10) {
                    targets = broadcast_targets(); // adapters come and go
                }
                let beacon = Beacon { name: ctx.name.clone(), port: ctx.port, platform: Platform::current(), group: group_id(&ctx.psk()) }.encode();
                for t in &targets {
                    let _ = socket.send_to(&beacon, (*t, ctx.port)).await;
                }
            }
            r = socket.recv_from(&mut buf) => {
                let Ok((n, src)) = r else { continue };
                if let Some(b) = Beacon::decode(&buf[..n]) {
                    if b.name != ctx.name {
                        let same_group = b.group == group_id(&ctx.psk());
                        let _ = ctx.events.send(Event::Beacon {
                            name: b.name,
                            addr: SocketAddr::new(src.ip(), b.port),
                            platform: b.platform,
                            same_group,
                        });
                    }
                }
            }
        }
    }
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
    fn broadcast_targets_include_limited_broadcast() {
        let t = broadcast_targets();
        assert_eq!(t[0], Ipv4Addr::BROADCAST);
        assert!(t.iter().all(|a| !a.is_loopback()));
    }

    #[tokio::test]
    async fn resolve_addresses() {
        assert_eq!(resolve("127.0.0.1", 47800).await, Some("127.0.0.1:47800".parse().unwrap()));
        assert_eq!(resolve("127.0.0.1:9000", 47800).await, Some("127.0.0.1:9000".parse().unwrap()));
    }
}
