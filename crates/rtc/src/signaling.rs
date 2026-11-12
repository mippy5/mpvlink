//! WebSocket client for the mpvlink signaling server.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use mpvlink_proto::{ClientMsg, PeerId, ServerMsg, Signal};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Sending half of a signaling connection. Cheap to clone. Incoming messages
/// arrive on the receiver returned by [`Signaling::connect`], which yields
/// `None` once the connection is gone.
#[derive(Clone)]
pub struct Signaling {
    tx: mpsc::UnboundedSender<ClientMsg>,
}

impl Signaling {
    pub async fn connect(url: &str) -> Result<(Self, mpsc::UnboundedReceiver<ServerMsg>)> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connecting to signaling server {url}"))?;
        let (mut ws_tx, mut ws_rx) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientMsg>();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let text = serde_json::to_string(&msg).expect("ClientMsg serializes");
                if ws_tx.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = ws_tx.close().await;
        });

        let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerMsg>();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = ws_rx.next().await {
                let Message::Text(text) = frame else { continue };
                match serde_json::from_str::<ServerMsg>(&text) {
                    Ok(msg) => {
                        if in_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => eprintln!("mpvlink: ignoring bad signaling message: {e}"),
                }
            }
        });

        Ok((Self { tx: out_tx }, in_rx))
    }

    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(msg);
    }

    pub fn signal(&self, to: PeerId, signal: Signal) {
        self.send(ClientMsg::Signal { to, signal });
    }
}
