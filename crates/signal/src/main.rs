//! mpvlink signaling server.
//!
//! Matches peers by room code and relays offers/answers between them over
//! WebSockets. It never sees media. Rooms exist while anyone is in them.
//! Clients that drop off can resume their place for a while (see the protocol
//! docs), so brief network blips or proxy reloads don't interrupt anyone.
//!
//! Usage: mpvlink-signal [--listen 0.0.0.0:8787]

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use mpvlink_proto::{
    ClientMsg, MAX_RESUME_TOKEN_LEN, PeerId, PeerInfo, RESUME_GRACE_SECS, Role, ServerMsg, normalize_name,
    normalize_room,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Signaling messages are small (an SDP is a few KB); refuse anything big.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Clients ping every 30 s. A connection silent for this long is presumed dead
/// (e.g. the peer lost power or network, so no TCP reset ever arrives) and is
/// dropped, so a vanished publisher doesn't block its room.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

type Outbox = mpsc::UnboundedSender<ServerMsg>;

/// How long a disconnected peer's place is held for it to resume.
const RESUME_GRACE: Duration = Duration::from_secs(RESUME_GRACE_SECS);

/// Messages kept for a disconnected peer; beyond this it's unlikely to catch
/// up meaningfully, and the queue mustn't grow without bound.
const MAX_QUEUED_WHILE_AWAY: usize = 256;

/// Identifies one WebSocket connection (a peer may have several over time).
type ConnId = u64;

enum Link {
    Connected(Outbox),
    /// Disconnected, waiting to resume; messages for it are queued.
    Away(Vec<ServerMsg>),
}

struct Member {
    info: PeerInfo,
    resume_token: Option<String>,
    /// The connection that currently speaks for this peer.
    conn: ConnId,
    link: Link,
}

impl Member {
    fn send(&mut self, msg: ServerMsg) {
        match &mut self.link {
            Link::Connected(outbox) => {
                let _ = outbox.send(msg);
            }
            Link::Away(queue) if queue.len() < MAX_QUEUED_WHILE_AWAY => queue.push(msg),
            Link::Away(_) => {}
        }
    }
}

#[derive(Default)]
struct State {
    rooms: HashMap<String, HashMap<PeerId, Member>>,
}

/// Result of a successful join.
#[derive(Debug, PartialEq, Eq)]
struct Joined {
    id: PeerId,
    resumed: bool,
}

/// What happened when a connection closed.
#[derive(Debug, PartialEq, Eq)]
enum Departure {
    /// Removed from the room; others were told.
    Left,
    /// Held for RESUME_GRACE; call `expire` afterwards.
    Away,
    /// The connection no longer owned the peer (it resumed elsewhere).
    Superseded,
}

impl State {
    /// Add a peer to a room, or resume its earlier place if `resume_token`
    /// matches. A new peer's id is its connection's id. Queues the welcome
    /// (then anything held while it was away) on `outbox` before returning,
    /// so it's always the first message.
    fn join(
        &mut self,
        room: &str,
        (name, role, resume_token): (String, Role, Option<String>),
        conn: ConnId,
        outbox: Outbox,
    ) -> Result<Joined, String> {
        let new_id: PeerId = conn;
        let members = self.rooms.entry(room.to_string()).or_default();
        let others = |members: &HashMap<PeerId, Member>, me: PeerId| -> Vec<PeerInfo> {
            members.values().filter(|m| m.info.id != me).map(|m| m.info.clone()).collect()
        };

        // Resume (also takes over from a connection that isn't known dead yet).
        if let Some(token) = &resume_token
            && let Some(id) = members.values().find(|m| m.resume_token.as_ref() == Some(token)).map(|m| m.info.id)
        {
            let peers = others(members, id);
            let member = members.get_mut(&id).expect("found above");
            let _ = outbox.send(ServerMsg::Welcome { you: id, peers, resumed: true });
            if let Link::Away(queue) = std::mem::replace(&mut member.link, Link::Connected(outbox.clone())) {
                for msg in queue {
                    let _ = outbox.send(msg);
                }
            }
            member.conn = conn;
            return Ok(Joined { id, resumed: true });
        }

        if role == Role::Publisher && members.values().any(|m| m.info.role == Role::Publisher) {
            return Err("This room already has someone streaming.".into());
        }
        let info = PeerInfo { id: new_id, name, role };
        let _ = outbox.send(ServerMsg::Welcome { you: new_id, peers: others(members, new_id), resumed: false });
        for m in members.values_mut() {
            m.send(ServerMsg::PeerJoined { peer: info.clone() });
        }
        members.insert(new_id, Member { info, resume_token, conn, link: Link::Connected(outbox) });
        Ok(Joined { id: new_id, resumed: false })
    }

    /// A connection closed. Peers that can resume are held; others leave now.
    fn disconnect(&mut self, room: &str, id: PeerId, conn: ConnId) -> Departure {
        let Some(member) = self.rooms.get_mut(room).and_then(|m| m.get_mut(&id)) else {
            return Departure::Superseded;
        };
        if member.conn != conn {
            return Departure::Superseded;
        }
        if member.resume_token.is_some() {
            member.link = Link::Away(Vec::new());
            return Departure::Away;
        }
        self.remove(room, id);
        Departure::Left
    }

    /// The grace period ran out: remove the peer unless it came back.
    /// Returns true if it was removed.
    fn expire(&mut self, room: &str, id: PeerId, conn: ConnId) -> bool {
        let still_away = self
            .rooms
            .get(room)
            .and_then(|m| m.get(&id))
            .is_some_and(|m| m.conn == conn && matches!(m.link, Link::Away(_)));
        if still_away {
            self.remove(room, id);
        }
        still_away
    }

    fn remove(&mut self, room: &str, id: PeerId) {
        let Some(members) = self.rooms.get_mut(room) else { return };
        if members.remove(&id).is_none() {
            return;
        }
        for m in members.values_mut() {
            m.send(ServerMsg::PeerLeft { id });
        }
        if members.is_empty() {
            self.rooms.remove(room);
        }
    }

    /// Deliver (or queue) a message for a member of the same room. False if
    /// they're gone.
    fn relay(&mut self, room: &str, to: PeerId, msg: ServerMsg) -> bool {
        match self.rooms.get_mut(room).and_then(|m| m.get_mut(&to)) {
            Some(member) => {
                member.send(msg);
                true
            }
            None => false,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let listen = parse_args()?;
    let listener = TcpListener::bind(&listen).await.with_context(|| format!("binding {listen}"))?;
    println!("mpvlink-signal listening on ws://{}", listener.local_addr()?);

    let state = Arc::new(Mutex::new(State::default()));
    // One counter for both connection ids and peer ids: unique, and a new
    // peer's id is simply its first connection's id.
    let next_id = Arc::new(AtomicU64::new(1));
    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();
        let conn = next_id.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, addr, conn, state).await {
                println!("[conn {conn}] {addr}: {e:#}");
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

async fn handle(stream: TcpStream, addr: SocketAddr, conn: ConnId, state: Arc<Mutex<State>>) -> Result<()> {
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

    // (room, our peer id) once joined.
    let mut joined: Option<(String, PeerId)> = None;
    let tag = |joined: &Option<(String, PeerId)>| match joined {
        Some((_, id)) => format!("[{id}] {addr}"),
        None => format!("[conn {conn}] {addr}"),
    };
    // Every exit from this loop must fall through to the cleanup below (no
    // `?`), or a peer whose connection drops would linger in its room.
    loop {
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, ws_rx.next()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => {
                println!("{}: no traffic for {}s, dropping", tag(&joined), IDLE_TIMEOUT.as_secs());
                break;
            }
        };
        let text = match frame {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => break,
            Ok(_) => continue, // pings (answered by tungstenite) count as traffic; ignore binary
            Err(e) => {
                println!("{}: connection error: {e}", tag(&joined));
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
            (ClientMsg::Join { room, name, role, resume }, None) => {
                let (Some(room), Some(name)) = (normalize_room(&room), normalize_name(&name)) else {
                    let _ = outbox.send(ServerMsg::Error { message: "Invalid room code or name.".into() });
                    break;
                };
                if resume.as_ref().is_some_and(|t| t.is_empty() || t.len() > MAX_RESUME_TOKEN_LEN) {
                    let _ = outbox.send(ServerMsg::Error { message: "Invalid resume token.".into() });
                    break;
                }
                let result = state.lock().unwrap().join(&room, (name.clone(), role, resume), conn, outbox.clone());
                match result {
                    Ok(Joined { id, resumed }) => {
                        let verb = if resumed { "resumed in" } else { "joined" };
                        println!("[{id}] {addr}: {name} {verb} {room} as {role:?}");
                        joined = Some((room, id));
                    }
                    Err(message) => {
                        let _ = outbox.send(ServerMsg::Error { message });
                        break;
                    }
                }
            }
            (ClientMsg::Signal { to, signal }, Some((room, id))) => {
                if !state.lock().unwrap().relay(room, to, ServerMsg::Signal { from: *id, signal }) {
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

    if let Some((room, id)) = joined {
        let departure = state.lock().unwrap().disconnect(&room, id, conn);
        match departure {
            Departure::Left => println!("[{id}] {addr}: left {room}"),
            Departure::Superseded => {}
            Departure::Away => {
                println!("[{id}] {addr}: disconnected from {room}; holding for {}s", RESUME_GRACE.as_secs());
                let state = state.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(RESUME_GRACE).await;
                    if state.lock().unwrap().expire(&room, id, conn) {
                        println!("[{id}] {addr}: didn't come back; left {room}");
                    }
                });
            }
        }
    }
    drop(outbox);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Client {
        rx: mpsc::UnboundedReceiver<ServerMsg>,
        tx: Outbox,
    }

    fn client() -> Client {
        let (tx, rx) = mpsc::unbounded_channel();
        Client { rx, tx }
    }

    impl Client {
        fn next(&mut self) -> ServerMsg {
            self.rx.try_recv().expect("a message")
        }
        fn idle(&mut self) -> bool {
            self.rx.try_recv().is_err()
        }
    }

    fn join(s: &mut State, c: &Client, id: PeerId, role: Role, token: Option<&str>) -> Result<Joined, String> {
        s.join("ROOM", (format!("p{id}"), role, token.map(str::to_string)), id, c.tx.clone())
    }

    fn info(id: PeerId, role: Role) -> PeerInfo {
        PeerInfo { id, name: format!("p{id}"), role }
    }

    #[test]
    fn rooms_track_members_and_reject_second_publisher() {
        let mut s = State::default();
        let (mut p, mut v, p2) = (client(), client(), client());

        join(&mut s, &p, 1, Role::Publisher, None).unwrap();
        assert_eq!(p.next(), ServerMsg::Welcome { you: 1, peers: vec![], resumed: false });
        join(&mut s, &v, 2, Role::Viewer, None).unwrap();
        assert_eq!(v.next(), ServerMsg::Welcome { you: 2, peers: vec![info(1, Role::Publisher)], resumed: false });
        assert_eq!(p.next(), ServerMsg::PeerJoined { peer: info(2, Role::Viewer) });
        assert!(join(&mut s, &p2, 3, Role::Publisher, None).is_err());

        assert!(s.relay("ROOM", 1, ServerMsg::PeerLeft { id: 9 }));
        assert_eq!(p.next(), ServerMsg::PeerLeft { id: 9 });
        assert!(!s.relay("OTHER", 1, ServerMsg::PeerLeft { id: 9 }), "no cross-room relay");

        assert_eq!(s.disconnect("ROOM", 2, 2), Departure::Left, "no token: leaves immediately");
        assert_eq!(p.next(), ServerMsg::PeerLeft { id: 2 });
        assert_eq!(s.disconnect("ROOM", 1, 1), Departure::Left);
        assert!(s.rooms.is_empty(), "empty rooms are removed");
    }

    #[test]
    fn resume_keeps_id_hides_the_blip_and_delivers_queued_messages() {
        let mut s = State::default();
        let (mut p, mut v) = (client(), client());
        join(&mut s, &p, 1, Role::Publisher, Some("ptoken")).unwrap();
        join(&mut s, &v, 2, Role::Viewer, Some("vtoken")).unwrap();
        p.next();
        p.next();
        v.next();

        // The viewer's connection drops; the publisher hears nothing.
        assert_eq!(s.disconnect("ROOM", 2, 2), Departure::Away);
        assert!(p.idle());
        // Meanwhile the publisher signals it; that's queued.
        assert!(s.relay("ROOM", 2, ServerMsg::PeerLeft { id: 42 }));

        // The viewer reconnects (new connection 7) with its token.
        let mut v2 = client();
        let joined = s.join("ROOM", ("p2".into(), Role::Viewer, Some("vtoken".into())), 7, v2.tx.clone());
        assert_eq!(joined, Ok(Joined { id: 2, resumed: true }));
        assert_eq!(v2.next(), ServerMsg::Welcome { you: 2, peers: vec![info(1, Role::Publisher)], resumed: true });
        assert_eq!(v2.next(), ServerMsg::PeerLeft { id: 42 }, "queued message delivered after the welcome");
        assert!(p.idle(), "publisher never saw the viewer leave or rejoin");

        // The old connection's late cleanup doesn't remove the resumed peer,
        // and the grace timer for the old connection does nothing.
        assert_eq!(s.disconnect("ROOM", 2, 2), Departure::Superseded);
        assert!(!s.expire("ROOM", 2, 2));
        assert!(s.relay("ROOM", 2, ServerMsg::PeerLeft { id: 43 }));
        assert_eq!(v2.next(), ServerMsg::PeerLeft { id: 43 });
    }

    #[test]
    fn unresumed_peers_expire_and_free_the_room() {
        let mut s = State::default();
        let (p, mut v) = (client(), client());
        join(&mut s, &p, 1, Role::Publisher, Some("ptoken")).unwrap();
        join(&mut s, &v, 2, Role::Viewer, Some("vtoken")).unwrap();
        v.next();

        assert_eq!(s.disconnect("ROOM", 1, 1), Departure::Away);
        // While away, the publisher still holds the room.
        assert!(join(&mut s, &client(), 3, Role::Publisher, Some("other")).is_err());
        assert!(v.idle());

        assert!(s.expire("ROOM", 1, 1));
        assert_eq!(v.next(), ServerMsg::PeerLeft { id: 1 });
        assert!(join(&mut s, &client(), 4, Role::Publisher, None).is_ok(), "room is free again");
    }

    #[test]
    fn resume_takes_over_a_connection_not_yet_known_dead() {
        let mut s = State::default();
        let (mut old, mut new) = (client(), client());
        join(&mut s, &old, 1, Role::Viewer, Some("t")).unwrap();
        old.next();
        // No disconnect seen yet, but the client reconnects anyway.
        let joined = s.join("ROOM", ("p1".into(), Role::Viewer, Some("t".into())), 5, new.tx.clone());
        assert_eq!(joined, Ok(Joined { id: 1, resumed: true }));
        new.next();
        assert!(s.relay("ROOM", 1, ServerMsg::PeerLeft { id: 9 }));
        assert_eq!(new.next(), ServerMsg::PeerLeft { id: 9 }, "messages go to the new connection");
        assert!(old.idle());
        assert_eq!(s.disconnect("ROOM", 1, 1), Departure::Superseded);
    }

    #[test]
    fn away_queue_is_bounded() {
        let mut s = State::default();
        join(&mut s, &client(), 1, Role::Viewer, Some("t")).unwrap();
        s.disconnect("ROOM", 1, 1);
        for i in 0..1000 {
            s.relay("ROOM", 1, ServerMsg::PeerLeft { id: i });
        }
        let Link::Away(q) = &s.rooms["ROOM"][&1].link else { panic!("away") };
        assert_eq!(q.len(), MAX_QUEUED_WHILE_AWAY);
    }
}
