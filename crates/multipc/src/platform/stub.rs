//! Fallback for systems without input support yet: files and clipboard still work.

use super::{InputBackend, LocalEvent};
use mpc_core::geometry::{Point, Rect};
use mpc_core::protocol::InputEvent;
use tokio::sync::mpsc::UnboundedSender;

pub struct NoInput {
    _events: UnboundedSender<LocalEvent>,
}

impl NoInput {
    pub fn new(events: UnboundedSender<LocalEvent>) -> Self {
        Self { _events: events }
    }
}

impl InputBackend for NoInput {
    fn supported(&self) -> bool {
        false
    }
    fn monitors(&self) -> Vec<Rect> {
        Vec::new()
    }
    fn cursor_pos(&self) -> Point {
        Point::default()
    }
    fn set_capture(&self, _capture: bool, _park: Point) {}
    fn warp(&self, _p: Point) {}
    fn inject(&self, _ev: &InputEvent) {}
}
