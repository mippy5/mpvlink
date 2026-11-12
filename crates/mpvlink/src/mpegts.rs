//! Just enough MPEG-TS to read ffmpeg's output: the video stream's frames
//! (one PES packet each) and their presentation timestamps.
//!
//! ffmpeg writes raw H.264 without timestamps, so the stream is wrapped in
//! MPEG-TS instead: that's how each frame's position in the file gets here,
//! which is what keeps the stream in step with mpv's playback.

const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// A video frame (Annex B H.264) and its timestamp in 90 kHz units.
#[derive(Debug, PartialEq, Eq)]
pub struct TsFrame {
    pub pts: i64,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub struct TsDemuxer {
    /// Bytes not yet forming a whole packet.
    pending: Vec<u8>,
    /// The PID carrying video, once seen.
    video_pid: Option<u16>,
    /// The frame being collected, and its timestamp.
    pes: Vec<u8>,
    pts: Option<i64>,
    /// Last timestamp, for unwrapping the 33-bit counter.
    last_pts: Option<i64>,
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

        let is_video_pes = unit_start && payload.len() > 3 && payload[..3] == [0, 0, 1] && (0xE0..=0xEF).contains(&payload[3]);
        if self.video_pid.is_none() && is_video_pes {
            self.video_pid = Some(pid);
        }
        if self.video_pid != Some(pid) {
            return;
        }
        if unit_start {
            self.emit(out);
            if !is_video_pes || payload.len() < 9 {
                return;
            }
            let flags = payload[7];
            let header_end = 9 + usize::from(payload[8]);
            if flags & 0x80 != 0 && payload.len() >= 14 {
                self.pts = Some(self.unwrap(parse_timestamp(&payload[9..14])));
            }
            if header_end <= payload.len() {
                self.pes.extend_from_slice(&payload[header_end..]);
            }
        } else {
            self.pes.extend_from_slice(payload);
        }
    }

    fn emit(&mut self, out: &mut Vec<TsFrame>) {
        let data = std::mem::take(&mut self.pes);
        if let Some(pts) = self.pts.take()
            && !data.is_empty()
        {
            out.push(TsFrame { pts, data });
        }
    }

    /// Timestamps are 33-bit and wrap (every ~26.5 hours); keep them increasing.
    fn unwrap(&mut self, raw: i64) -> i64 {
        const WRAP: i64 = 1 << 33;
        let pts = match self.last_pts {
            Some(last) => {
                let base = last - last.rem_euclid(WRAP);
                // The candidate closest to the last timestamp.
                [base - WRAP, base, base + WRAP]
                    .into_iter()
                    .map(|b| b + raw)
                    .min_by_key(|p| (p - last).abs())
                    .unwrap_or(raw)
            }
            None => raw,
        };
        self.last_pts = Some(pts);
        pts
    }
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
        let mut pes = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5];
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
        assert_eq!(out[0], TsFrame { pts: 3000, data: b"\0\0\0\x01\x65first".to_vec() });
        assert_eq!(out[1], TsFrame { pts: 6000, data: big });
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
}
