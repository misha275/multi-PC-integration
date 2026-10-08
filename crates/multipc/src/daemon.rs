//! The engine: one task that owns all shared state (peers, layout, who has the
//! cursor) and reacts to events from the network, local input, clipboard and UI.

use crate::platform::{self, InputBackend, LocalEvent, ViewerEvent, WindowBackend, WindowInfo};
use crate::transfer::{self, TransferStatus, Transfers};
use crate::{clipboard, net, web};
use anyhow::Result;
use mpc_core::config::{self, Config};
use mpc_core::geometry::{Layout, Placement, Point, Rect, Step};
use mpc_core::protocol::{
    Capabilities, ClipboardData, FocusToken, Hello, InputEvent, Message, MouseButton, Platform, SharedLayout, PROTOCOL_VERSION,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

pub struct PeerHandle {
    pub name: String,
    pub conn_id: u64,
    /// Name of the machine that opened the connection; used to pick one when two
    /// machines connect to each other at the same time.
    pub initiator: String,
    pub addr: SocketAddr,
    pub hello: Hello,
    /// Latency-sensitive messages (input, focus).
    pub hi: mpsc::Sender<Message>,
    /// Clipboard and files; waits behind input.
    pub bulk: mpsc::Sender<Message>,
    pub abort: Option<oneshot::Sender<()>>,
}

pub enum Event {
    Connected(PeerHandle),
    Disconnected {
        name: String,
        conn_id: u64,
    },
    FromPeer {
        name: String,
        conn_id: u64,
        msg: Message,
    },
    Beacon {
        name: String,
        addr: SocketAddr,
        platform: Platform,
        same_group: bool,
    },
    /// Another machine asks to join our group; `answer` carries the user's choice.
    PairIncoming {
        id: u64,
        name: String,
        platform: Platform,
        addr: SocketAddr,
        code: String,
        answer: oneshot::Sender<bool>,
    },
    PairIncomingDone {
        id: u64,
    },
    PairOutgoing {
        id: u64,
        progress: net::PairProgress,
    },
    /// Our join request was accepted: switch to that group's key.
    PairJoined {
        name: String,
        key: String,
        addr: SocketAddr,
    },
    DialDone {
        key: String,
    },
    Local(LocalEvent),
    Clipboard(ClipboardData),
    Tick,
    Api(ApiRequest),
    /// Something happened in a window showing another PC's window.
    Viewer(ViewerEvent),
    /// Our shared window was closed or can't be captured any more.
    ShareEnded {
        share: u64,
    },
}

pub enum ApiRequest {
    State(oneshot::Sender<StateView>),
    ShareWindow {
        window: u64,
        peer: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    StopShare {
        share: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetPlacements(BTreeMap<String, Placement>, oneshot::Sender<()>),
    SendFiles {
        peer: String,
        paths: Vec<PathBuf>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Ask the machine at this address ("192.168.1.5" or a name seen on the LAN) to join.
    Pair {
        target: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    PairAnswer {
        id: u64,
        accept: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

/// A MultiPC heard on the LAN.
struct Discovered {
    addr: SocketAddr,
    platform: Platform,
    same_group: bool,
    seen: Instant,
}

struct IncomingPair {
    id: u64,
    name: String,
    platform: Platform,
    addr: SocketAddr,
    code: String,
    answer: Option<oneshot::Sender<bool>>,
}

#[derive(Serialize)]
pub struct DeviceView {
    pub name: String,
    pub address: String,
    pub platform: Platform,
    /// "connected", "group" (our group, connecting) or "other" (can be asked to join).
    pub status: &'static str,
}

#[derive(Serialize)]
pub struct PairRequestView {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub platform: Platform,
    pub code: String,
}

#[derive(Serialize)]
pub struct PairOutgoingView {
    pub id: u64,
    pub target: String,
    pub progress: net::PairProgress,
}

#[derive(Serialize)]
pub struct MachineView {
    pub name: String,
    pub is_self: bool,
    pub online: bool,
    pub platform: Platform,
    pub caps: Capabilities,
    pub address: Option<String>,
    pub monitors: Vec<Rect>,
    pub placement: Option<Placement>,
}

#[derive(Serialize)]
pub struct StateView {
    pub name: String,
    pub cursor_on: String,
    pub machines: Vec<MachineView>,
    pub transfers: Vec<TransferStatus>,
    pub download_dir: String,
    pub devices: Vec<DeviceView>,
    pub pair_requests: Vec<PairRequestView>,
    pub pair_outgoing: Vec<PairOutgoingView>,
    pub windows: bool,
    /// Our windows that can be shown on another PC.
    pub window_list: Vec<WindowInfo>,
    pub shares: Vec<share::ShareView>,
}

mod share;

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn sign(v: i32) -> i32 {
    v.signum()
}

struct Engine {
    cfg: Config,
    cfg_dir: PathBuf,
    name: String,
    caps: Capabilities,
    backend: Box<dyn InputBackend>,
    net: Arc<net::NetCtx>,
    events: mpsc::UnboundedSender<Event>,
    hello: Arc<Mutex<Hello>>,
    transfers: Transfers,
    clipboard: Option<std::sync::mpsc::Sender<ClipboardData>>,
    windows: Arc<dyn WindowBackend>,
    shares: HashMap<u64, share::ShareOut>,
    viewers: HashSet<(String, u64)>,

    peers: HashMap<String, PeerHandle>,
    dialing: HashSet<String>,
    monitors: Vec<Rect>,
    layout: SharedLayout,
    geom: Layout,
    focus: FocusToken,
    captured: bool,
    last_mouse: Option<Point>,
    /// Keys and buttons we forwarded as pressed, and to whom, so they can be
    /// released if the cursor leaves that machine mid-press.
    forwarded_to: Option<String>,
    pressed_keys: HashSet<(u16, bool, u16)>,
    pressed_buttons: HashSet<MouseButton>,

    discovered: BTreeMap<String, Discovered>,
    pair_incoming: Vec<IncomingPair>,
    pair_outgoing: Vec<PairOutgoingView>,
}

pub async fn run(cfg: Config) -> Result<()> {
    let cfg_dir = Config::dir();
    let psk = cfg.psk()?;
    let name = cfg.name.clone();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Event>();

    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<LocalEvent>();
    let backend = platform::start(local_tx);
    {
        let events = events_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = local_rx.recv().await {
                if events.send(Event::Local(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let (viewer_tx, mut viewer_rx) = mpsc::unbounded_channel::<ViewerEvent>();
    let windows = platform::start_windows(viewer_tx);
    {
        let events = events_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = viewer_rx.recv().await {
                if events.send(Event::Viewer(ev)).is_err() {
                    break;
                }
            }
        });
    }
    let caps = Capabilities {
        input: backend.supported() && cfg.share_input,
        clipboard: cfg.share_clipboard,
        files: true,
        drives: false,
        windows: windows.supported(),
    };
    let monitors = if caps.input { backend.monitors() } else { Vec::new() };
    let layout = config::load_layout(&cfg_dir);
    let focus = FocusToken { epoch: 0, owner: name.clone() };
    let hello = Arc::new(Mutex::new(Hello {
        name: name.clone(),
        protocol: PROTOCOL_VERSION,
        platform: Platform::current(),
        caps,
        monitors: monitors.clone(),
        layout: layout.clone(),
        focus: focus.clone(),
    }));

    let transfers = Transfers::default();
    let download_dir = cfg.download_dir();
    let net = net::NetCtx::new(psk, name.clone(), cfg.port, hello.clone(), events_tx.clone(), download_dir, transfers.clone());
    {
        let net = net.clone();
        tokio::spawn(async move {
            if let Err(e) = net::listen(net).await {
                tracing::error!("{e:#}");
                std::process::exit(1);
            }
        });
    }
    {
        let net = net.clone();
        tokio::spawn(async move {
            if let Err(e) = net::discovery(net).await {
                tracing::warn!("LAN discovery disabled: {e:#}");
            }
        });
    }
    web::start(cfg.control_port, cfg_dir.clone(), events_tx.clone()).await?;
    if cfg.open_panel {
        web::open_in_browser(&format!("http://127.0.0.1:{}", cfg.control_port));
    }
    let clipboard = if cfg.share_clipboard { clipboard::start(events_tx.clone()) } else { None };
    {
        let events = events_tx.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(2));
            loop {
                t.tick().await;
                if events.send(Event::Tick).is_err() {
                    break;
                }
            }
        });
    }

    let mut engine = Engine {
        cfg,
        cfg_dir,
        name,
        caps,
        backend,
        net,
        events: events_tx,
        hello,
        transfers,
        clipboard,
        windows,
        shares: HashMap::new(),
        viewers: HashSet::new(),
        peers: HashMap::new(),
        dialing: HashSet::new(),
        monitors,
        layout,
        geom: Layout::default(),
        focus,
        captured: false,
        last_mouse: None,
        forwarded_to: None,
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
        discovered: BTreeMap::new(),
        pair_incoming: Vec::new(),
        pair_outgoing: Vec::new(),
    };
    engine.rebuild_geometry();
    engine.ensure_placements();
    while let Some(ev) = events_rx.recv().await {
        engine.handle(ev);
    }
    Ok(())
}

impl Engine {
    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Connected(p) => self.on_connected(p),
            Event::Disconnected { name, conn_id } => {
                if self.peers.get(&name).is_some_and(|p| p.conn_id == conn_id) {
                    self.peers.remove(&name);
                    self.windows_peer_gone(&name);
                    self.rebuild_geometry();
                    self.apply_focus(None);
                }
            }
            Event::FromPeer { name, conn_id, msg } => {
                if self.peers.get(&name).is_some_and(|p| p.conn_id == conn_id) {
                    self.on_peer_message(&name, msg);
                }
            }
            Event::Beacon { name, addr, platform, same_group } => {
                self.discovered.insert(name.clone(), Discovered { addr, platform, same_group, seen: Instant::now() });
                // The machine with the smaller name dials, so each pair connects once.
                if same_group && self.name < name && !self.peers.contains_key(&name) {
                    self.dial(name, addr);
                }
            }
            Event::PairIncoming { id, name, platform, addr, code, answer } => {
                tracing::info!("{name} ({addr}) asks to join; code {code}");
                self.pair_incoming.retain(|p| p.name != name);
                self.pair_incoming.push(IncomingPair { id, name, platform, addr, code, answer: Some(answer) });
                if self.pair_incoming.len() > 5 {
                    self.pair_incoming.remove(0);
                }
            }
            Event::PairIncomingDone { id } => self.pair_incoming.retain(|p| p.id != id),
            Event::PairOutgoing { id, progress } => {
                if let Some(p) = self.pair_outgoing.iter_mut().find(|p| p.id == id) {
                    p.progress = progress;
                }
            }
            Event::PairJoined { name, key, addr } => self.join_group(&name, &key, addr),
            Event::DialDone { key } => {
                self.dialing.remove(&key);
            }
            Event::Local(ev) => self.on_local(ev),
            Event::Clipboard(data) => {
                for p in self.peers.values().filter(|p| p.hello.caps.clipboard) {
                    let _ = p.bulk.try_send(Message::Clipboard(data.clone()));
                }
            }
            Event::Tick => self.on_tick(),
            Event::Api(req) => self.on_api(req),
            Event::Viewer(ev) => self.on_viewer(ev),
            Event::ShareEnded { share } => self.stop_share(share),
        }
    }

    fn dial(&mut self, key: String, addr: SocketAddr) {
        if !self.dialing.insert(key.clone()) {
            return;
        }
        let net = self.net.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            if let Err(e) = net::dial(net, addr).await {
                tracing::debug!("connection to {addr}: {e:#}");
            }
            let _ = events.send(Event::DialDone { key });
        });
    }

    fn on_tick(&mut self) {
        self.discovered.retain(|_, d| d.seen.elapsed() < Duration::from_secs(12));
        if self.caps.input {
            let monitors = self.backend.monitors();
            if monitors != self.monitors && !monitors.is_empty() {
                tracing::info!("monitor configuration changed");
                self.monitors = monitors.clone();
                self.broadcast(|p| p.hello.caps.input, Message::Monitors(monitors));
                self.rebuild_geometry();
                self.ensure_placements();
            }
        }
        // Configured addresses, for networks where discovery broadcasts don't pass.
        for peer in self.cfg.peers.clone() {
            let key = format!("static:{peer}");
            if self.dialing.contains(&key) {
                continue;
            }
            let port = self.cfg.port;
            let connected: Vec<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
            let events = self.events.clone();
            let net = self.net.clone();
            self.dialing.insert(key.clone());
            tokio::spawn(async move {
                if let Some(addr) = net::resolve(&peer, port).await {
                    if !connected.iter().any(|c| c.ip() == addr.ip()) {
                        if let Err(e) = net::dial(net, addr).await {
                            tracing::debug!("connection to {peer}: {e:#}");
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
                let _ = events.send(Event::DialDone { key });
            });
        }
    }

    fn on_connected(&mut self, mut p: PeerHandle) {
        if let Some(existing) = self.peers.get_mut(&p.name) {
            // Both machines must keep the same connection: the one opened by the
            // machine with the smaller name.
            let preferred = self.name.clone().min(p.name.clone());
            if p.initiator == preferred && existing.initiator != preferred {
                if let Some(a) = existing.abort.take() {
                    let _ = a.send(());
                }
            } else {
                if let Some(a) = p.abort.take() {
                    let _ = a.send(());
                }
                return;
            }
        }
        if p.hello.layout.is_newer_than(&self.layout) {
            self.adopt_layout(p.hello.layout.clone());
        }
        // Catch up on the focus counter, but don't hand our cursor to whoever had it
        // in the group: a machine that just started keeps its own input.
        self.focus.epoch = self.focus.epoch.max(p.hello.focus.epoch);
        tracing::info!("{} joined ({:?})", p.name, p.hello.platform);
        self.peers.insert(p.name.clone(), p);
        self.rebuild_geometry();
        self.ensure_placements();
        self.apply_focus(None);
    }

    fn on_peer_message(&mut self, from: &str, msg: Message) {
        match msg {
            Message::Monitors(m) => {
                if let Some(p) = self.peers.get_mut(from) {
                    p.hello.monitors = m;
                }
                self.rebuild_geometry();
                self.ensure_placements();
            }
            Message::Layout(l) => {
                if l.is_newer_than(&self.layout) {
                    self.adopt_layout(l);
                    self.rebuild_geometry();
                }
            }
            Message::Focus { token, enter } => {
                if token.is_newer_than(&self.focus) {
                    let mine = token.owner == self.name;
                    self.focus = token;
                    self.apply_focus(if mine { enter } else { None });
                }
            }
            Message::Input(ev) => self.on_remote_input(from, ev),
            m @ (Message::WindowOpen { .. } | Message::WindowFrame { .. } | Message::WindowClose { .. } | Message::WindowInput { .. }) => {
                self.on_window_message(from, m)
            }
            Message::Clipboard(data) => {
                if let Some(c) = &self.clipboard {
                    let _ = c.send(data);
                }
            }
            _ => {}
        }
    }

    // ---- shared cursor -------------------------------------------------------

    /// Who really has the cursor: the focus owner, unless it went offline, in
    /// which case local input stays here.
    fn effective_owner(&self) -> String {
        let owner = &self.focus.owner;
        if *owner != self.name && self.peers.get(owner).is_some_and(|p| p.hello.caps.input) {
            owner.clone()
        } else {
            self.name.clone()
        }
    }

    fn park_point(&self) -> Point {
        self.monitors.iter().find(|m| m.contains(Point::new(0, 0))).or(self.monitors.first()).map(|m| m.center()).unwrap_or_default()
    }

    /// Move the cursor to `target`, appearing at `at` there, and tell everyone.
    fn give_focus(&mut self, target: String, at: Point) {
        self.focus = self.focus.next(target);
        tracing::debug!("cursor -> {} at {:?}", self.focus.owner, at);
        let msg = Message::Focus { token: self.focus.clone(), enter: Some(at) };
        self.broadcast(|p| p.hello.caps.input, msg);
        let mine = self.focus.owner == self.name;
        self.apply_focus(mine.then_some(at));
    }

    fn apply_focus(&mut self, enter: Option<Point>) {
        let owner = self.effective_owner();
        if self.forwarded_to.as_ref().is_some_and(|f| *f != owner) {
            self.release_forwarded();
        }
        let remote = owner != self.name;
        if remote != self.captured {
            self.captured = remote;
            if remote {
                tracing::info!("курсор и клавиатура теперь управляют ПК {owner}; вернуть их сюда: Scroll Lock");
            } else {
                tracing::info!("курсор и клавиатура снова на этом ПК");
            }
            self.backend.set_capture(remote, self.park_point());
        }
        if !remote {
            if let Some(at) = enter {
                self.backend.warp(at);
                self.last_mouse = Some(at);
            }
        }
        self.update_hello();
    }

    fn release_forwarded(&mut self) {
        let Some(to) = self.forwarded_to.take() else { return };
        let mut ups: Vec<InputEvent> =
            self.pressed_keys.drain().map(|(scan, extended, vk)| InputEvent::Key { scan, extended, vk, down: false }).collect();
        ups.extend(self.pressed_buttons.drain().map(|button| InputEvent::MouseButton { button, down: false }));
        if let Some(p) = self.peers.get(&to) {
            for ev in ups {
                let _ = p.hi.try_send(Message::Input(ev));
            }
        }
    }

    fn on_local(&mut self, ev: LocalEvent) {
        match ev {
            LocalEvent::MouseAt(p) => {
                if self.captured {
                    return;
                }
                let last = self.last_mouse.replace(p);
                let dir = last.map(|l| (sign(p.x - l.x), sign(p.y - l.y))).unwrap_or((0, 0));
                let probes: Vec<(i32, i32)> = if dir != (0, 0) {
                    vec![dir]
                } else {
                    // The mouse moved but the cursor didn't: it is pushed against a
                    // screen edge. Look past every edge it touches.
                    vec![(1, 0), (-1, 0), (0, 1), (0, -1)]
                };
                for (dx, dy) in probes {
                    if let Step::Cross { machine, at } = self.geom.step(&self.name, p, dx, dy) {
                        // A window dragged over the edge goes along to that PC.
                        if let Some(window) = self.windows.dragged() {
                            if let Err(e) = self.start_share(machine.clone(), window) {
                                tracing::info!("window not moved: {e}");
                            }
                        }
                        self.give_focus(machine, at);
                        return;
                    }
                }
            }
            LocalEvent::Captured(ev) => {
                if !self.captured {
                    return;
                }
                let owner = self.effective_owner();
                match ev {
                    InputEvent::Key { scan, extended, vk, down } => {
                        if down {
                            self.pressed_keys.insert((scan, extended, vk));
                        } else {
                            self.pressed_keys.remove(&(scan, extended, vk));
                        }
                    }
                    InputEvent::MouseButton { button, down } => {
                        if down {
                            self.pressed_buttons.insert(button);
                        } else {
                            self.pressed_buttons.remove(&button);
                        }
                    }
                    _ => {}
                }
                self.forwarded_to = Some(owner.clone());
                if let Some(p) = self.peers.get(&owner) {
                    let _ = p.hi.try_send(Message::Input(ev));
                }
            }
            LocalEvent::ReturnHotkey => {
                let at = self.park_point();
                self.give_focus(self.name.clone(), at);
            }
        }
    }

    fn on_remote_input(&mut self, from: &str, ev: InputEvent) {
        let owner = self.effective_owner();
        if owner != self.name {
            // The cursor moved on before the sender noticed: pass it along once.
            if owner != from {
                if let Some(p) = self.peers.get(&owner) {
                    let _ = p.hi.try_send(Message::Input(ev));
                }
            }
            return;
        }
        match ev {
            InputEvent::MouseMove { dx, dy } => {
                let cur = self.backend.cursor_pos();
                match self.geom.step(&self.name, cur, dx, dy) {
                    Step::Stay(p) => {
                        self.backend.warp(p);
                        self.last_mouse = Some(p);
                    }
                    Step::Cross { machine, at } => self.give_focus(machine, at),
                }
            }
            other => self.backend.inject(&other),
        }
    }

    // ---- layout --------------------------------------------------------------

    fn adopt_layout(&mut self, l: SharedLayout) {
        self.layout = l;
        if let Err(e) = config::save_layout(&self.cfg_dir, &self.layout) {
            tracing::warn!("saving layout: {e:#}");
        }
        self.update_hello();
    }

    fn input_machines(&self) -> Vec<(String, Vec<Rect>)> {
        let mut v = Vec::new();
        if self.caps.input {
            v.push((self.name.clone(), self.monitors.clone()));
        }
        for p in self.peers.values().filter(|p| p.hello.caps.input) {
            v.push((p.name.clone(), p.hello.monitors.clone()));
        }
        v
    }

    fn rebuild_geometry(&mut self) {
        let mut geom = Layout::default();
        for (name, monitors) in self.input_machines() {
            if let (Some(pl), false) = (self.layout.placements.get(&name), monitors.is_empty()) {
                geom.insert(name, monitors, *pl);
            }
        }
        self.geom = geom;
    }

    /// Give a position to machines that have none yet. Only the machine with the
    /// smallest name does this, so everyone ends up with the same layout.
    fn ensure_placements(&mut self) {
        let machines = self.input_machines();
        let Some(min) = machines.iter().map(|(n, _)| n).min() else { return };
        if *min != self.name {
            return;
        }
        let mut missing: Vec<_> = machines.into_iter().filter(|(n, m)| !m.is_empty() && !self.layout.placements.contains_key(n)).collect();
        if missing.is_empty() {
            return;
        }
        missing.sort_by(|a, b| a.0.cmp(&b.0));
        let mut layout = self.layout.clone();
        for (name, monitors) in missing {
            let pl = self.geom.next_free_placement();
            layout.placements.insert(name.clone(), pl);
            self.geom.insert(name, monitors, pl);
        }
        self.publish_layout(layout);
    }

    fn publish_layout(&mut self, mut layout: SharedLayout) {
        layout.version = now_ms().max(self.layout.version + 1);
        layout.author = self.name.clone();
        self.adopt_layout(layout.clone());
        self.broadcast(|_| true, Message::Layout(layout));
        self.rebuild_geometry();
    }

    // ---- helpers -------------------------------------------------------------

    fn broadcast(&self, filter: impl Fn(&PeerHandle) -> bool, msg: Message) {
        for p in self.peers.values().filter(|p| filter(p)) {
            let _ = p.hi.try_send(msg.clone());
        }
    }

    fn update_hello(&self) {
        let mut h = self.hello.lock().unwrap();
        h.monitors = self.monitors.clone();
        h.layout = self.layout.clone();
        h.focus = self.focus.clone();
    }

    /// Our join request was accepted: take the other group's key, drop the old
    /// group's connections and reconnect with the new key.
    fn join_group(&mut self, via: &str, key: &str, addr: SocketAddr) {
        let psk = match config::parse_key(key) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("{via} sent an invalid key: {e:#}");
                return;
            }
        };
        tracing::info!("joined the group of {via}");
        self.cfg.key = hex::encode(psk);
        if let Err(e) = self.cfg.save_to(&self.cfg_dir.join("config.toml")) {
            tracing::warn!("saving settings: {e:#}");
        }
        self.net.set_psk(psk);
        for p in self.peers.values_mut() {
            if let Some(a) = p.abort.take() {
                let _ = a.send(());
            }
        }
        // Beacons heard so far carry the old group's verdicts; let new ones decide.
        self.discovered.clear();
        // Connect right away instead of waiting for the next beacon.
        self.dial(format!("joined:{via}"), addr);
    }

    fn start_pair(&mut self, target: String, reply: oneshot::Sender<Result<(), String>>) {
        let known = self.discovered.get(&target).map(|d| d.addr);
        let id = transfer::js_safe_id();
        self.pair_outgoing.retain(|p| p.target != target);
        self.pair_outgoing.push(PairOutgoingView { id, target: target.clone(), progress: net::PairProgress::Connecting });
        let (net, port, events) = (self.net.clone(), self.cfg.port, self.events.clone());
        tokio::spawn(async move {
            let addr = match known {
                Some(a) => Some(a),
                None => net::resolve(&target, port).await,
            };
            match addr {
                Some(addr) => net::pair_outgoing(net, id, addr).await,
                None => {
                    let error = format!("address {target} not found");
                    let _ = events.send(Event::PairOutgoing { id, progress: net::PairProgress::Failed { error } });
                }
            }
        });
        let _ = reply.send(Ok(()));
    }

    fn on_api(&mut self, req: ApiRequest) {
        match req {
            ApiRequest::ShareWindow { window, peer, reply } => {
                let _ = reply.send(self.start_share(peer, window));
            }
            ApiRequest::StopShare { share, reply } => {
                let known = self.shares.contains_key(&share);
                self.stop_share(share);
                let _ = reply.send(if known { Ok(()) } else { Err("not shared any more".into()) });
            }
            ApiRequest::Pair { target, reply } => self.start_pair(target, reply),
            ApiRequest::PairAnswer { id, accept, reply } => {
                let answer = self.pair_incoming.iter_mut().find(|p| p.id == id).and_then(|p| p.answer.take());
                let result = match answer {
                    Some(tx) => tx.send(accept).map_err(|_| "the request is no longer active".to_string()),
                    None => Err("the request is no longer active".to_string()),
                };
                self.pair_incoming.retain(|p| p.id != id);
                let _ = reply.send(result);
            }
            ApiRequest::State(reply) => {
                let _ = reply.send(self.state_view());
            }
            ApiRequest::SetPlacements(placements, reply) => {
                let mut layout = self.layout.clone();
                layout.placements.extend(placements);
                self.publish_layout(layout);
                let _ = reply.send(());
            }
            ApiRequest::SendFiles { peer, paths, reply } => {
                let Some(p) = self.peers.get(&peer).filter(|p| p.hello.caps.files) else {
                    let _ = reply.send(Err(format!("{peer} is not connected")));
                    return;
                };
                let (bulk, transfers) = (p.bulk.clone(), self.transfers.clone());
                tokio::spawn(async move {
                    if let Err(e) = transfer::send_paths(peer.clone(), paths, bulk, transfers).await {
                        tracing::warn!("sending to {peer}: {e:#}");
                    }
                });
                let _ = reply.send(Ok(()));
            }
        }
    }

    fn state_view(&self) -> StateView {
        let mut machines = vec![MachineView {
            name: self.name.clone(),
            is_self: true,
            online: true,
            platform: Platform::current(),
            caps: self.caps,
            address: None,
            monitors: self.monitors.clone(),
            placement: self.layout.placements.get(&self.name).copied(),
        }];
        for p in self.peers.values() {
            machines.push(MachineView {
                name: p.name.clone(),
                is_self: false,
                online: true,
                platform: p.hello.platform,
                caps: p.hello.caps,
                address: Some(p.addr.ip().to_string()),
                monitors: p.hello.monitors.clone(),
                placement: self.layout.placements.get(&p.name).copied(),
            });
        }
        machines[1..].sort_by(|a, b| a.name.cmp(&b.name));
        StateView {
            name: self.name.clone(),
            cursor_on: self.effective_owner(),
            machines,
            transfers: self.transfers.snapshot(),
            download_dir: self.cfg.download_dir().display().to_string(),
            devices: self.devices_view(),
            pair_requests: self
                .pair_incoming
                .iter()
                .map(|p| PairRequestView {
                    id: p.id,
                    name: p.name.clone(),
                    address: p.addr.ip().to_string(),
                    platform: p.platform,
                    code: p.code.clone(),
                })
                .collect(),
            pair_outgoing: self
                .pair_outgoing
                .iter()
                .map(|p| PairOutgoingView { id: p.id, target: p.target.clone(), progress: p.progress.clone() })
                .collect(),
            windows: cfg!(windows),
            window_list: if self.windows.supported() { self.windows.list() } else { Vec::new() },
            shares: self.share_views(),
        }
    }

    fn devices_view(&self) -> Vec<DeviceView> {
        let mut v: Vec<DeviceView> = self
            .peers
            .values()
            .map(|p| DeviceView { name: p.name.clone(), address: p.addr.ip().to_string(), platform: p.hello.platform, status: "connected" })
            .collect();
        for (name, d) in &self.discovered {
            if !self.peers.contains_key(name) {
                v.push(DeviceView {
                    name: name.clone(),
                    address: d.addr.ip().to_string(),
                    platform: d.platform,
                    status: if d.same_group { "group" } else { "other" },
                });
            }
        }
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        cursor: Point,
        captured: bool,
        injected: Vec<InputEvent>,
    }

    #[derive(Clone, Default)]
    struct FakeBackend(Arc<Mutex<Fake>>);

    impl InputBackend for FakeBackend {
        fn supported(&self) -> bool {
            true
        }
        fn monitors(&self) -> Vec<Rect> {
            vec![Rect::new(0, 0, 1920, 1080)]
        }
        fn cursor_pos(&self) -> Point {
            self.0.lock().unwrap().cursor
        }
        fn set_capture(&self, capture: bool, park: Point) {
            let mut f = self.0.lock().unwrap();
            f.captured = capture;
            if capture {
                f.cursor = park;
            }
        }
        fn warp(&self, p: Point) {
            self.0.lock().unwrap().cursor = p;
        }
        fn inject(&self, ev: &InputEvent) {
            self.0.lock().unwrap().injected.push(*ev);
        }
    }

    #[derive(Default)]
    struct FakeWindows {
        injected: Mutex<Vec<(u64, mpc_core::protocol::WindowInputEvent)>>,
        viewers: Mutex<Vec<(String, u64, bool)>>,
        dragged: Mutex<Option<u64>>,
    }

    impl WindowBackend for FakeWindows {
        fn supported(&self) -> bool {
            true
        }
        fn list(&self) -> Vec<WindowInfo> {
            vec![WindowInfo { id: 42, title: "Блокнот".into(), width: 640, height: 480 }]
        }
        fn title(&self, id: u64) -> Option<String> {
            (id == 42).then(|| "Блокнот".into())
        }
        fn dragged(&self) -> Option<u64> {
            *self.dragged.lock().unwrap()
        }
        fn prepare(&self, _id: u64) {}
        fn capture(&self, _id: u64) -> platform::Capture {
            platform::Capture::Unavailable
        }
        fn inject(&self, id: u64, ev: &mpc_core::protocol::WindowInputEvent) {
            self.injected.lock().unwrap().push((id, *ev));
        }
        fn open_viewer(&self, peer: &str, share: u64, _title: &str, _w: u32, _h: u32) {
            self.viewers.lock().unwrap().push((peer.into(), share, true));
        }
        fn show_frame(&self, _peer: &str, _share: u64, _jpeg: Vec<u8>) {}
        fn close_viewer(&self, peer: &str, share: u64) {
            self.viewers.lock().unwrap().push((peer.into(), share, false));
        }
    }

    fn hello(name: &str, input: bool) -> Hello {
        Hello {
            name: name.into(),
            protocol: PROTOCOL_VERSION,
            platform: Platform::Windows,
            caps: Capabilities { input, clipboard: true, files: true, drives: false, windows: input },
            monitors: if input { vec![Rect::new(0, 0, 1920, 1080)] } else { vec![] },
            layout: SharedLayout::default(),
            focus: FocusToken::default(),
        }
    }

    fn engine(backend: FakeBackend) -> Engine {
        engine_w(backend, Arc::new(FakeWindows::default()))
    }

    fn engine_w(backend: FakeBackend, windows: Arc<FakeWindows>) -> Engine {
        let (events, _rx) = mpsc::unbounded_channel();
        let name = "alpha".to_string();
        let mut layout = SharedLayout { version: 1, author: name.clone(), ..Default::default() };
        layout.placements.insert("alpha".into(), Placement { x: 0, y: 0 });
        layout.placements.insert("beta".into(), Placement { x: 1920, y: 0 });
        let h = hello(&name, true);
        let hello = Arc::new(Mutex::new(h));
        let dir = std::env::temp_dir().join(format!("mpc-engine-test-{}-{}", std::process::id(), transfer::random_id()));
        let net = net::NetCtx::new([0; 32], name.clone(), 0, hello.clone(), events.clone(), dir.clone(), Transfers::default());
        let mut e = Engine {
            cfg: Config { name: name.clone(), ..Default::default() },
            cfg_dir: dir,
            name: name.clone(),
            caps: Capabilities { input: true, clipboard: true, files: true, drives: false, windows: true },
            backend: Box::new(backend.clone()),
            net,
            events,
            hello,
            transfers: Transfers::default(),
            clipboard: None,
            windows,
            shares: HashMap::new(),
            viewers: HashSet::new(),
            peers: HashMap::new(),
            dialing: HashSet::new(),
            monitors: backend.monitors(),
            layout,
            geom: Layout::default(),
            focus: FocusToken { epoch: 0, owner: name },
            captured: false,
            last_mouse: None,
            forwarded_to: None,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            discovered: BTreeMap::new(),
            pair_incoming: Vec::new(),
            pair_outgoing: Vec::new(),
        };
        e.rebuild_geometry();
        e
    }

    fn connect(e: &mut Engine, name: &str, conn_id: u64, input: bool) -> mpsc::Receiver<Message> {
        let (hi, rx) = mpsc::channel(64);
        let (bulk, _bulk_rx) = mpsc::channel(64);
        e.handle(Event::Connected(PeerHandle {
            name: name.into(),
            conn_id,
            initiator: "alpha".into(),
            addr: "127.0.0.1:1".parse().unwrap(),
            hello: hello(name, input),
            hi,
            bulk,
            abort: None,
        }));
        rx
    }

    fn drain(rx: &mut mpsc::Receiver<Message>) -> Vec<Message> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn cursor_crosses_to_peer_and_input_follows() {
        let be = FakeBackend::default();
        let mut e = engine(be.clone());
        let mut beta = connect(&mut e, "beta", 1, true);
        drain(&mut beta);

        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1918, 500))));
        assert!(drain(&mut beta).is_empty());
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1919, 500))));
        let msgs = drain(&mut beta);
        assert_eq!(msgs, [Message::Focus { token: FocusToken { epoch: 1, owner: "beta".into() }, enter: Some(Point::new(0, 500)) }]);
        assert!(be.0.lock().unwrap().captured);

        let key = InputEvent::Key { scan: 0x1e, extended: false, vk: 0x41, down: true };
        e.handle(Event::Local(LocalEvent::Captured(InputEvent::MouseMove { dx: 4, dy: -2 })));
        e.handle(Event::Local(LocalEvent::Captured(key)));
        assert_eq!(drain(&mut beta), [Message::Input(InputEvent::MouseMove { dx: 4, dy: -2 }), Message::Input(key)]);

        // beta sends the cursor back while A is still held: A must be released there.
        e.handle(Event::FromPeer {
            name: "beta".into(),
            conn_id: 1,
            msg: Message::Focus { token: FocusToken { epoch: 2, owner: "alpha".into() }, enter: Some(Point::new(1919, 700)) },
        });
        assert_eq!(drain(&mut beta), [Message::Input(InputEvent::Key { scan: 0x1e, extended: false, vk: 0x41, down: false })]);
        let f = be.0.lock().unwrap();
        assert!(!f.captured);
        assert_eq!(f.cursor, Point::new(1919, 700));
    }

    #[test]
    fn remote_mouse_moves_cursor_here_and_crosses_back() {
        let be = FakeBackend::default();
        let mut e = engine(be.clone());
        let mut beta = connect(&mut e, "beta", 1, true);
        be.warp(Point::new(1900, 300));

        let input = |e: &mut Engine, ev| e.handle(Event::FromPeer { name: "beta".into(), conn_id: 1, msg: Message::Input(ev) });
        input(&mut e, InputEvent::MouseMove { dx: 10, dy: 5 });
        assert_eq!(be.cursor_pos(), Point::new(1910, 305));
        input(&mut e, InputEvent::MouseButton { button: MouseButton::Left, down: true });
        assert_eq!(be.0.lock().unwrap().injected, [InputEvent::MouseButton { button: MouseButton::Left, down: true }]);

        drain(&mut beta);
        input(&mut e, InputEvent::MouseMove { dx: 15, dy: 0 });
        assert_eq!(
            drain(&mut beta),
            [Message::Focus { token: FocusToken { epoch: 1, owner: "beta".into() }, enter: Some(Point::new(5, 305)) }]
        );
        assert_eq!(e.effective_owner(), "beta");
    }

    #[test]
    fn owner_going_offline_returns_input_and_scroll_lock_reclaims() {
        let be = FakeBackend::default();
        let mut e = engine(be.clone());
        let mut beta = connect(&mut e, "beta", 1, true);
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1910, 10))));
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1919, 10))));
        assert!(be.0.lock().unwrap().captured);

        e.handle(Event::Local(LocalEvent::ReturnHotkey));
        assert!(!be.0.lock().unwrap().captured);
        assert!(matches!(drain(&mut beta).last(), Some(Message::Focus { token, .. }) if token.owner == "alpha"));

        // Cross again, then beta disconnects: input must come back here.
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1910, 10))));
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1919, 10))));
        assert!(be.0.lock().unwrap().captured);
        e.handle(Event::Disconnected { name: "beta".into(), conn_id: 1 });
        assert!(!be.0.lock().unwrap().captured);
    }

    #[test]
    fn joining_does_not_steal_the_cursor() {
        let be = FakeBackend::default();
        let mut e = engine(be.clone());
        let (hi, _rx) = mpsc::channel(64);
        let (bulk, _b) = mpsc::channel(64);
        let mut h = hello("beta", true);
        h.focus = FocusToken { epoch: 7, owner: "beta".into() };
        e.handle(Event::Connected(PeerHandle {
            name: "beta".into(),
            conn_id: 1,
            initiator: "alpha".into(),
            addr: "127.0.0.1:1".parse().unwrap(),
            hello: h,
            hi,
            bulk,
            abort: None,
        }));
        assert_eq!(e.effective_owner(), "alpha");
        assert!(!be.0.lock().unwrap().captured);
        // The next transfer outranks everything the group has seen.
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1910, 10))));
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1919, 10))));
        assert_eq!(e.focus, FocusToken { epoch: 8, owner: "beta".into() });
    }

    #[test]
    fn join_requests_are_listed_and_answered() {
        let mut e = engine(FakeBackend::default());
        let (answer, mut answered) = oneshot::channel();
        e.handle(Event::PairIncoming {
            id: 9,
            name: "КУХНЯ".into(),
            platform: Platform::Windows,
            addr: "192.168.1.31:47800".parse().unwrap(),
            code: "123 456".into(),
            answer,
        });
        let view = e.state_view();
        assert_eq!(view.pair_requests.len(), 1);
        assert_eq!(view.pair_requests[0].code, "123 456");

        let (reply, mut result) = oneshot::channel();
        e.handle(Event::Api(ApiRequest::PairAnswer { id: 9, accept: true, reply }));
        assert_eq!(result.try_recv().unwrap(), Ok(()));
        assert!(answered.try_recv().unwrap());
        assert!(e.state_view().pair_requests.is_empty());

        // Answering twice is refused.
        let (reply, mut result) = oneshot::channel();
        e.handle(Event::Api(ApiRequest::PairAnswer { id: 9, accept: true, reply }));
        assert!(result.try_recv().unwrap().is_err());
    }

    #[test]
    fn devices_from_other_groups_are_listed_but_not_dialed() {
        let mut e = engine(FakeBackend::default());
        e.handle(Event::Beacon {
            name: "zeta".into(),
            addr: "192.168.1.9:47800".parse().unwrap(),
            platform: Platform::Windows,
            same_group: false,
        });
        assert!(e.dialing.is_empty());
        let view = e.devices_view();
        assert_eq!(view.len(), 1);
        assert_eq!((view[0].name.as_str(), view[0].status), ("zeta", "other"));
    }

    #[tokio::test]
    async fn dragging_a_window_over_the_edge_shows_it_on_the_other_pc() {
        let fw = Arc::new(FakeWindows::default());
        let mut e = engine_w(FakeBackend::default(), fw.clone());
        let mut beta = connect(&mut e, "beta", 1, true);
        drain(&mut beta);
        *fw.dragged.lock().unwrap() = Some(42);
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1910, 500))));
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1919, 500))));
        let msgs = drain(&mut beta);
        let share = match &msgs[0] {
            Message::WindowOpen { share, title, width, height } => {
                assert_eq!((title.as_str(), *width, *height), ("Блокнот", 640, 480));
                *share
            }
            other => panic!("expected WindowOpen, got {other:?}"),
        };
        assert!(matches!(msgs[1], Message::Focus { .. }), "the cursor goes along");
        assert_eq!(e.share_views().len(), 1);

        // Input from the viewer reaches the original window.
        let ev = mpc_core::protocol::WindowInputEvent::MouseButton { button: MouseButton::Left, down: true, x: 10, y: 20 };
        e.handle(Event::FromPeer { name: "beta".into(), conn_id: 1, msg: Message::WindowInput { share, ev } });
        assert_eq!(*fw.injected.lock().unwrap(), [(42, ev)]);

        // Closing the viewer there ends the share here.
        e.handle(Event::FromPeer { name: "beta".into(), conn_id: 1, msg: Message::WindowClose { share } });
        assert!(e.share_views().is_empty());
    }

    #[tokio::test]
    async fn windows_from_other_pcs_open_here_and_close_with_their_pc() {
        let fw = Arc::new(FakeWindows::default());
        let mut e = engine_w(FakeBackend::default(), fw.clone());
        let mut beta = connect(&mut e, "beta", 1, true);
        drain(&mut beta);
        let open = Message::WindowOpen { share: 7, title: "Paint".into(), width: 800, height: 600 };
        e.handle(Event::FromPeer { name: "beta".into(), conn_id: 1, msg: open });
        assert_eq!(*fw.viewers.lock().unwrap(), [("beta".to_string(), 7, true)]);

        let ev = mpc_core::protocol::WindowInputEvent::Key { scan: 0x1e, extended: false, vk: 0x41, down: true };
        e.handle(Event::Viewer(ViewerEvent::Input { peer: "beta".into(), share: 7, ev }));
        assert_eq!(drain(&mut beta), [Message::WindowInput { share: 7, ev }]);

        e.handle(Event::Disconnected { name: "beta".into(), conn_id: 1 });
        assert_eq!(fw.viewers.lock().unwrap().last(), Some(&("beta".to_string(), 7, false)));
        assert!(e.viewers.is_empty());
    }

    #[test]
    fn phones_are_not_part_of_the_screen_layout() {
        let be = FakeBackend::default();
        let mut e = engine(be.clone());
        let mut phone = connect(&mut e, "phone", 1, false);
        assert!(!e.geom.machines.contains_key("phone"));
        // A phone gets clipboard and files but never cursor focus messages.
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1910, 10))));
        e.handle(Event::Local(LocalEvent::MouseAt(Point::new(1919, 10))));
        assert!(drain(&mut phone).iter().all(|m| !matches!(m, Message::Focus { .. })));
    }

    #[test]
    fn new_machine_gets_a_placement_from_the_smallest_name() {
        let be = FakeBackend::default();
        let mut e = engine(be);
        let mut gamma = connect(&mut e, "gamma", 1, true);
        let placement = e.layout.placements.get("gamma").copied();
        assert_eq!(placement, Some(Placement { x: 1920, y: 0 }), "right of alpha (beta is offline)");
        assert!(drain(&mut gamma).iter().any(|m| matches!(m, Message::Layout(l) if l.placements.contains_key("gamma"))));
    }
}
