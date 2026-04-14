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
}

/// High-level Rust view of a terminal-tunnel frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalMessage {
    Ready { tunnel_id: String },
    Error { message: String },
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
            None => Err(anyhow!("TerminalFrame has no payload")),
        }
    }
}
