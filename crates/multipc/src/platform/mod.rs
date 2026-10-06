//! Operating-system specific input capture and injection.

use mpc_core::geometry::{Point, Rect};
use mpc_core::protocol::{InputEvent, WindowInputEvent};
use mpc_core::window::Frame;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_share;

/// What the local keyboard and mouse did.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalEvent {
    /// The real cursor moved to this point while input stays on this machine.
    MouseAt(Point),
    /// Input captured for forwarding while the cursor is on another machine.
    Captured(InputEvent),
    /// The "come back" hotkey (Scroll Lock) was pressed while captured.
    ReturnHotkey,
}

pub trait InputBackend: Send + Sync {
    /// Whether this machine can capture and inject input at all.
    fn supported(&self) -> bool;
    fn monitors(&self) -> Vec<Rect>;
    fn cursor_pos(&self) -> Point;
    /// Start or stop capturing local input. While capturing, local keyboard and
    /// mouse events are swallowed and reported as [`LocalEvent::Captured`], and the
    /// cursor is parked at `park` so relative movement can be measured.
    fn set_capture(&self, capture: bool, park: Point);
    /// Move the cursor to an absolute local position.
    fn warp(&self, p: Point);
    fn inject(&self, ev: &InputEvent);
}

/// Start the platform backend; local events are delivered to `events`.
pub fn start(events: UnboundedSender<LocalEvent>) -> Box<dyn InputBackend> {
    #[cfg(windows)]
    {
        Box::new(windows::WindowsInput::start(events))
    }
    #[cfg(not(windows))]
    {
        Box::new(stub::NoInput::new(events))
    }
}

/// A window that can be shown on another PC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub enum Capture {
    Frame(Frame),
    /// The window exists but can't be captured right now (minimized).
    Unavailable,
    /// The window was closed.
    Gone,
}

/// What happened in a window showing another PC's window.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewerEvent {
    Input { peer: String, share: u64, ev: WindowInputEvent },
    Closed { peer: String, share: u64 },
}

/// Sharing windows with other PCs: capturing and controlling our own windows,
/// and showing windows of other PCs.
pub trait WindowBackend: Send + Sync {
    fn supported(&self) -> bool;
    fn list(&self) -> Vec<WindowInfo>;
    fn title(&self, id: u64) -> Option<String>;
    /// The window being dragged with the mouse right now, if any.
    fn dragged(&self) -> Option<u64>;
    /// Get a window ready to be shown elsewhere: end a drag in progress and keep
    /// it fully on its screen so input for it lands on it.
    fn prepare(&self, id: u64);
    fn capture(&self, id: u64) -> Capture;
    fn inject(&self, id: u64, ev: &WindowInputEvent);
    fn open_viewer(&self, peer: &str, share: u64, title: &str, width: u32, height: u32);
    fn show_frame(&self, peer: &str, share: u64, jpeg: Vec<u8>);
    fn close_viewer(&self, peer: &str, share: u64);
}

pub fn start_windows(events: UnboundedSender<ViewerEvent>) -> std::sync::Arc<dyn WindowBackend> {
    #[cfg(windows)]
    {
        std::sync::Arc::new(windows_share::WindowsShare::start(events))
    }
    #[cfg(not(windows))]
    {
        std::sync::Arc::new(stub::NoWindows::new(events))
    }
}
