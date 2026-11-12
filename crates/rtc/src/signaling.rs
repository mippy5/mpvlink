//! WebSocket client for the mpvlink signaling server.
//!
//! Supports `ws://` and `wss://` (TLS via rustls, verified against Mozilla's
//! root certificates bundled into the binary).
//!
//! The connection repairs itself: if it drops after joining, it reconnects
//! with backoff and rejoins with a resume token, so the server gives back the
//! same peer id and other peers never notice (see `mpvlink_proto` docs).
//! Peer-to-peer media isn't affected by any of this.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use mpvlink_proto::{ClientMsg, PeerId, Role, ServerMsg, Signal};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Ping this often so idle connections (a viewer waiting in a room) aren't
/// dropped by NATs or proxies, and so a dead connection is noticed.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Reconnect delays; the last one repeats. The early ones fit well inside the
/// server's resume grace period (30 s).
const BACKOFF: &[Duration] = &[
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(15),
];

/// Use ring for TLS. rustls needs exactly one process-wide crypto provider;
/// installing it explicitly keeps working even if another dependency ever
/// enables a second provider (which would make rustls refuse to pick).
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Who we are in the room.
#[derive(Clone, Debug)]
pub struct JoinInfo {
    pub room: String,
    pub name: String,
    pub role: Role,
}

/// What the connection reports.
#[derive(Debug)]
pub enum SignalingEvent {
    /// A message from the server. After a reconnect, the first one is a
    /// `Welcome`: `resumed: true` means nothing changed for anyone; `false`
    /// means a fresh start (e.g. the server restarted): new peer id, and
    /// peers must set up their connections again.
    Message(ServerMsg),
    /// The connection dropped; the next attempt is in `retry_in`.
    Disconnected { error: String, retry_in: Duration },
}

/// Sending half of a signaling connection. Cheap to clone. Events arrive on
/// the receiver from [`Signaling::connect`]; it yields `None` when the
/// connection is closed for good: after the server rejects us (a
/// `ServerMsg::Error` comes first), or once every `Signaling` is dropped.
#[derive(Clone)]
pub struct Signaling {
    tx: mpsc::UnboundedSender<ClientMsg>,
}

impl Signaling {
    /// Connect and join. Fails if the first connection can't be made (so a
    /// wrong address is reported right away); after that, it reconnects
    /// on its own.
    pub async fn connect(url: &str, join: JoinInfo) -> Result<(Self, mpsc::UnboundedReceiver<SignalingEvent>)> {
        if let Err(problem) = mpvlink_proto::validate_server_url(url) {
            bail!("bad signaling server address {url:?}: {problem}");
        }
        install_crypto_provider();
        let ws = open(url).await?;

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let conn = Connection { url: url.to_string(), join, token: resume_token(), out_rx, events: events_tx, unsent: None };
        tokio::spawn(conn.run(ws));
        Ok((Self { tx: out_tx }, events_rx))
    }

    /// Queue a message. While reconnecting, messages wait and are sent once
    /// the connection is back.
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(msg);
    }

    pub fn signal(&self, to: PeerId, signal: Signal) {
        self.send(ClientMsg::Signal { to, signal });
    }
}

async fn open(url: &str) -> Result<Ws> {
    let connect = tokio_tungstenite::connect_async(url);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(10), connect)
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))
        .and_then(|r| r.map_err(Into::into))
        .with_context(|| format!("connecting to signaling server {url}"))?;
    Ok(ws)
}

/// Random secret identifying this session to the server across reconnects.
fn resume_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let bits = || std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", bits(), bits())
}

/// How one connection ended.
enum Ended {
    /// Don't reconnect: rejected by the server, or nobody's listening anymore.
    ForGood,
    /// The connection was lost. `welcomed`: it got as far as joining.
    Lost { error: String, welcomed: bool },
}

struct Connection {
    url: String,
    join: JoinInfo,
    token: String,
    out_rx: mpsc::UnboundedReceiver<ClientMsg>,
    events: mpsc::UnboundedSender<SignalingEvent>,
    /// A message taken from the queue whose send failed; resent first.
    unsent: Option<ClientMsg>,
}

