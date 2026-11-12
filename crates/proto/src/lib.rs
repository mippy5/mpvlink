//! Signaling protocol between mpvlink clients and the signaling server.
//!
//! JSON messages over a WebSocket. The server only matches peers by room code
//! and relays [`Signal`]s between them; media never touches it.
//!
//! Flow for one publisher (P) and one viewer (V) in a room:
//!
//! ```text
//! P -> server: join {role: publisher}      V -> server: join {role: viewer}
//! server -> P: welcome {peers: [...]}      server -> V: welcome {peers: [...]}
//! server -> P: peer_joined {V}             (or V is already in P's welcome)
//! P -> V: offer      (relayed: signal {to} -> signal {from})
//! V -> P: answer
//! V -> P: keyframe_request                 (whenever V needs a fresh keyframe)
//! ```
//!
//! ICE candidates aren't trickled: each side waits for gathering to finish and
//! sends a complete SDP. Simpler, at the cost of a slower initial connect.

use serde::{Deserialize, Serialize};

/// Server-assigned id, unique for the server's lifetime.
pub type PeerId = u64;

pub const MAX_ROOM_LEN: usize = 16;
pub const MIN_ROOM_LEN: usize = 4;
pub const MAX_NAME_CHARS: usize = 32;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Sends the stream. At most one per room.
    Publisher,
    /// Watches the stream.
    Viewer,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    pub id: PeerId,
    pub name: String,
    pub role: Role,
}

/// Messages from a client to the server.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Must be the first message. Creates the room if it doesn't exist.
    Join { room: String, name: String, role: Role },
    /// Relay a signal to another peer in the same room.
    Signal { to: PeerId, signal: Signal },
}

/// Messages from the server to a client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Reply to `Join`: your id and everyone already in the room.
    Welcome { you: PeerId, peers: Vec<PeerInfo> },
    PeerJoined { peer: PeerInfo },
    PeerLeft { id: PeerId },
    Signal { from: PeerId, signal: Signal },
    /// The request was rejected. If it was the `Join`, the server closes the connection.
    Error { message: String },
}

/// Peer-to-peer payloads relayed by the server.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Signal {
    Offer { sdp: String },
    Answer { sdp: String },
    /// Viewer -> publisher: please send a keyframe (on join, or after loss).
    KeyframeRequest,
}

/// Normalize a room code: uppercase letters and digits only. Returns None if
/// the result isn't a valid length. Clients and server use the same rules.
pub fn normalize_room(code: &str) -> Option<String> {
    let code: String = code.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    (MIN_ROOM_LEN..=MAX_ROOM_LEN).contains(&code.len()).then_some(code)
}

/// Normalize a display name: trimmed, single-line, at most MAX_NAME_CHARS.
/// Returns None if nothing is left.
pub fn normalize_name(name: &str) -> Option<String> {
    let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let msg = ClientMsg::Signal { to: 7, signal: Signal::Offer { sdp: "v=0".into() } };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"signal","to":7,"signal":{"kind":"offer","sdp":"v=0"}}"#
        );
        let msg = ClientMsg::Join { room: "ABCD".into(), name: "Ana".into(), role: Role::Viewer };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"join","room":"ABCD","name":"Ana","role":"viewer"}"#
        );
        let back: ServerMsg = serde_json::from_str(r#"{"type":"peer_left","id":3}"#).unwrap();
        assert_eq!(back, ServerMsg::PeerLeft { id: 3 });
    }

    #[test]
    fn room_and_name_rules() {
        assert_eq!(normalize_room("ab-cd 12").as_deref(), Some("ABCD12"));
        assert_eq!(normalize_room("abc"), None);
        assert_eq!(normalize_room(&"a".repeat(17)), None);
        assert_eq!(normalize_name("  Zoë \n").as_deref(), Some("Zoë"));
        assert_eq!(normalize_name(" \t "), None);
        assert_eq!(normalize_name(&"x".repeat(50)).unwrap().len(), MAX_NAME_CHARS);
    }
}
