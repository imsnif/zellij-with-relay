use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use zellij_relay_protocol::{decode_control_frame, ControlMessage, PROTOCOL_VERSION};

use crate::relay_error::RelayHandshakeError;
use crate::tunnel_url::reject_insecure_relay_url;

/// Connect to the relay's `/tunnel/control` endpoint, send the auth frame,
/// and wait for the `Established` response. Returns the tunnel metadata plus
/// the open socket halves so the caller can keep the control channel alive.
pub async fn open_control_tunnel(
    relay_url: &str,
    session_name: String,
    zellij_version: String,
    relay_tunnel_auth_token: String,
    requested_slug: String,
    read_only: bool,
) -> Result<ControlTunnelSession> {
    let url = format!("{}/tunnel/control", relay_url.trim_end_matches('/'));
    reject_insecure_relay_url(&url)?;
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
        read_only,
    };
    sink.send(Message::Binary(auth.encode().into()))
        .await
        .context("sending TunnelAuth")?;

    let next = stream
        .next()
        .await
        .ok_or_else(|| anyhow!("relay closed control socket before sending Established"))?
        .context("reading first frame from relay control socket")?;

    let bytes = match next {
        Message::Binary(b) => b.to_vec(),
        Message::Text(t) => t.as_bytes().to_vec(),
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
            terminal_binding_secret,
        } => {
            validate_terminal_binding_secret_shape(&terminal_binding_secret)?;
            Ok(ControlTunnelSession {
                public_url,
                slug,
                tunnel_id,
                terminal_binding_secret,
                sink,
                stream,
            })
        },
        ControlMessage::Error { message, code } => Err(RelayHandshakeError::new(
            code,
            format!("relay rejected tunnel: {}", message),
        )
        .into()),
        other => Err(anyhow!("unexpected control frame during handshake: {:?}", other)),
    }
}

/// The relay always sends 64 lowercase hex chars (32 random bytes,
/// hex-encoded). With no protocol-version bump on this pre-release draft,
/// this shape check is the early, clear diagnostic for accidentally
/// connecting to a stale relay build (which would otherwise fail later,
/// more confusingly, at terminal binding).
fn validate_terminal_binding_secret_shape(secret: &str) -> Result<()> {
    if secret.len() != 64 || !secret.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(anyhow!(
            "relay did not provide a valid terminal binding secret (stale relay build?)"
        ));
    }
    Ok(())
}

pub struct ControlTunnelSession {
    pub public_url: String,
    pub slug: String,
    pub tunnel_id: String,
    pub terminal_binding_secret: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_64_char_lowercase_hex_accepted() {
        assert!(validate_terminal_binding_secret_shape(&"a".repeat(64)).is_ok());
        let mixed: String = "0123456789abcdef".chars().cycle().take(64).collect();
        assert!(validate_terminal_binding_secret_shape(&mixed).is_ok());
    }

    #[test]
    fn empty_secret_rejected() {
        let err = validate_terminal_binding_secret_shape("").unwrap_err();
        assert!(err.to_string().contains("stale relay build"));
    }

    #[test]
    fn wrong_length_rejected() {
        assert!(validate_terminal_binding_secret_shape(&"a".repeat(63)).is_err());
        assert!(validate_terminal_binding_secret_shape(&"a".repeat(65)).is_err());
    }

    #[test]
    fn uppercase_hex_rejected() {
        // Relay always emits lowercase; matches lowercase explicitly rather
        // than accepting `is_ascii_hexdigit()` (which would also allow A-F).
        let err = validate_terminal_binding_secret_shape(&"A".repeat(64)).unwrap_err();
        assert!(err.to_string().contains("stale relay build"));
    }

    #[test]
    fn non_hex_chars_rejected() {
        assert!(validate_terminal_binding_secret_shape(&"z".repeat(64)).is_err());
    }
}
