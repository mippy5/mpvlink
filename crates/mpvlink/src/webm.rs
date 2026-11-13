//! A minimal live WebM (Matroska) writer for one Opus track: how received
//! audio is handed to mpv.
//!
//! The stream is a header (EBML header, a Segment of unknown size, Info and
//! Tracks) followed by one Cluster per Opus packet. Clusters are
//! self-contained, so packets can be dropped anywhere to keep latency down.

/// Opus at 48 kHz (always, for Opus), stereo (what the sender encodes).
pub const CHANNELS: u8 = 2;
/// Samples the decoder drops at the start (libopus' usual encoder delay).
const PRE_SKIP: u16 = 312;

/// The stream header: everything before the first audio packet.
pub fn header() -> Vec<u8> {
    let ebml = [
        element(0x4286, &uint(1)),     // EBMLVersion
        element(0x42F7, &uint(1)),     // EBMLReadVersion
        element(0x42F2, &uint(4)),     // EBMLMaxIDLength
        element(0x42F3, &uint(8)),     // EBMLMaxSizeLength
        element(0x4282, b"webm"),      // DocType
        element(0x4287, &uint(4)),     // DocTypeVersion
        element(0x4285, &uint(2)),     // DocTypeReadVersion
    ]
    .concat();
    let info = [
        element(0x2AD7B1, &uint(1_000_000)), // TimecodeScale: timestamps in ms
        element(0x4D80, b"mpvlink"),         // MuxingApp
        element(0x5741, b"mpvlink"),         // WritingApp
    ]
    .concat();
    let audio = [
        element(0xB5, &48_000f64.to_be_bytes()), // SamplingFrequency
        element(0x9F, &uint(CHANNELS.into())),   // Channels
    ]
    .concat();
    let track = [
        element(0xD7, &uint(1)),         // TrackNumber
        element(0x73C5, &uint(1)),       // TrackUID
        element(0x83, &uint(2)),         // TrackType: audio
        element(0x86, b"A_OPUS"),        // CodecID
        element(0x63A2, &opus_head()),   // CodecPrivate
        element(0xE1, &audio),           // Audio
    ]
    .concat();
    let tracks = element(0xAE, &track); // TrackEntry

    let mut out = element(0x1A45DFA3, &ebml);
    out.extend(id(0x18538067)); // Segment, of unknown size: it's live
    out.extend([0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    out.extend(element(0x1549A966, &info));
    out.extend(element(0x1654AE6B, &tracks));
    out
}

/// A Cluster holding one Opus packet that starts at `timecode_ms`.
pub fn cluster(timecode_ms: u64, packet: &[u8]) -> Vec<u8> {
    // SimpleBlock: track 1, timecode 0 relative to the cluster, keyframe.
    let mut block = vec![0x81, 0x00, 0x00, 0x80];
    block.extend_from_slice(packet);
    let body = [element(0xE7, &uint(timecode_ms)), element(0xA3, &block)].concat();
    element(0x1F43B675, &body)
}

/// The Opus identification header (RFC 7845 5.1), Matroska's CodecPrivate.
fn opus_head() -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1); // version
    head.push(CHANNELS);
    head.extend(PRE_SKIP.to_le_bytes());
    head.extend(48_000u32.to_le_bytes()); // original sample rate
    head.extend(0i16.to_le_bytes()); // output gain
    head.push(0); // channel mapping family: mono/stereo
    head
}

/// An element: ID, size (always the 8-byte form, which is valid and simple),
/// payload.
fn element(element_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = id(element_id);
    out.push(0x01);
    out.extend(&(payload.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(payload);
    out
}

/// Element IDs are written as-is, without leading zero bytes.
fn id(element_id: u32) -> Vec<u8> {
    let bytes = element_id.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    bytes[skip..].to_vec()
}

/// An unsigned integer in as few bytes as possible (at least one).
fn uint(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count().min(7);
    bytes[skip..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_elements() {
        assert_eq!(uint(0), [0]);
        assert_eq!(uint(1_000_000), [0x0F, 0x42, 0x40]);
        assert_eq!(id(0xA3), [0xA3]);
        assert_eq!(id(0x1A45DFA3), [0x1A, 0x45, 0xDF, 0xA3]);
        assert_eq!(element(0xE7, &[5]), [0xE7, 0x01, 0, 0, 0, 0, 0, 0, 1, 5]);
    }

    #[test]
    fn header_and_cluster_shape() {
        let h = header();
        assert_eq!(&h[..4], [0x1A, 0x45, 0xDF, 0xA3], "starts with the EBML magic");
        assert!(h.windows(6).any(|w| w == b"A_OPUS"));
        assert!(h.windows(8).any(|w| w == b"OpusHead"));
        let c = cluster(40, &[0xFC, 9]);
        assert_eq!(&c[..4], [0x1F, 0x43, 0xB6, 0x75]);
        assert!(c.ends_with(&[0x81, 0, 0, 0x80, 0xFC, 9]));
    }
}
