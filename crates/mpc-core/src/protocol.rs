//! Messages exchanged between MultiPC peers.
//!
//! Every message travels inside an encrypted session (see [`crate::transport`]).
//! The protocol is the same for every platform; a peer announces what it can do
//! in [`Hello::caps`], so an Android phone simply never receives input events.

use crate::geometry::{Placement, Point, Rect};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u32 = 3;
/// TCP port for peer connections and UDP port for discovery beacons.
pub const DEFAULT_PORT: u16 = 47800;
/// Local HTTP port of the control panel (bound to 127.0.0.1 only).
pub const DEFAULT_CONTROL_PORT: u16 = 47801;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Platform {
    Windows,
    Android,
    Linux,
    MacOs,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "android") {
            Platform::Android
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Other
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Shares keyboard and mouse and takes part in the screen layout.
    pub input: bool,
    pub clipboard: bool,
    pub files: bool,
    /// Exposes its drives to peers (stage 2).
    pub drives: bool,
    /// Can show other PCs' windows and share its own.
    pub windows: bool,
}

/// Who currently has the shared cursor. Newer tokens win; ties are broken by name
/// so every peer converges on the same owner.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusToken {
    pub epoch: u64,
    pub owner: String,
}

impl FocusToken {
    pub fn is_newer_than(&self, other: &FocusToken) -> bool {
        (self.epoch, &self.owner) > (other.epoch, &other.owner)
    }

    pub fn next(&self, owner: impl Into<String>) -> FocusToken {
        FocusToken { epoch: self.epoch + 1, owner: owner.into() }
    }
}

/// Arrangement of machines, shared by all peers. The newest version wins.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedLayout {
    pub version: u64,
    pub author: String,
    pub placements: BTreeMap<String, Placement>,
}

impl SharedLayout {
    pub fn is_newer_than(&self, other: &SharedLayout) -> bool {
        (self.version, &self.author) > (other.version, &other.author)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub protocol: u32,
    pub platform: Platform,
    pub caps: Capabilities,
    pub monitors: Vec<Rect>,
    pub layout: SharedLayout,
    pub focus: FocusToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative movement in physical pixels of the sending machine.
    MouseMove {
        dx: i32,
        dy: i32,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// Wheel rotation in Windows units (120 = one notch).
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// A key identified by its hardware scan code, so the receiving machine applies
    /// its own keyboard layout. `vk` is a fallback for keys without a scan code.
    Key {
        scan: u16,
        extended: bool,
        vk: u16,
        down: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardData {
    Text(String),
    /// RGBA pixels, deflate-compressed.
    Image {
        width: u32,
        height: u32,
        rgba_deflate: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Hello(Hello),
    /// The sender's monitor configuration changed.
    Monitors(Vec<Rect>),
    Layout(SharedLayout),
    /// The shared cursor moved to `token.owner`. `enter` is where it appears there.
    Focus {
        token: FocusToken,
        enter: Option<Point>,
    },
    Input(InputEvent),
    Clipboard(ClipboardData),
    /// Start of a file. `rel_path` uses `/` separators and is relative to the
    /// receiver's download folder; `batch` groups files sent together.
    FileOffer {
        id: u64,
        batch: u64,
        rel_path: String,
        size: u64,
    },
    FileChunk {
        id: u64,
        data: Vec<u8>,
    },
    FileEnd {
        id: u64,
        sha256: [u8; 32],
    },
    FileAbort {
        id: u64,
        reason: String,
    },
    /// A window of the sender is now shown on the receiver.
    WindowOpen {
        share: u64,
        title: String,
        width: u32,
        height: u32,
    },
    /// New picture of a shared window, JPEG-encoded.
    WindowFrame {
        share: u64,
        width: u32,
        height: u32,
        jpeg: Vec<u8>,
    },
    /// Stop showing a shared window (sent by either side).
    WindowClose {
        share: u64,
    },
    /// Keyboard or mouse used on the shown copy of a window, for the original.
    WindowInput {
        share: u64,
        ev: WindowInputEvent,
    },
    Ping(u64),
    Pong(u64),
}

/// Input for a shared window. Positions are pixels inside the original window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowInputEvent {
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, down: bool, x: i32, y: i32 },
    Wheel { dx: i32, dy: i32, x: i32, y: i32 },
    Key { scan: u16, extended: bool, vk: u16, down: bool },
}

impl Message {
    /// Bulk messages may wait behind input; everything else is latency sensitive.
    pub fn is_bulk(&self) -> bool {
        matches!(
            self,
            Message::Clipboard(_)
                | Message::FileOffer { .. }
                | Message::FileChunk { .. }
                | Message::FileEnd { .. }
                | Message::FileAbort { .. }
                | Message::WindowFrame { .. }
        )
    }
}

/// Messages on a pairing connection, before the asking machine knows the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairMessage {
    /// "Let me join your group."
    Request {
        name: String,
        platform: Platform,
    },
    /// The user on the other machine said yes: here is the group key.
    Accept {
        name: String,
        key: String,
    },
    Decline,
}

/// Announcement every MultiPC sends on the LAN, so all machines are listed in the
/// panel, including ones from another group that can be asked to join.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    pub name: String,
    pub port: u16,
    pub platform: Platform,
    /// Public tag of the sender's group (see `config::group_id`).
    pub group: [u8; 8],
}

const BEACON_MAGIC: &[u8; 4] = b"MPB2";

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = BEACON_MAGIC.to_vec();
        v.extend(postcard::to_stdvec(self).expect("beacon serializes"));
        v
    }

    pub fn decode(data: &[u8]) -> Option<Beacon> {
        let body = data.strip_prefix(BEACON_MAGIC)?;
        postcard::from_bytes(body).ok()
    }
}

