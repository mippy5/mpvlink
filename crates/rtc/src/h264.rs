//! H.264 over RTP: reassembling received packets into whole frames.

use bytes::{BufMut, Bytes, BytesMut};
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;

const START_CODE: &[u8] = &[0, 0, 0, 1];

/// An access unit delimiter NAL. Appending one after each frame tells a
/// downstream Annex B parser (like FFmpeg's) the frame is complete right away,
/// instead of it waiting for the start of the next frame (a frame of latency).
const ACCESS_UNIT_DELIMITER: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0];

const NAL_IDR: u8 = 5;

/// Split an Annex B byte stream into NAL units (without start codes).
pub fn nal_units(mut data: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        // Skip to just past the next start code (00 00 01; a 4-byte code is a 3-byte one plus a zero).
        let start = data.windows(3).position(|w| w == [0, 0, 1])? + 3;
        data = &data[start..];
        let end = data.windows(3).position(|w| w == [0, 0, 1] || w == [0, 0, 0]).unwrap_or(data.len());
        let nal = &data[..end];
        data = &data[end..];
        Some(nal)
    })
    .filter(|nal| !nal.is_empty())
}

/// Does this Annex B frame contain an IDR slice (i.e. can decoding start here)?
pub fn is_keyframe(annex_b: &[u8]) -> bool {
    nal_units(annex_b).any(|nal| nal[0] & 0x1F == NAL_IDR)
}

/// What one received packet produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// A complete, decodable frame in Annex B format (ends with an AUD).
    pub frame: Option<Bytes>,
    /// Packets were lost; the caller should ask the sender for a keyframe.
    pub lost: bool,
}

/// Turns RTP payloads back into whole H.264 frames, for low latency:
///
/// - A frame is emitted as soon as its last packet (RTP marker bit) arrives.
/// - There's no jitter buffer: a sequence gap counts as loss. The damaged
///   frame is dropped and so is everything after it until the next keyframe,
///   since those frames would reference missing data and decode as garbage.
///   Retransmitted/reordered stragglers are ignored.
pub struct FrameAssembler {
    depacketizer: H264Packet,
    next_seq: Option<u16>,
    frame: BytesMut,
    frame_damaged: bool,
    need_keyframe: bool,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            depacketizer: H264Packet::default(),
            next_seq: None,
            frame: BytesMut::new(),
            frame_damaged: false,
            // Can't start decoding mid-stream.
            need_keyframe: true,
        }
    }

    /// Drop frames until the next keyframe (e.g. after the consumer fell behind
    /// and discarded data).
    pub fn require_keyframe(&mut self) {
        self.need_keyframe = true;
    }

    pub fn waiting_for_keyframe(&self) -> bool {
        self.need_keyframe
    }

    pub fn push(&mut self, seq: u16, marker: bool, payload: &Bytes) -> Pushed {
        let mut out = Pushed::default();

        if let Some(expected) = self.next_seq {
            let ahead = seq.wrapping_sub(expected);
            if ahead >= 0x8000 {
                return out; // older than expected: duplicate or late retransmission
            }
            if ahead > 0 {
                out.lost = true;
                self.frame_damaged = true;
                self.need_keyframe = true;
                // A half-received fragmented NAL can't be completed now.
                self.depacketizer = H264Packet::default();
            }
        }
        self.next_seq = Some(seq.wrapping_add(1));

        match self.depacketizer.depacketize(payload) {
            Ok(data) => self.frame.extend_from_slice(&data),
            Err(_) => self.frame_damaged = true,
        }

        if marker {
            let mut frame = std::mem::take(&mut self.frame);
            let damaged = std::mem::replace(&mut self.frame_damaged, false);
            if !damaged && !frame.is_empty() && (!self.need_keyframe || is_keyframe(&frame)) {
                self.need_keyframe = false;
                frame.put_slice(ACCESS_UNIT_DELIMITER);
                out.frame = Some(frame.freeze());
            }
        }
        out
    }
}

// ---- Frame positions ------------------------------------------------------------
//
// Each video frame carries its position in the streamed file (seconds), so
// a viewer can play the stream on the file's timeline (and time subtitle
// tracks to it). It rides in an SEI "user data unregistered" message, which
// decoders ignore.

/// Identifies our SEI message (16 bytes, as the format requires).
const POSITION_UUID: &[u8; 16] = b"mpvlink.position";
const NAL_SEI: u8 = 6;
const SEI_USER_DATA_UNREGISTERED: u8 = 5;

