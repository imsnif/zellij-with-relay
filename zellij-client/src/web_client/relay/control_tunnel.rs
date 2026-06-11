use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use zellij_relay_protocol::{decode_control_frame, ControlMessage, PROTOCOL_VERSION};

/// Connect to the relay's `/tunnel/control` endpoint, send the auth frame,
/// and wait for the `Established` response. Returns the tunnel metadata plus
/// the open socket halves so the caller can keep the control channel alive.
pub async fn open_control_tunnel(
    relay_url: &str,
    session_name: String,
    zellij_version: String,
    relay_tunnel_auth_token: String,
    requested_slug: String,
) -> Result<ControlTunnelSession> {
    let url = format!("{}/tunnel/control", relay_url.trim_end_matches('/'));
    let (ws_stream, _resp) = connect_async(&url)
        .await
        .with_context(|| format!("connecting to relay control endpoint at {}", url))?;
    let (mut sink, mut stream) = ws_stream.split();

    let auth = ControlMessage::Auth {
        token: relay_tunnel_auth_token,
        session_name,
        protocol_version: PROTOCOL_VERSION,
        zellij_version,
        requested_slug,
    };
    sink.send(Message::Binary(auth.encode()))
        .await
        .context("sending TunnelAuth")?;

    let next = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("relay closed control socket before sending Established"))?
        .context("reading first frame from relay control socket")?;

    let bytes = match next {
        Message::Binary(b) => b,
        Message::Text(t) => t.into_bytes(),
        Message::Close(_) => {
            return Err(anyhow!("relay closed control socket during handshake"));
        },
        other => {
            return Err(anyhow!(
                "unexpected ws frame during handshake: {:?}",
                other
            ));
        },
    };

    match decode_control_frame(&bytes)? {
        ControlMessage::Established {
            public_url,
            slug,
            tunnel_id,
        } => Ok(ControlTunnelSession {
            public_url,
            slug,
            tunnel_id,
            sink,
            stream,
        }),
        ControlMessage::Error { message } => Err(anyhow!("relay rejected tunnel: {}", message)),
        other => Err(anyhow!("unexpected control frame during handshake: {:?}", other)),
    }
}

pub struct ControlTunnelSession {
    pub public_url: String,
    pub slug: String,
    pub tunnel_id: String,
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
