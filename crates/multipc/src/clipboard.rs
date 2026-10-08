//! Clipboard sync: watch the local clipboard, apply clipboards from peers.

use crate::daemon::Event;
use mpc_core::protocol::{decode_image, encode_image, ClipboardData};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const POLL: Duration = Duration::from_millis(300);
const MAX_TEXT: usize = 16 * 1024 * 1024;
const MAX_IMAGE_PIXELS: usize = 8192 * 8192;

/// Start the clipboard thread. Returns a sender for clipboards received from peers.
pub fn start(events: UnboundedSender<Event>) -> Option<mpsc::Sender<ClipboardData>> {
    let (tx, rx) = mpsc::channel::<ClipboardData>();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
    std::thread::Builder::new().name("clipboard".into()).spawn(move || run(events, rx, ready_tx)).ok()?;
    ready_rx.recv().ok().filter(|ok| *ok).map(|_| tx)
}

fn fingerprint(data: &ClipboardData) -> [u8; 32] {
    let mut h = Sha256::new();
    match data {
        ClipboardData::Text(t) => {
            h.update(b"t");
            h.update(t.as_bytes());
        }
        ClipboardData::Image { width, height, rgba_deflate } => {
            h.update(b"i");
            h.update(width.to_le_bytes());
            h.update(height.to_le_bytes());
            h.update(rgba_deflate);
        }
    }
    h.finalize().into()
}

#[cfg(windows)]
fn sequence() -> Option<u32> {
    Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn sequence() -> Option<u32> {
    None
}

fn read(cb: &mut arboard::Clipboard) -> Option<ClipboardData> {
    if let Ok(t) = cb.get_text() {
        if !t.is_empty() && t.len() <= MAX_TEXT {
            return Some(ClipboardData::Text(t));
        }
        return None;
    }
    // Only Windows tells us cheaply that the clipboard changed; elsewhere polling
    // images would mean re-reading them every few hundred milliseconds.
    sequence()?;
    let img = cb.get_image().ok()?;
    if img.width * img.height > MAX_IMAGE_PIXELS {
        return None;
    }
    Some(encode_image(img.width as u32, img.height as u32, &img.bytes))
}

fn write(cb: &mut arboard::Clipboard, data: &ClipboardData) {
    let result = match data {
        ClipboardData::Text(t) => cb.set_text(t.clone()),
        ClipboardData::Image { width, height, rgba_deflate } => {
            let (w, h) = (*width as usize, *height as usize);
            if w * h > MAX_IMAGE_PIXELS {
                return;
            }
            let Some(bytes) = decode_image(rgba_deflate, w * h * 4) else { return };
            cb.set_image(arboard::ImageData { width: w, height: h, bytes: Cow::Owned(bytes) })
        }
    };
    if let Err(e) = result {
        tracing::debug!("setting clipboard: {e}");
    }
}

fn run(events: UnboundedSender<Event>, incoming: mpsc::Receiver<ClipboardData>, ready: mpsc::Sender<bool>) {
    let mut cb = match arboard::Clipboard::new() {
        Ok(cb) => cb,
        Err(e) => {
            tracing::warn!("clipboard sync disabled: {e}");
            let _ = ready.send(false);
            return;
        }
    };
    let _ = ready.send(true);
    // Whatever is on the clipboard at startup is not news.
    let mut last_seq = sequence();
    let mut last = read(&mut cb).map(|d| fingerprint(&d));
    loop {
        match incoming.recv_timeout(POLL) {
            Ok(data) => {
                write(&mut cb, &data);
                // Remember what the clipboard holds now, not what we sent to it: the
                // system may convert images, and that must not bounce back to peers.
                last = read(&mut cb).map(|d| fingerprint(&d)).or(Some(fingerprint(&data)));
                last_seq = sequence();
            }
            Err(RecvTimeoutError::Timeout) => {
                let seq = sequence();
                if seq.is_some() && seq == last_seq {
                    continue;
                }
                last_seq = seq;
                if let Some(data) = read(&mut cb) {
                    let fp = fingerprint(&data);
                    if last != Some(fp) {
                        last = Some(fp);
                        if events.send(Event::Clipboard(data)).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
