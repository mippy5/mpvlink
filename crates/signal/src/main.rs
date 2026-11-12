//! mpvlink signaling server.
//!
//! Matches peers by room code and relays offers/answers between them over
//! WebSockets. It never sees media. Rooms exist while anyone is in them.
//!
//! Usage: mpvlink-signal [--listen 0.0.0.0:8787]

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use mpvlink_proto::{ClientMsg, PeerId, PeerInfo, Role, ServerMsg, normalize_name, normalize_room};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Signaling messages are small (an SDP is a few KB); refuse anything big.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

type Outbox = mpsc::UnboundedSender<ServerMsg>;

struct Member {
    info: PeerInfo,
    outbox: Outbox,
}

#[derive(Default)]
struct State {
    rooms: HashMap<String, HashMap<PeerId, Member>>,
}

impl State {
    /// Add a peer to a room. Returns the other members' info for the welcome.
    fn join(&mut self, room: &str, info: PeerInfo, outbox: Outbox) -> Result<Vec<PeerInfo>, String> {
        let members = self.rooms.entry(room.to_string()).or_default();
        if info.role == Role::Publisher && members.values().any(|m| m.info.role == Role::Publisher) {
            return Err("This room already has someone streaming.".into());
        }
        let others: Vec<PeerInfo> = members.values().map(|m| m.info.clone()).collect();
        for m in members.values() {
            let _ = m.outbox.send(ServerMsg::PeerJoined { peer: info.clone() });
        }
        members.insert(info.id, Member { info, outbox });
        Ok(others)
    }

    fn leave(&mut self, room: &str, id: PeerId) {
        let Some(members) = self.rooms.get_mut(room) else { return };
        if members.remove(&id).is_none() {
            return;
        }
        for m in members.values() {
            let _ = m.outbox.send(ServerMsg::PeerLeft { id });
        }
        if members.is_empty() {
            self.rooms.remove(room);
        }
    }

    /// Deliver a message to a member of the same room. False if they're gone.
    fn relay(&self, room: &str, to: PeerId, msg: ServerMsg) -> bool {
        self.rooms.get(room).and_then(|m| m.get(&to)).is_some_and(|m| m.outbox.send(msg).is_ok())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let listen = parse_args()?;
    let listener = TcpListener::bind(&listen).await.with_context(|| format!("binding {listen}"))?;
    println!("mpvlink-signal listening on ws://{}", listener.local_addr()?);

    let state = Arc::new(Mutex::new(State::default()));
    let next_id = Arc::new(AtomicU64::new(1));
    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, addr, id, state).await {
                println!("[{id}] {addr}: {e:#}");
            }
        });
    }
}

fn parse_args() -> Result<String> {
    let mut listen = "0.0.0.0:8787".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().context("--listen needs an address")?,
            "-h" | "--help" => {
                println!("usage: mpvlink-signal [--listen ADDR:PORT]   (default 0.0.0.0:8787)");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other:?} (try --help)"),
        }
    }
    Ok(listen)
}

async fn handle(stream: TcpStream, addr: SocketAddr, id: PeerId, state: Arc<Mutex<State>>) -> Result<()> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // All writes go through one task, fed by this peer's outbox.
    let (outbox, mut outbox_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = outbox_rx.recv().await {
            let close = matches!(msg, ServerMsg::Error { .. });
            let text = serde_json::to_string(&msg).expect("ServerMsg serializes");
            if ws_tx.send(Message::text(text)).await.is_err() || close {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    let mut joined: Option<String> = None;
    // Every exit from this loop must fall through to the cleanup below (no
    // `?`), or a peer whose connection drops would linger in its room.
    while let Some(frame) = ws_rx.next().await {
        let text = match frame {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue, // pings are answered by tungstenite; ignore binary
            Err(e) => {
                println!("[{id}] {addr}: connection error: {e}");
                break;
            }
        };
        let msg: ClientMsg = match serde_json::from_str(&text) {
            Ok(msg) => msg,
            Err(e) => {
                let _ = outbox.send(ServerMsg::Error { message: format!("bad message: {e}") });
                break;
            }
        };
        match (msg, &joined) {
            (ClientMsg::Join { room, name, role }, None) => {
                let (Some(room), Some(name)) = (normalize_room(&room), normalize_name(&name)) else {
                    let _ = outbox.send(ServerMsg::Error { message: "Invalid room code or name.".into() });
                    break;
                };
                let info = PeerInfo { id, name, role };
                let result = state.lock().unwrap().join(&room, info.clone(), outbox.clone());
                match result {
                    Ok(peers) => {
                        println!("[{id}] {addr}: {} joined {room} as {:?}", info.name, role);
                        let _ = outbox.send(ServerMsg::Welcome { you: id, peers });
                        joined = Some(room);
                    }
                    Err(message) => {
                        let _ = outbox.send(ServerMsg::Error { message });
                        break;
                    }
                }
            }
            (ClientMsg::Signal { to, signal }, Some(room)) => {
                if !state.lock().unwrap().relay(room, to, ServerMsg::Signal { from: id, signal }) {
                    let _ = outbox.send(ServerMsg::PeerLeft { id: to });
                }
            }
            (ClientMsg::Join { .. }, Some(_)) => {
                let _ = outbox.send(ServerMsg::Error { message: "Already joined.".into() });
                break;
            }
            (ClientMsg::Signal { .. }, None) => {
                let _ = outbox.send(ServerMsg::Error { message: "Join a room first.".into() });
                break;
            }
        }
    }

    if let Some(room) = &joined {
        state.lock().unwrap().leave(room, id);
        println!("[{id}] {addr}: left {room}");
    }
    drop(outbox);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: PeerId, role: Role) -> (PeerInfo, mpsc::UnboundedReceiver<ServerMsg>, Outbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        (PeerInfo { id, name: format!("p{id}"), role }, rx, tx)
    }

    #[test]
    fn rooms_track_members_and_reject_second_publisher() {
        let mut s = State::default();
        let (p, mut p_rx, p_tx) = peer(1, Role::Publisher);
        let (v, _v_rx, v_tx) = peer(2, Role::Viewer);
        let (p2, _p2_rx, p2_tx) = peer(3, Role::Publisher);

        assert_eq!(s.join("ROOM", p.clone(), p_tx).unwrap(), vec![]);
        assert_eq!(s.join("ROOM", v.clone(), v_tx).unwrap(), vec![p.clone()]);
        assert_eq!(p_rx.try_recv().unwrap(), ServerMsg::PeerJoined { peer: v.clone() });
        assert!(s.join("ROOM", p2, p2_tx).is_err());

        assert!(s.relay("ROOM", 1, ServerMsg::PeerLeft { id: 9 }));
        assert_eq!(p_rx.try_recv().unwrap(), ServerMsg::PeerLeft { id: 9 });
        assert!(!s.relay("OTHER", 1, ServerMsg::PeerLeft { id: 9 }), "no cross-room relay");

        s.leave("ROOM", 2);
        assert_eq!(p_rx.try_recv().unwrap(), ServerMsg::PeerLeft { id: 2 });
        s.leave("ROOM", 1);
        assert!(s.rooms.is_empty(), "empty rooms are removed");
    }
}