pub fn encode_image(width: u32, height: u32, rgba: &[u8]) -> ClipboardData {
    ClipboardData::Image { width, height, rgba_deflate: miniz_oxide::deflate::compress_to_vec(rgba, 3) }
}

pub fn decode_image(data: &[u8], expected_len: usize) -> Option<Vec<u8>> {
    let out = miniz_oxide::inflate::decompress_to_vec_with_limit(data, expected_len).ok()?;
    (out.len() == expected_len).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_ordering() {
        let a = FocusToken { epoch: 3, owner: "a".into() };
        let b = FocusToken { epoch: 3, owner: "b".into() };
        assert!(b.is_newer_than(&a));
        assert!(a.next("a").is_newer_than(&b));
        assert!(!a.is_newer_than(&a));
    }

    #[test]
    fn message_roundtrip() {
        let m = Message::Input(InputEvent::Key { scan: 0x1e, extended: false, vk: 0x41, down: true });
        let bytes = postcard::to_stdvec(&m).unwrap();
        assert_eq!(postcard::from_bytes::<Message>(&bytes).unwrap(), m);
        // Input events must stay tiny: they are sent for every mouse movement.
        assert!(bytes.len() < 12, "{} bytes", bytes.len());
    }

    #[test]
    fn beacon_roundtrip() {
        let b = Beacon { name: "ПК-1".into(), port: 47800, platform: Platform::Windows, group: [3; 8] };
        assert_eq!(Beacon::decode(&b.encode()), Some(b));
        assert_eq!(Beacon::decode(b"MPB2"), None);
        assert_eq!(Beacon::decode(b"garbage"), None);
    }

    #[test]
    fn image_roundtrip() {
        let rgba: Vec<u8> = (0..64 * 64 * 4).map(|i| (i % 7) as u8).collect();
        let ClipboardData::Image { rgba_deflate, .. } = encode_image(64, 64, &rgba) else { unreachable!() };
        assert!(rgba_deflate.len() < rgba.len());
        assert_eq!(decode_image(&rgba_deflate, rgba.len()).unwrap(), rgba);
        assert!(decode_image(&rgba_deflate, 10).is_none());
    }
}
