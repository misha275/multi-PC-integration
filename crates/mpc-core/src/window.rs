//! Helpers for showing a window of one PC on another: frame compression and
//! mapping positions between the copy and the original.

use anyhow::{ensure, Result};

/// A captured window picture, 4 bytes per pixel in B, G, R, A order (what
/// Windows hands out), rows top to bottom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// JPEG quality for window frames: sharp enough for text, small enough for 15+ fps on a LAN.
const QUALITY: u8 = 80;

pub fn encode_frame(f: &Frame) -> Result<Vec<u8>> {
    ensure!(f.width > 0 && f.height > 0 && f.width <= 16384 && f.height <= 16384, "bad frame size");
    ensure!(f.bgra.len() == (f.width * f.height * 4) as usize, "bad frame data");
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, QUALITY);
    enc.encode(&f.bgra, f.width as u16, f.height as u16, jpeg_encoder::ColorType::Bgra)?;
    Ok(out)
}

pub fn decode_frame(jpeg: &[u8]) -> Result<Frame> {
    let mut dec = jpeg_decoder::Decoder::new(jpeg);
    let rgb = dec.decode()?;
    let info = dec.info().ok_or_else(|| anyhow::anyhow!("no image info"))?;
    ensure!(info.pixel_format == jpeg_decoder::PixelFormat::RGB24, "unexpected JPEG format");
    let mut bgra = Vec::with_capacity(rgb.len() / 3 * 4);
    for p in rgb.chunks_exact(3) {
        bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    Ok(Frame { width: info.width as u32, height: info.height as u32, bgra })
}

/// Largest size with the same proportions as `w`×`h` that fits in `max_w`×`max_h`
/// (never enlarged).
pub fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || (w <= max_w && h <= max_h) {
        return (w, h);
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1))
}

/// Position in a copy shown at `shown_w`×`shown_h` → position in the original `src_w`×`src_h`.
pub fn to_source(x: i32, y: i32, shown_w: u32, shown_h: u32, src_w: u32, src_h: u32) -> (i32, i32) {
    if shown_w == 0 || shown_h == 0 {
        return (x, y);
    }
    let sx = (x as i64 * src_w as i64 / shown_w as i64).clamp(0, src_w.saturating_sub(1) as i64);
    let sy = (y as i64 * src_h as i64 / shown_h as i64).clamp(0, src_h.saturating_sub(1) as i64);
    (sx as i32, sy as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_keeps_size_and_colours() {
        let (w, h) = (64u32, 40u32);
        let mut bgra = Vec::new();
        for y in 0..h {
            for x in 0..w {
                // left half blue, right half red
                let px = if x < w / 2 { [200, 30, 30, 255] } else { [30, 30, 200, 255] };
                let _ = y;
                bgra.extend_from_slice(&px);
            }
        }
        let f = Frame { width: w, height: h, bgra };
        let jpeg = encode_frame(&f).unwrap();
        assert!(jpeg.len() < f.bgra.len() / 4);
        let back = decode_frame(&jpeg).unwrap();
        assert_eq!((back.width, back.height), (w, h));
        let px = |x: u32, y: u32| &back.bgra[((y * w + x) * 4) as usize..][..4];
        assert!(px(5, 20)[0] > 150 && px(5, 20)[2] < 80, "blue stays blue: {:?}", px(5, 20));
        assert!(px(60, 20)[2] > 150 && px(60, 20)[0] < 80, "red stays red: {:?}", px(60, 20));
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(encode_frame(&Frame { width: 2, height: 2, bgra: vec![0; 3] }).is_err());
        assert!(decode_frame(b"not a jpeg").is_err());
    }

    #[test]
    fn fitting_and_mapping() {
        assert_eq!(fit(1000, 500, 2000, 2000), (1000, 500));
        assert_eq!(fit(4000, 2000, 1000, 1000), (1000, 500));
        assert_eq!(to_source(500, 250, 1000, 500, 4000, 2000), (2000, 1000));
        assert_eq!(to_source(-5, 9999, 100, 100, 50, 50), (0, 49));
    }
}
