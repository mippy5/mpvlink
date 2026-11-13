//! Just enough MPEG-TS to read ffmpeg's output: the video stream's frames
//! (one PES packet each), the Opus audio stream's packets, and their
//! presentation timestamps.
//!
//! ffmpeg writes raw H.264 without timestamps, so the stream is wrapped in
//! MPEG-TS instead: that's how each frame's position in the file gets here,
//! which is what keeps the stream in step with mpv's playback.

const PACKET: usize = 188;
const SYNC: u8 = 0x47;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An H.264 access unit (Annex B).
    Video,
    /// One Opus packet.
    Audio,
}

/// A frame and its timestamp in 90 kHz units.
#[derive(Debug, PartialEq, Eq)]
pub struct TsFrame {
    pub kind: Kind,
    pub pts: i64,
    pub data: Vec<u8>,
}

/// One elementary stream being collected.
#[derive(Default)]
struct Stream {
    pid: Option<u16>,
    /// The PES being collected, and its timestamp.
    pes: Vec<u8>,
    pts: Option<i64>,
    /// Last timestamp, for unwrapping the 33-bit counter.
    last_pts: Option<i64>,
}

#[derive(Default)]
pub struct TsDemuxer {
    /// Bytes not yet forming a whole packet.
    pending: Vec<u8>,
    video: Stream,
    audio: Stream,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; complete frames are appended to `out`. A frame is complete
    /// when the next one starts, or at [`finish`](Self::finish).
    pub fn push(&mut self, data: &[u8], out: &mut Vec<TsFrame>) {
        self.pending.extend_from_slice(data);
        let mut at = 0;
        while self.pending.len() - at >= PACKET {
            if self.pending[at] != SYNC {
                // Lost sync: skip to the next sync byte.
                at += 1;
                continue;
            }
            let packet: [u8; PACKET] = self.pending[at..at + PACKET].try_into().expect("whole packet");
            self.packet(&packet, out);
            at += PACKET;
        }
        self.pending.drain(..at);
    }

    /// End of input: emit the last frame.
    pub fn finish(&mut self, out: &mut Vec<TsFrame>) {
        self.emit(out);
    }

    fn packet(&mut self, p: &[u8; PACKET], out: &mut Vec<TsFrame>) {
        let unit_start = p[1] & 0x40 != 0;
        let pid = (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]);
        let adaptation = (p[3] >> 4) & 0x3;
        let mut start = 4;
        if adaptation & 0x2 != 0 {
            start += 1 + usize::from(p[4]);
        }
        if adaptation & 0x1 == 0 || start >= PACKET {
            return; // no payload
        }
        let payload = &p[start..];

        // Find the streams by their first PES: video has stream ids E0-EF;
        // ffmpeg puts Opus in private stream 1 (BD), each packet starting with
        // an Opus control header (0x7FE0, 11 bits set).
        let pes_start = unit_start && payload.len() > 9 && payload[..3] == [0, 0, 1];
        if pes_start && self.video.pid.is_none() && (0xE0..=0xEF).contains(&payload[3]) {
            self.video.pid = Some(pid);
        }
        if pes_start && self.audio.pid.is_none() && payload[3] == 0xBD {
            let data = &payload[(9 + usize::from(payload[8])).min(payload.len())..];
            if data.len() >= 2 && data[0] == 0x7F && data[1] & 0xE0 == 0xE0 {
                self.audio.pid = Some(pid);
            }
        }
        let kind = if self.video.pid == Some(pid) {
            Kind::Video
        } else if self.audio.pid == Some(pid) {
            Kind::Audio
        } else {
            return;
        };
        let stream = match kind {
            Kind::Video => &mut self.video,
            Kind::Audio => &mut self.audio,
        };
        if unit_start {
            emit(kind, stream, out);
            if !pes_start {
                return;
            }
            let flags = payload[7];
            let header_end = 9 + usize::from(payload[8]);
            if flags & 0x80 != 0 && payload.len() >= 14 {
                stream.pts = Some(unwrap(&mut stream.last_pts, parse_timestamp(&payload[9..14])));
            }
            if header_end <= payload.len() {
                stream.pes.extend_from_slice(&payload[header_end..]);
            }
        } else {
            stream.pes.extend_from_slice(payload);
        }
    }

    fn emit(&mut self, out: &mut Vec<TsFrame>) {
        emit(Kind::Video, &mut self.video, out);
        emit(Kind::Audio, &mut self.audio, out);
    }
}

