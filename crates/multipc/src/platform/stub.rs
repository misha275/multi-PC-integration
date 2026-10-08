//! Fallback for systems without input support yet: files and clipboard still work.

use super::{Capture, InputBackend, LocalEvent, ViewerEvent, WindowBackend, WindowInfo};
use mpc_core::geometry::{Point, Rect};
use mpc_core::protocol::{InputEvent, WindowInputEvent};
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

pub struct NoWindows {
    _events: UnboundedSender<ViewerEvent>,
}

impl NoWindows {
    pub fn new(events: UnboundedSender<ViewerEvent>) -> Self {
        Self { _events: events }
    }
}

impl WindowBackend for NoWindows {
    fn supported(&self) -> bool {
        false
    }
    fn list(&self) -> Vec<WindowInfo> {
        Vec::new()
    }
    fn title(&self, _id: u64) -> Option<String> {
        None
    }
    fn dragged(&self) -> Option<u64> {
        None
    }
    fn prepare(&self, _id: u64) {}
    fn capture(&self, _id: u64) -> Capture {
        Capture::Gone
    }
    fn inject(&self, _id: u64, _ev: &WindowInputEvent) {}
    fn open_viewer(&self, _peer: &str, _share: u64, _title: &str, _width: u32, _height: u32) {}
    fn show_frame(&self, _peer: &str, _share: u64, _jpeg: Vec<u8>) {}
    fn close_viewer(&self, _peer: &str, _share: u64) {}
}
