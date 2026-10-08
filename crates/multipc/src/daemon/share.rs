//! Showing a window of this PC on another PC ("moving" it there), and showing
//! other PCs' windows here.
//!
//! The program keeps running here; its window is captured about 15 times a
//! second and shown in a viewer on the other PC, which sends keyboard and mouse
//! back. Closing the viewer, or the window, ends the share.

use super::{Engine, Event};
use crate::platform::{Capture, ViewerEvent, WindowBackend};
use crate::transfer;
use mpc_core::protocol::Message;
use mpc_core::window::encode_frame;
use serde::Serialize;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const FRAME_EVERY: Duration = Duration::from_millis(66);
/// Re-send an unchanged picture this often, in case a frame was dropped.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

pub(super) struct ShareOut {
    pub peer: String,
    pub title: String,
    pub window: u64,
    task: tokio::task::AbortHandle,
}

#[derive(Serialize)]
pub struct ShareView {
    pub share: u64,
    pub peer: String,
    pub title: String,
}

impl Engine {
    /// Show `window` on `peer`. Returns an error message for the user.
    pub(super) fn start_share(&mut self, peer: String, window: u64) -> Result<(), String> {
        if !self.windows.supported() {
            return Err("this PC can't share windows".into());
        }
        let Some(p) = self.peers.get(&peer) else { return Err(format!("{peer} is not connected")) };
        if !p.hello.caps.windows {
            return Err(format!("{peer} can't show windows"));
        }
        if self.shares.values().any(|s| s.window == window && s.peer == peer) {
            return Ok(());
        }
        let title = self.windows.title(window).ok_or("the window is gone")?;
        let share = transfer::js_safe_id();
        let (w, h) = self.windows.list().iter().find(|i| i.id == window).map(|i| (i.width, i.height)).unwrap_or((800, 600));
        let _ = p.hi.try_send(Message::WindowOpen { share, title: title.clone(), width: w, height: h });
        let task = tokio::spawn(stream(self.windows.clone(), window, share, p.bulk.clone(), self.events.clone())).abort_handle();
        tracing::info!("showing {title:?} on {peer}");
        self.shares.insert(share, ShareOut { peer, title, window, task });
        Ok(())
    }

    /// End one of our shares and tell the viewing PC.
    pub(super) fn stop_share(&mut self, share: u64) {
        if let Some(s) = self.shares.remove(&share) {
            s.task.abort();
            if let Some(p) = self.peers.get(&s.peer) {
                let _ = p.hi.try_send(Message::WindowClose { share });
            }
        }
    }

    pub(super) fn on_window_message(&mut self, from: &str, msg: Message) {
        match msg {
            Message::WindowOpen { share, title, width, height } => {
                if self.windows.supported() && self.viewers.insert((from.to_string(), share)) {
                    self.windows.open_viewer(from, share, &title, width, height);
                }
            }
            Message::WindowFrame { share, jpeg, .. } => {
                if self.viewers.contains(&(from.to_string(), share)) {
                    self.windows.show_frame(from, share, jpeg);
                }
            }
            Message::WindowClose { share } => {
                // Either our viewer of their window, or their viewer of ours.
                if self.viewers.remove(&(from.to_string(), share)) {
                    self.windows.close_viewer(from, share);
                }
                if self.shares.get(&share).is_some_and(|s| s.peer == from) {
                    if let Some(s) = self.shares.remove(&share) {
                        s.task.abort();
                    }
                }
            }
            Message::WindowInput { share, ev } => {
                if let Some(s) = self.shares.get(&share).filter(|s| s.peer == from) {
                    self.windows.inject(s.window, &ev);
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_viewer(&mut self, ev: ViewerEvent) {
        match ev {
            ViewerEvent::Input { peer, share, ev } => {
                if let Some(p) = self.peers.get(&peer) {
                    let _ = p.hi.try_send(Message::WindowInput { share, ev });
                }
            }
            ViewerEvent::Closed { peer, share } => {
                if self.viewers.remove(&(peer.clone(), share)) {
                    if let Some(p) = self.peers.get(&peer) {
                        let _ = p.hi.try_send(Message::WindowClose { share });
                    }
                }
            }
        }
    }

    /// A peer disconnected: stop sharing with it and close its windows here.
    pub(super) fn windows_peer_gone(&mut self, peer: &str) {
        let ours: Vec<u64> = self.shares.iter().filter(|(_, s)| s.peer == peer).map(|(id, _)| *id).collect();
        for id in ours {
            self.stop_share(id);
        }
        let theirs: Vec<(String, u64)> = self.viewers.iter().filter(|(p, _)| p == peer).cloned().collect();
        for (p, share) in theirs {
            self.viewers.remove(&(p.clone(), share));
            self.windows.close_viewer(&p, share);
        }
    }

    pub(super) fn share_views(&self) -> Vec<ShareView> {
        let mut v: Vec<ShareView> =
            self.shares.iter().map(|(id, s)| ShareView { share: *id, peer: s.peer.clone(), title: s.title.clone() }).collect();
        v.sort_by(|a, b| a.title.cmp(&b.title));
        v
    }
}

enum Shot {
    Frame { hash: u64, width: u32, height: u32, jpeg: Vec<u8> },
    Unavailable,
    Gone,
}

fn shoot(windows: &dyn WindowBackend, window: u64, last_hash: u64) -> Shot {
    match windows.capture(window) {
        Capture::Gone => Shot::Gone,
        Capture::Unavailable => Shot::Unavailable,
        Capture::Frame(f) => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (f.width, f.height).hash(&mut h);
            f.bgra.hash(&mut h);
            let hash = h.finish();
            if hash == last_hash {
                // Unchanged: skip the expensive encoding.
                return Shot::Frame { hash, width: f.width, height: f.height, jpeg: Vec::new() };
            }
            match encode_frame(&f) {
                Ok(jpeg) => Shot::Frame { hash, width: f.width, height: f.height, jpeg },
                Err(_) => Shot::Unavailable,
            }
        }
    }
}

/// Capture the window and send pictures until it closes or the share is stopped.
async fn stream(
    windows: Arc<dyn WindowBackend>,
    window: u64,
    share: u64,
    bulk: mpsc::Sender<Message>,
    events: mpsc::UnboundedSender<Event>,
) {
    {
        let w = windows.clone();
        let _ = tokio::task::spawn_blocking(move || w.prepare(window)).await;
    }
    let mut tick = tokio::time::interval(FRAME_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut last_hash, mut last_jpeg, mut last_sent) = (0u64, Vec::new(), Instant::now());
    loop {
        tick.tick().await;
        let w = windows.clone();
        let Ok(shot) = tokio::task::spawn_blocking(move || shoot(&*w, window, last_hash)).await else { return };
        match shot {
            Shot::Gone => {
                let _ = events.send(Event::ShareEnded { share });
                return;
            }
            Shot::Unavailable => {}
            Shot::Frame { hash, width, height, jpeg } => {
                let changed = hash != last_hash;
                if !changed && last_sent.elapsed() < REFRESH_EVERY {
                    continue;
                }
                let jpeg = if changed { jpeg } else { last_jpeg.clone() };
                // A full queue means the network is behind: drop this picture, the
                // next one is newer anyway.
                if bulk.try_send(Message::WindowFrame { share, width, height, jpeg: jpeg.clone() }).is_ok() {
                    last_hash = hash;
                    last_jpeg = jpeg;
                    last_sent = Instant::now();
                } else if bulk.is_closed() {
                    return;
                }
            }
        }
    }
}
