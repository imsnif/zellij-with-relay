use anyhow::{Context, Result};
use futures_util::SinkExt;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use zellij_relay_protocol::TerminalMessage;

use super::control_tunnel::ControlTunnelSession;

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

/// After both control + terminal sockets are open, this function holds them
/// both alive until the caller fires the shutdown signal. Phase 1 only — once
/// virtual-client wiring lands in Phase 2, this becomes the multiplex pump.
pub async fn run_until_shutdown(
    mut control: ControlTunnelSession,
    terminal: TerminalTunnelSession,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let TerminalTunnelSession { mut sink, mut stream } = terminal;
    tokio::select! {
        _ = shutdown_rx => {
            log::info!("Relay tunnel shutdown signal received, closing sockets");
        }
        _ = drain(&mut control.stream) => {
            log::warn!("Relay control socket closed unexpectedly");
        }
        _ = drain(&mut stream) => {
            log::warn!("Relay terminal socket closed unexpectedly");
        }
    }
    let _ = control.sink.send(Message::Close(None)).await;
    let _ = sink.send(Message::Close(None)).await;
}

async fn drain<S>(stream: &mut S)
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    use futures_util::StreamExt;
    while let Some(msg) = stream.next().await {
        if let Err(e) = msg {
            log::debug!("relay socket read error: {}", e);
            break;
        }
    }
}