/// The SEI NAL unit (without start code) carrying `position`.
pub fn position_sei(position: f64) -> Vec<u8> {
    let mut payload = POSITION_UUID.to_vec();
    payload.extend(position.to_be_bytes());
    let mut rbsp = vec![SEI_USER_DATA_UNREGISTERED, payload.len() as u8];
    rbsp.extend(payload);
    rbsp.push(0x80); // rbsp_trailing_bits
    let mut nal = vec![NAL_SEI];
    // Emulation prevention: never 00 00 followed by 00..03 inside a NAL.
    let mut zeros = 0;
    for b in rbsp {
        if zeros >= 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        nal.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    nal
}

/// The position our SEI message in this Annex B frame carries, if any.
pub fn read_position(annex_b: &[u8]) -> Option<f64> {
    nal_units(annex_b).filter(|nal| nal[0] & 0x1F == NAL_SEI).find_map(|nal| {
        // Undo emulation prevention.
        let mut rbsp = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in &nal[1..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            rbsp.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        let rest = rbsp.strip_prefix(&[SEI_USER_DATA_UNREGISTERED, 24])?;
        let value = rest.strip_prefix(POSITION_UUID.as_slice())?;
        Some(f64::from_be_bytes(value.get(..8)?.try_into().ok()?))
    })
}

/// Put `sei` (a NAL unit) in an Annex B frame (with 4-byte start codes),
/// before its first slice, as SEI messages must be.
pub fn insert_before_slices(frame: &[u8], sei: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(frame.len() + sei.len() + 8);
    let mut inserted = false;
    for nal in nal_units(frame) {
        if !inserted && matches!(nal[0] & 0x1F, 1 | NAL_IDR) {
            out.put_slice(START_CODE);
            out.put_slice(sei);
            inserted = true;
        }
        out.put_slice(START_CODE);
        out.put_slice(nal);
    }
    out.freeze()
}

/// Convert an Annex B frame to a list of NAL units with 4-byte start codes,
/// which is the form `TrackLocalStaticSample` packetizes from, leaving out
/// NAL units of the types in `skip`.
pub fn normalize_start_codes(annex_b: &[u8], skip: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(annex_b.len() + 16);
    for nal in nal_units(annex_b).filter(|nal| !skip.contains(&(nal[0] & 0x1F))) {
        out.put_slice(START_CODE);
        out.put_slice(nal);
    }
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Single-NAL RTP payloads (the payload is the NAL itself).
    fn sps() -> Bytes {
        Bytes::from_static(&[0x67, 0x42, 0xC0, 0x1F])
    }
    fn idr() -> Bytes {
        Bytes::from_static(&[0x65, 0x88, 0x84, 0x00])
    }
    fn p_slice() -> Bytes {
        Bytes::from_static(&[0x41, 0x9A, 0x02, 0x00])
    }

    #[test]
    fn positions_round_trip_through_sei() {
        for position in [0.0, 1.0 / 3.0, 300.125, 1.0e-300, f64::from_bits(0x0000_0003_0000_0001)] {
            let frame = [&[0u8, 0, 0, 1][..], &sps(), &[0, 0, 0, 1], &idr()].concat();
            let tagged = insert_before_slices(&frame, &position_sei(position));
            assert_eq!(read_position(&tagged), Some(position), "{position}");
            let types: Vec<u8> = nal_units(&tagged).map(|n| n[0] & 0x1F).collect();
            assert_eq!(types, vec![7, 6, 5], "SPS, SEI, then the slice");
            assert!(!position_sei(position)[1..].windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] < 3), "escaped (00 00 03 is the escape itself)");
        }
        assert_eq!(read_position(&[0, 0, 0, 1, 0x65, 1]), None);
    }

    #[test]
    fn splits_nal_units() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        let nals: Vec<_> = nal_units(&stream).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
        assert!(is_keyframe(&stream));
        assert!(!is_keyframe(&[0, 0, 1, 0x41, 9]));
    }

    #[test]
    fn emits_frames_on_marker_starting_at_a_keyframe() {
        let mut a = FrameAssembler::new();
        // A P-frame before any keyframe is useless.
        assert_eq!(a.push(10, true, &p_slice()).frame, None);

        assert_eq!(a.push(11, false, &sps()).frame, None);
        let key = a.push(12, true, &idr()).frame.expect("keyframe");
        assert!(is_keyframe(&key));
        assert!(key.ends_with(ACCESS_UNIT_DELIMITER));
        assert_eq!(nal_units(&key).count(), 3, "SPS + IDR + AUD");

        assert!(a.push(13, true, &p_slice()).frame.is_some());
    }

    #[test]
    fn loss_drops_frames_until_next_keyframe() {
        let mut a = FrameAssembler::new();
        a.push(1, true, &idr());

        let gap = a.push(3, true, &p_slice()); // 2 is missing
        assert!(gap.lost);
        assert_eq!(gap.frame, None);
        assert_eq!(a.push(4, true, &p_slice()).frame, None, "still waiting for a keyframe");
        assert_eq!(a.push(2, true, &p_slice()), Pushed::default(), "late packet ignored");

        let recovered = a.push(5, true, &idr());
        assert!(!recovered.lost);
        assert!(recovered.frame.is_some());
        assert!(a.push(6, true, &p_slice()).frame.is_some());
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut a = FrameAssembler::new();
        a.push(u16::MAX, true, &idr());
        let next = a.push(0, true, &p_slice());
        assert!(!next.lost);
        assert!(next.frame.is_some());
    }

    #[test]
    fn require_keyframe_gates_until_idr() {
        let mut a = FrameAssembler::new();
        a.push(1, true, &idr());
        a.require_keyframe();
        assert_eq!(a.push(2, true, &p_slice()).frame, None);
        assert!(a.push(3, true, &idr()).frame.is_some());
    }

    #[test]
    fn normalizes_three_byte_start_codes() {
        let out = normalize_start_codes(&[0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2], &[]);
        assert_eq!(&out[..], &[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 2]);
    }

    #[test]
    fn leaves_out_skipped_nal_types() {
        // AUD, SPS, IDR -> SPS, IDR
        let out = normalize_start_codes(&[0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2], &[9]);
        assert_eq!(&out[..], &[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 2]);
    }
}
