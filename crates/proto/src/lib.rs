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
//!
//! Reconnecting: clients send a random `resume` token with `join`. If their
//! WebSocket drops, the server keeps their place for [`RESUME_GRACE_SECS`]
//! without telling anyone, queueing messages for them. Rejoining with the same
//! token within that window gets the same peer id back (`welcome.resumed`),
//! so other peers never notice and peer-to-peer media keeps flowing.

use serde::{Deserialize, Serialize};

/// Server-assigned id, unique for the server's lifetime.
pub type PeerId = u64;

pub const MAX_ROOM_LEN: usize = 16;
pub const MIN_ROOM_LEN: usize = 4;
pub const MAX_NAME_CHARS: usize = 32;
pub const MAX_RESUME_TOKEN_LEN: usize = 64;

/// How long the server holds a disconnected peer's place for them to resume.
pub const RESUME_GRACE_SECS: u64 = 30;

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
    Join {
        room: String,
        name: String,
        role: Role,
        /// Client-chosen secret identifying this session across reconnects.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
    },
    /// Relay a signal to another peer in the same room.
    Signal { to: PeerId, signal: Signal },
}

/// Messages from the server to a client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Reply to `Join`: your id and everyone already in the room.
    Welcome {
        you: PeerId,
        peers: Vec<PeerInfo>,
        /// True if this resumed an earlier connection: same id, and other
        /// peers were never told you left. False means a fresh start.
        #[serde(default)]
        resumed: bool,
    },
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

/// Check a signaling server URL: `ws://` or `wss://`, a host, and an
/// optional numeric port and path. Returns a user-facing message if invalid.
pub fn validate_server_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .ok_or("The address must start with wss:// (or ws:// for unencrypted).")?;
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The address can't contain spaces.".into());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Split host and port, allowing a bracketed IPv6 host like [::1]:8787.
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or("Missing ] after the IPv6 address.")?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or("Unexpected text after the IPv6 address.")?),
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("The address is missing a host name.".into());
    }
    if let Some(port) = port
        && port.parse::<u16>().map_or(true, |p| p == 0)
    {
        return Err(format!("{port:?} isn't a valid port number."));
    }
    Ok(())
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
        let msg = ClientMsg::Join { room: "ABCD".into(), name: "Ana".into(), role: Role::Viewer, resume: None };
        assert_eq!(
            serde_json::to_string(&msg).unwrap(),
            r#"{"type":"join","room":"ABCD","name":"Ana","role":"viewer"}"#
        );
        // Old clients/servers without the resume fields still interoperate.
        let old: ServerMsg = serde_json::from_str(r#"{"type":"welcome","you":1,"peers":[]}"#).unwrap();
        assert_eq!(old, ServerMsg::Welcome { you: 1, peers: vec![], resumed: false });
        let back: ServerMsg = serde_json::from_str(r#"{"type":"peer_left","id":3}"#).unwrap();
        assert_eq!(back, ServerMsg::PeerLeft { id: 3 });
    }

    #[test]
    fn server_urls() {
        for ok in [
            "wss://signal.example.com",
            "wss://signal.example.com/",
            "ws://127.0.0.1:8787",
            "wss://203.0.113.5",
            "wss://[2001:db8::1]:443/path",
            "ws://localhost:8787/?x=1",
        ] {
            assert_eq!(validate_server_url(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "signal.example.com",
            "https://signal.example.com",
            "wss://",
            "wss://:8787",
            "wss://host:port",
            "wss://host:0",
            "wss://host:99999",
            "wss://[::1",
            "wss://[::1]x",
            "wss://exa mple.com",
        ] {
            assert!(validate_server_url(bad).is_err(), "{bad:?} should be rejected");
        }
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