impl Connection {
    async fn run(mut self, mut ws: Ws) {
        let mut attempt = 0;
        loop {
            let mut error = match self.serve(ws).await {
                Ended::ForGood => return,
                Ended::Lost { error, welcomed } => {
                    if welcomed {
                        attempt = 0;
                    }
                    error
                }
            };
            // Reconnect, with backoff, until it works or nobody's listening.
            ws = loop {
                let retry_in = BACKOFF[attempt.min(BACKOFF.len() - 1)];
                attempt += 1;
                if self.events.send(SignalingEvent::Disconnected { error, retry_in }).is_err() {
                    return;
                }
                tokio::time::sleep(retry_in).await;
                match open(&self.url).await {
                    Ok(ws) => break ws,
                    Err(e) => error = format!("{e:#}"),
                }
            };
        }
    }

    /// Join (or resume) over `ws`, then pump messages until it ends.
    async fn serve(&mut self, ws: Ws) -> Ended {
        let (mut ws_tx, mut ws_rx) = ws.split();
        let lost = |e: &dyn std::fmt::Display, welcomed| Ended::Lost { error: e.to_string(), welcomed };

        let join = ClientMsg::Join {
            room: self.join.room.clone(),
            name: self.join.name.clone(),
            role: self.join.role,
            resume: Some(self.token.clone()),
        };
        if let Err(e) = ws_tx.send(text(&join)).await {
            return lost(&e, false);
        }

        let mut welcomed = false;
        let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
        keepalive.tick().await; // the first tick is immediate
        loop {
            tokio::select! {
                // Queued messages go out only once joined (the server requires Join first).
                msg = next_outgoing(&mut self.unsent, &mut self.out_rx), if welcomed => {
                    let Some(msg) = msg else {
                        let _ = ws_tx.close().await;
                        return Ended::ForGood; // every Signaling handle was dropped
                    };
                    if let Err(e) = ws_tx.send(text(&msg)).await {
                        self.unsent = Some(msg);
                        return lost(&e, welcomed);
                    }
                }
                frame = ws_rx.next() => {
                    let text = match frame {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) | None => return lost(&"connection closed", welcomed),
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => return lost(&e, welcomed),
                    };
                    let msg = match serde_json::from_str::<ServerMsg>(&text) {
                        Ok(msg) => msg,
                        Err(e) => {
                            eprintln!("mpvlink: ignoring bad signaling message: {e}");
                            continue;
                        }
                    };
                    let rejected = matches!(msg, ServerMsg::Error { .. });
                    welcomed |= matches!(msg, ServerMsg::Welcome { .. });
                    if self.events.send(SignalingEvent::Message(msg)).is_err() || rejected {
                        let _ = ws_tx.close().await;
                        return Ended::ForGood;
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = ws_tx.send(Message::Ping(Default::default())).await {
                        return lost(&e, welcomed);
                    }
                }
            }
        }
    }
}

/// The message that failed to send last time, else the next queued one.
async fn next_outgoing(unsent: &mut Option<ClientMsg>, queue: &mut mpsc::UnboundedReceiver<ClientMsg>) -> Option<ClientMsg> {
    match unsent.take() {
        Some(msg) => Some(msg),
        None => queue.recv().await,
    }
}

fn text(msg: &ClientMsg) -> Message {
    Message::text(serde_json::to_string(msg).expect("ClientMsg serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_tokens_are_random_and_fit_the_server_limit() {
        let (a, b) = (resume_token(), resume_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.len() <= mpvlink_proto::MAX_RESUME_TOKEN_LEN);
    }

    #[test]
    fn backoff_gets_back_within_the_resume_grace_period() {
        let first_few: Duration = BACKOFF.iter().take(5).sum();
        assert!(first_few < Duration::from_secs(mpvlink_proto::RESUME_GRACE_SECS));
    }
}
