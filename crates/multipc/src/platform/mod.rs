//! Operating-system specific input capture and injection.

use mpc_core::geometry::{Point, Rect};
use mpc_core::protocol::InputEvent;
use tokio::sync::mpsc::UnboundedSender;

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod windows;

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
