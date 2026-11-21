//! Sending a set of files (a "pack") from a streamer to each viewer over a
//! WebRTC data channel: how subtitle tracks and their fonts reach viewers.
//!
//! The channel is ordered and reliable, so the protocol is simply: a text
//! message `begin <json>` listing the files (name, size, and metadata the
//! caller defines), then the files' bytes back to back in binary chunks,
//! then the text message `end`. `clear` (text) means "no pack any more".
//! A new `begin` replaces whatever came before.

use bytes::{Bytes, BytesMut};
use serde_json::{Value, json};
use std::time::Duration;
use webrtc::data_channel::DataChannel;

/// The data channel's label.
pub const LABEL: &str = "mpvlink-files";
/// Binary messages carry at most this much (well under SCTP's limits).
const CHUNK: usize = 16 * 1024;
/// Sending pace: a pause after each this many bytes, so a large pack
/// (image subtitles run to tens of MB) doesn't crowd out the video.
const BURST: usize = 256 * 1024;
const BURST_PAUSE: Duration = Duration::from_millis(60);

/// One file of a pack.
#[derive(Clone, Debug, PartialEq)]
pub struct PackFile {
    pub name: String,
    /// Whatever the sender wants the receiver to know about it.
    pub meta: Value,
    pub data: Bytes,
}

/// Files sent together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pack {
    pub files: Vec<PackFile>,
}

/// Send `pack` (None: tell the viewer there's none now).
pub async fn send(channel: &dyn DataChannel, pack: Option<&Pack>) -> webrtc::error::Result<()> {
    let Some(pack) = pack else { return channel.send_text("clear").await };
    let list: Vec<Value> =
        pack.files.iter().map(|f| json!({ "name": f.name, "size": f.data.len(), "meta": f.meta })).collect();
    channel.send_text(&format!("begin {}", Value::Array(list))).await?;
    let mut since_pause = 0;
    for file in &pack.files {
        for chunk in file.data.chunks(CHUNK) {
            channel.send(BytesMut::from(chunk)).await?;
            since_pause += chunk.len();
            if since_pause >= BURST {
                since_pause = 0;
                tokio::time::sleep(BURST_PAUSE).await;
            }
        }
    }
    channel.send_text("end").await
}

/// What a receiver makes of the messages.
#[derive(Debug, PartialEq)]
pub enum Received {
    /// A complete pack.
    Pack(Pack),
    /// The sender has none now.
    Clear,
}

/// Puts packs back together from the channel's messages.
#[derive(Default)]
pub struct Receiver {
    /// The pack being received: its files, their data as it comes in, and
    /// which file is being filled.
    files: Vec<PackFile>,
    buffers: Vec<Vec<u8>>,
    sizes: Vec<usize>,
    current: usize,
    receiving: bool,
}

impl Receiver {
    /// Handle one message (`is_text`: a text message). A finished pack (or
    /// "clear") comes back; anything malformed is skipped.
    pub fn message(&mut self, data: &[u8], is_text: bool) -> Option<Received> {
        if is_text {
            let text = std::str::from_utf8(data).ok()?;
            if text == "clear" {
                self.receiving = false;
                return Some(Received::Clear);
            }
            if text == "end" {
                if !self.receiving || self.buffers.iter().zip(&self.sizes).any(|(b, s)| b.len() != *s) {
                    self.receiving = false;
                    return None; // incomplete: drop it
                }
                self.receiving = false;
                let mut files = std::mem::take(&mut self.files);
                for (file, data) in files.iter_mut().zip(self.buffers.drain(..)) {
                    file.data = Bytes::from(data);
                }
                return Some(Received::Pack(Pack { files }));
            }
            let list: Vec<Value> = serde_json::from_str(text.strip_prefix("begin ")?).ok()?;
            self.files.clear();
            self.buffers.clear();
            self.sizes.clear();
            for item in list {
                let name = item["name"].as_str()?.to_string();
                let size = item["size"].as_u64()? as usize;
                self.sizes.push(size);
                self.buffers.push(Vec::with_capacity(size.min(64 << 20)));
                self.files.push(PackFile { name, meta: item["meta"].clone(), data: Bytes::new() });
            }
            self.current = 0;
            self.receiving = true;
            return None;
        }
        if !self.receiving {
            return None;
        }
        // Bytes go to the first file that isn't full yet.
        let mut data = data;
        while !data.is_empty() {
            while self.current < self.buffers.len() && self.buffers[self.current].len() >= self.sizes[self.current] {
                self.current += 1;
            }
            let Some(buffer) = self.buffers.get_mut(self.current) else { break };
            let take = (self.sizes[self.current] - buffer.len()).min(data.len());
            buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassembles_files_across_chunks() {
        let pack = Pack {
            files: vec![
                PackFile { name: "a.ass".into(), meta: json!({ "title": "English" }), data: Bytes::from(vec![1u8; 40_000]) },
                PackFile { name: "empty".into(), meta: Value::Null, data: Bytes::new() },
                PackFile { name: "b.sup".into(), meta: json!(2), data: Bytes::from(vec![2u8; 5]) },
            ],
        };
        let mut r = Receiver::default();
        let list: Vec<Value> = pack.files.iter().map(|f| json!({ "name": f.name, "size": f.data.len(), "meta": f.meta })).collect();
        assert_eq!(r.message(format!("begin {}", Value::Array(list)).as_bytes(), true), None);
        // Chunks that straddle files.
        let all: Vec<u8> = pack.files.iter().flat_map(|f| f.data.to_vec()).collect();
        for chunk in all.chunks(7_000) {
            assert_eq!(r.message(chunk, false), None);
        }
        assert_eq!(r.message(b"end", true), Some(Received::Pack(pack)));
        assert_eq!(r.message(b"clear", true), Some(Received::Clear));
    }

    #[test]
    fn an_incomplete_pack_is_dropped() {
        let mut r = Receiver::default();
        r.message(br#"begin [{"name":"a","size":10,"meta":null}]"#, true);
        r.message(&[0; 4], false);
        assert_eq!(r.message(b"end", true), None);
    }
}