/// A finished PES: a video frame, or one or more Opus packets.
fn emit(kind: Kind, stream: &mut Stream, out: &mut Vec<TsFrame>) {
    let data = std::mem::take(&mut stream.pes);
    let Some(pts) = stream.pts.take() else { return };
    if data.is_empty() {
        return;
    }
    match kind {
        Kind::Video => out.push(TsFrame { kind, pts, data }),
        Kind::Audio => {
            // Later packets in the same PES follow on from the first.
            let mut pts = pts;
            for packet in opus_packets(&data) {
                let duration = opus_samples(packet).unwrap_or(960) as i64 * 90_000 / 48_000;
                out.push(TsFrame { kind, pts, data: packet.to_vec() });
                pts += duration;
            }
        }
    }
}

/// Timestamps are 33-bit and wrap (every ~26.5 hours); keep them increasing.
fn unwrap(last: &mut Option<i64>, raw: i64) -> i64 {
    const WRAP: i64 = 1 << 33;
    let pts = match *last {
        Some(prev) => {
            let base = prev - prev.rem_euclid(WRAP);
            // The candidate closest to the previous timestamp.
            [base - WRAP, base, base + WRAP].into_iter().map(|b| b + raw).min_by_key(|p| (p - prev).abs()).unwrap_or(raw)
        }
        None => raw,
    };
    *last = Some(pts);
    pts
}

/// Split an Opus-in-MPEG-TS PES payload into Opus packets. Each packet comes
/// after a control header: 0x7FE0 with flags, the size as a run of 0xFF bytes
/// plus a final byte, then optional trim and extension fields.
fn opus_packets(mut data: &[u8]) -> Vec<&[u8]> {
    let mut packets = Vec::new();
    while data.len() >= 2 && data[0] == 0x7F && data[1] & 0xE0 == 0xE0 {
        let flags = data[1];
        let mut at = 2;
        let mut size = 0usize;
        loop {
            let Some(&b) = data.get(at) else { return packets };
            at += 1;
            size += usize::from(b);
            if b != 0xFF {
                break;
            }
        }
        if flags & 0x10 != 0 {
            at += 2; // start trim
        }
        if flags & 0x08 != 0 {
            at += 2; // end trim
        }
        if flags & 0x04 != 0 {
            at += 1 + usize::from(*data.get(at).unwrap_or(&0)); // control extension
        }
        let Some(packet) = data.get(at..at + size) else { return packets };
        packets.push(packet);
        data = &data[at + size..];
    }
    packets
}

/// Samples (at 48 kHz) in an Opus packet, from its TOC byte (RFC 6716 3.1).
pub fn opus_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let per_frame = match config {
        0..=11 => [480, 960, 1920, 2880][usize::from(config % 4)], // SILK: 10/20/40/60 ms
        12..=15 => [480, 960][usize::from(config % 2)],             // hybrid: 10/20 ms
        _ => [120, 240, 480, 960][usize::from(config % 4)],         // CELT: 2.5/5/10/20 ms
    };
    let frames = match toc & 0x3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    Some(per_frame * frames)
}

