//! Generated test video: moving bars plus a machine-readable send timestamp.

use openh264::formats::YUVBuffer;
use std::time::{SystemTime, UNIX_EPOCH};

/// Side of one timestamp block, in pixels. 32 blocks across the top-left.
pub const BLOCK: usize = 16;
pub const BITS: usize = 32;

const BLACK: u8 = 16;
const WHITE: u8 = 235;

pub struct TestPattern {
    width: usize,
    height: usize,
    frame: u64,
}

impl TestPattern {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, frame: 0 }
    }

    /// Render the next frame as I420.
    pub fn next_frame(&mut self) -> YUVBuffer {
        let (w, h) = (self.width, self.height);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        let (y_plane, uv) = yuv.split_at_mut(w * h);
        let (u_plane, v_plane) = uv.split_at_mut(w * h / 4);

        // Luma: a diagonal gradient with a bright bar sweeping left to right.
        let bar_x = (self.frame as usize * 8) % w;
        for y in 0..h {
            for x in 0..w {
                let in_bar = x.wrapping_sub(bar_x) < 24;
                y_plane[y * w + x] = if in_bar { WHITE } else { (40 + (x + y) * 140 / (w + h)) as u8 };
            }
        }
        // Chroma: four colored quadrants.
        let (cw, ch) = (w / 2, h / 2);
        for y in 0..ch {
            for x in 0..cw {
                let (right, bottom) = (x >= cw / 2, y >= ch / 2);
                u_plane[y * cw + x] = if right { 160 } else { 100 };
                v_plane[y * cw + x] = if bottom { 160 } else { 100 };
            }
        }

        // Send timestamp: ms since the epoch, low 32 bits, MSB first, on a neutral background.
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(0);
        for bit in 0..BITS {
            let on = ms >> (BITS - 1 - bit) & 1 == 1;
            for y in 0..BLOCK.min(h) {
                for x in bit * BLOCK..((bit + 1) * BLOCK).min(w) {
                    y_plane[y * w + x] = if on { WHITE } else { BLACK };
                }
            }
        }
        for y in 0..(BLOCK / 2).min(ch) {
            for x in 0..(BITS * BLOCK / 2).min(cw) {
                u_plane[y * cw + x] = 128;
                v_plane[y * cw + x] = 128;
            }
        }

        self.frame += 1;
        YUVBuffer::from_vec(yuv, w, h)
    }
}

/// Read a timestamp back from a grayscale image of a frame (same size as sent).
#[cfg(test)]
fn decode_timestamp(gray: &[u8], width: usize) -> u32 {
    (0..BITS).fold(0, |acc, bit| {
        // Sample the block's center to stay clear of compression ringing at edges.
        let (x, y) = (bit * BLOCK + BLOCK / 2, BLOCK / 2);
        acc << 1 | (gray[y * width + x] > 128) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::formats::YUVSource;

    #[test]
    fn timestamp_round_trips() {
        let mut p = TestPattern::new(640, 360);
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u32;
        let frame = p.next_frame();
        let decoded = decode_timestamp(frame.y(), 640);
        let after = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u32;
        assert!(decoded.wrapping_sub(before) <= after.wrapping_sub(before));
    }
}
