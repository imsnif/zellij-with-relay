//! Ergonomic Rust wrappers around the generated protobuf messages.
//!
//! The generated `ControlFrame` / `TerminalFrame` types are always a oneof
//! payload; these enums make pattern-matching and construction easier.

use anyhow::{anyhow, Result};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::generated::zellij::relay::v1 as proto;

/// High-level Rust view of a control-tunnel frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    Auth {
        token: String,
        session_name: String,
        protocol_version: u32,
        zellij_version: String,
    },
    Established {
        public_url: String,
        slug: String,
        tunnel_id: String,
    },
    Error {
        message: String,
    },
    AuthChallenge {
        request_id: Vec<u8>,
        token_hash: String,
    },
    AuthResponse {
        request_id: Vec<u8>,
        client_id: u32,
        accepted: bool,
        is_read_only: bool,
        session_token_hash: String,
    },
    ClientConnected {
        client_id: u32,
    },
    ClientDisconnected {
        client_id: u32,
    },
    ControlFrameData {
        client_id: u32,
        data: Vec<u8>,
    },
}

/// High-level Rust view of a terminal-tunnel frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalMessage {
    Ready {
        tunnel_id: String,
    },
    Error {
        message: String,
    },
    TerminalFrameData {
        client_id: u32,
        data: Vec<u8>,
    },
}

impl ControlMessage {
    /// Encode to protobuf bytes for transmission on the control WebSocket.
    pub fn encode(&self) -> Vec<u8> {
        let frame: proto::ControlFrame = self.clone().into();
        frame.encode_to_vec()
    }
}

impl TerminalMessage {
    pub fn encode(&self) -> Vec<u8> {
        let frame: proto::TerminalFrame = self.clone().into();
        frame.encode_to_vec()
    }
}

pub fn decode_control_frame(bytes: &[u8]) -> Result<ControlMessage> {
    let frame = proto::ControlFrame::decode(bytes)?;
    frame.try_into()
}

pub fn decode_terminal_frame(bytes: &[u8]) -> Result<TerminalMessage> {
    let frame = proto::TerminalFrame::decode(bytes)?;
    frame.try_into()
}

// ---- ControlFrame conversions ----

impl From<ControlMessage> for proto::ControlFrame {
    fn from(msg: ControlMessage) -> Self {
        use proto::control_frame::Payload;
        let payload = match msg {
            ControlMessage::Auth {
                token,
                session_name,
                protocol_version,
                zellij_version,
            } => Payload::Auth(proto::TunnelAuth {
                token,
                session_name,
                protocol_version,
                zellij_version,
            }),
            ControlMessage::Established {
                public_url,
                slug,
                tunnel_id,
            } => Payload::Established(proto::TunnelEstablished {
                public_url,
                slug,
                tunnel_id,
            }),
            ControlMessage::Error { message } => {
                Payload::Error(proto::TunnelError { message })
            },
            ControlMessage::AuthChallenge {
                request_id,
                token_hash,
            } => Payload::AuthChallenge(proto::AuthChallenge {
                request_id,
                token_hash,
            }),
            ControlMessage::AuthResponse {
                request_id,
                client_id,
                accepted,
                is_read_only,
                session_token_hash,
            } => Payload::AuthResponse(proto::AuthResponse {
                request_id,
                client_id,
                accepted,
                is_read_only,
                session_token_hash,
            }),
            ControlMessage::ClientConnected { client_id } => {
                Payload::ClientConnected(proto::ClientConnected { client_id })
            },
            ControlMessage::ClientDisconnected { client_id } => {
                Payload::ClientDisconnected(proto::ClientDisconnected { client_id })
            },
            ControlMessage::ControlFrameData { client_id, data } => {
                Payload::ControlFrameData(proto::ControlFrameData { client_id, data })
            },
        };
        proto::ControlFrame {
            payload: Some(payload),
        }
    }
}

impl TryFrom<proto::ControlFrame> for ControlMessage {
    type Error = anyhow::Error;

    fn try_from(frame: proto::ControlFrame) -> Result<Self> {
        use proto::control_frame::Payload;
        match frame.payload {
            Some(Payload::Auth(a)) => Ok(ControlMessage::Auth {
                token: a.token,
                session_name: a.session_name,
                protocol_version: a.protocol_version,
                zellij_version: a.zellij_version,
            }),
            Some(Payload::Established(e)) => Ok(ControlMessage::Established {
                public_url: e.public_url,
                slug: e.slug,
                tunnel_id: e.tunnel_id,
            }),
            Some(Payload::Error(e)) => Ok(ControlMessage::Error {
                message: e.message,
            }),
            Some(Payload::AuthChallenge(c)) => Ok(ControlMessage::AuthChallenge {
                request_id: c.request_id,
                token_hash: c.token_hash,
            }),
            Some(Payload::AuthResponse(r)) => Ok(ControlMessage::AuthResponse {
                request_id: r.request_id,
                client_id: r.client_id,
                accepted: r.accepted,
                is_read_only: r.is_read_only,
                session_token_hash: r.session_token_hash,
            }),
            Some(Payload::ClientConnected(c)) => Ok(ControlMessage::ClientConnected {
                client_id: c.client_id,
            }),
            Some(Payload::ClientDisconnected(c)) => Ok(ControlMessage::ClientDisconnected {
                client_id: c.client_id,
            }),
            Some(Payload::ControlFrameData(d)) => Ok(ControlMessage::ControlFrameData {
                client_id: d.client_id,
                data: d.data,
            }),
            None => Err(anyhow!("ControlFrame has no payload")),
        }
    }
}

// ---- TerminalFrame conversions ----

impl From<TerminalMessage> for proto::TerminalFrame {
    fn from(msg: TerminalMessage) -> Self {
        use proto::terminal_frame::Payload;
        let payload = match msg {
            TerminalMessage::Ready { tunnel_id } => {
                Payload::Ready(proto::TunnelReady { tunnel_id })
            },
            TerminalMessage::Error { message } => {
                Payload::Error(proto::TunnelError { message })
            },
            TerminalMessage::TerminalFrameData { client_id, data } => {
                Payload::TerminalFrameData(proto::TerminalFrameData { client_id, data })
            },
        };
        proto::TerminalFrame {
            payload: Some(payload),
        }
    }
}

impl TryFrom<proto::TerminalFrame> for TerminalMessage {
    type Error = anyhow::Error;

    fn try_from(frame: proto::TerminalFrame) -> Result<Self> {
        use proto::terminal_frame::Payload;
        match frame.payload {
            Some(Payload::Ready(r)) => Ok(TerminalMessage::Ready {
                tunnel_id: r.tunnel_id,
            }),
            Some(Payload::Error(e)) => Ok(TerminalMessage::Error {
                message: e.message,
            }),
            Some(Payload::TerminalFrameData(d)) => Ok(TerminalMessage::TerminalFrameData {
                client_id: d.client_id,
                data: d.data,
            }),
            None => Err(anyhow!("TerminalFrame has no payload")),
        }
    }
}