/// A 33-bit PES timestamp from its 5-byte encoding.
fn parse_timestamp(b: &[u8]) -> i64 {
    (i64::from(b[0] >> 1) & 0x7) << 30
        | i64::from(b[1]) << 22
        | i64::from(b[2] >> 1) << 15
        | i64::from(b[3]) << 7
        | i64::from(b[4] >> 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_timestamp(pts: i64) -> [u8; 5] {
        [
            0x21 | (((pts >> 30) & 0x7) << 1) as u8,
            (pts >> 22) as u8,
            0x01 | (((pts >> 15) & 0x7F) << 1) as u8,
            (pts >> 7) as u8,
            0x01 | ((pts & 0x7F) << 1) as u8,
        ]
    }

    /// TS packets carrying one PES with `data`, padded with an adaptation field.
    fn pes_packets(pid: u16, pts: i64, data: &[u8]) -> Vec<u8> {
        pes_packets_with(0xE0, pid, pts, data)
    }

    fn pes_packets_with(stream_id: u8, pid: u16, pts: i64, data: &[u8]) -> Vec<u8> {
        let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5];
        pes.extend_from_slice(&encode_timestamp(pts));
        pes.extend_from_slice(data);
        let mut out = Vec::new();
        for (i, chunk) in pes.chunks(184 - 8).enumerate() {
            let mut p = vec![SYNC, (if i == 0 { 0x40 } else { 0 }) | (pid >> 8) as u8, pid as u8, 0x30];
            let stuffing = 184 - 1 - chunk.len();
            p.push(stuffing as u8);
            if stuffing > 0 {
                p.push(0);
                p.extend(std::iter::repeat_n(0xFF, stuffing - 1));
            }
            p.extend_from_slice(chunk);
            assert_eq!(p.len(), PACKET);
            out.extend(p);
        }
        out
    }

    #[test]
    fn reads_frames_and_timestamps_across_split_input() {
        let big: Vec<u8> = (0..500).map(|i| i as u8).collect();
        let mut stream = pes_packets(256, 3000, b"\0\0\0\x01\x65first");
        stream.extend(pes_packets(256, 6000, &big));
        stream.extend(pes_packets(256, 9000, b"last"));

        let mut demux = TsDemuxer::new();
        let mut out = Vec::new();
        // Feed in awkward pieces, as a pipe would.
        for piece in stream.chunks(100) {
            demux.push(piece, &mut out);
        }
        assert_eq!(out.len(), 2, "the last frame waits for the next one or finish");
        demux.finish(&mut out);
        assert_eq!(out[0], TsFrame { kind: Kind::Video, pts: 3000, data: b"\0\0\0\x01\x65first".to_vec() });
        assert_eq!(out[1], TsFrame { kind: Kind::Video, pts: 6000, data: big });
        assert_eq!(out[2].pts, 9000);
    }

    #[test]
    fn ignores_other_pids_and_unwraps_timestamps() {
        let wrap = 1i64 << 33;
        let mut stream = pes_packets(256, wrap - 3000, b"a");
        stream.extend(pes_packets(4096, 1, b"not video")); // PAT/PMT-like noise on another PID
        stream.extend(pes_packets(256, 0, b"b"));
        let mut demux = TsDemuxer::new();
        let mut out = Vec::new();
        demux.push(&stream, &mut out);
        demux.finish(&mut out);
        assert_eq!(out.iter().map(|f| f.pts).collect::<Vec<_>>(), [wrap - 3000, wrap]);
    }

    #[test]
    fn reads_opus_packets_interleaved_with_video() {
        // Two Opus packets (20 ms CELT, TOC 0xFC... config 31 = 20 ms) in one
        // PES: the first with a start trim, the second 300 bytes (size 255+45).
        let first = [0xFC, 1, 2, 3];
        let second: Vec<u8> = std::iter::once(0xFC).chain((0..299).map(|i| i as u8)).collect();
        let mut pes = vec![0x7F, 0xF0, first.len() as u8, 0x01, 0x38];
        pes.extend_from_slice(&first);
        pes.extend_from_slice(&[0x7F, 0xE0, 0xFF, 45]);
        pes.extend_from_slice(&second);

        let mut stream = pes_packets_with(0xBD, 257, 1000, &pes);
        stream.extend(pes_packets(256, 1500, b"\0\0\0\x01\x65v"));
        stream.extend(pes_packets_with(0xBD, 257, 1000 + 2 * 1800, &[0x7F, 0xE0, 1, 0xFC]));
        let mut demux = TsDemuxer::new();
        let mut out = Vec::new();
        demux.push(&stream, &mut out);
        demux.finish(&mut out);

        let audio: Vec<_> = out.iter().filter(|f| f.kind == Kind::Audio).collect();
        assert_eq!(audio.len(), 3);
        assert_eq!((audio[0].pts, audio[0].data.as_slice()), (1000, &first[..]));
        assert_eq!((audio[1].pts, audio[1].data.len()), (1000 + 1800, 300), "20 ms later, size read past 0xFF");
        assert_eq!(audio[2].pts, 1000 + 3600);
        assert_eq!(out.iter().filter(|f| f.kind == Kind::Video).count(), 1);
    }

    #[test]
    fn opus_durations_from_the_toc() {
        assert_eq!(opus_samples(&[0xFC]), Some(960), "CELT FB 20 ms, one frame");
        assert_eq!(opus_samples(&[0x18]), Some(2880), "SILK 60 ms (config 3)");
        assert_eq!(opus_samples(&[0xFD]), Some(1920), "two 20 ms frames");
        assert_eq!(opus_samples(&[0xE3, 3]), Some(360), "CELT 2.5 ms (config 28), code 3 with three frames");
        assert_eq!(opus_samples(&[]), None);
    }
}
