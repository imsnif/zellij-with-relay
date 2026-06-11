use anyhow::{Context, Result};
use futures_util::SinkExt;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use zellij_relay_protocol::TerminalMessage;

/// Phase 1: open the secondary `/tunnel/terminal?slug={slug}` WebSocket and
/// send the `Ready` ack so the relay links the two channels. The returned
/// type just bundles the streams so the caller can hold them open.
pub async fn open_terminal_tunnel(
    relay_url: &str,
    slug: &str,
    tunnel_id: String,
) -> Result<TerminalTunnelSession> {
    let url = format!(
        "{}/tunnel/terminal?slug={}",
        relay_url.trim_end_matches('/'),
        slug
    );
    let (ws_stream, _resp) = connect_async(&url)
        .await
        .with_context(|| format!("connecting to relay terminal endpoint at {}", url))?;
    let (mut sink, stream) = futures_util::StreamExt::split(ws_stream);

    let ready = TerminalMessage::Ready {
        tunnel_id: tunnel_id.clone(),
    };
    sink.send(Message::Binary(ready.encode()))
        .await
        .context("sending TerminalReady ack")?;

    Ok(TerminalTunnelSession { sink, stream })
}

pub struct TerminalTunnelSession {
    pub sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    pub stream: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
}

