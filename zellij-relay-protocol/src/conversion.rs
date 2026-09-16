//! Ergonomic Rust wrappers around the generated protobuf messages.
//!
//! The generated `ControlFrame` / `TerminalFrame` types are always a oneof
//! payload; these enums make pattern-matching and construction easier.

use anyhow::{anyhow, Result};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::generated::zellij::relay::v1 as proto;

/// Typed classification of a `TunnelError`, mirroring the proto `TunnelErrorCode`
/// enum plus the structured protocol-version detail. Receivers branch on this
/// instead of substring-matching the human-readable message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TunnelErrorCode {
    Unspecified,
    MalformedFrame,
    UnexpectedFrame,
    ProtocolVersionUnsupported {
        supported_min: u32,
        supported_max: u32,
        offered_version: u32,
    },
    AuthRejected,
    MissingSlug,
    UnknownSlug,
    TunnelIdMismatch,
    /// Terminal socket presented a bad/absent binding secret. Distinct from
    /// `AuthRejected` — signals a linking bug/attack, not bad account
    /// credentials, and must not trigger the sharer plugin's auth-rejected UX.
    TerminalBindingRejected,
}

impl TunnelErrorCode {
    fn from_proto(err: &proto::TunnelError) -> Self {
        match proto::TunnelErrorCode::from_i32(err.code)
            .unwrap_or(proto::TunnelErrorCode::TunnelErrorUnspecified)
        {
            proto::TunnelErrorCode::TunnelErrorUnspecified => TunnelErrorCode::Unspecified,
            proto::TunnelErrorCode::MalformedFrame => TunnelErrorCode::MalformedFrame,
            proto::TunnelErrorCode::UnexpectedFrame => TunnelErrorCode::UnexpectedFrame,
            proto::TunnelErrorCode::ProtocolVersionUnsupported => {
                TunnelErrorCode::ProtocolVersionUnsupported {
                    supported_min: err.supported_min,
                    supported_max: err.supported_max,
                    offered_version: err.offered_version,
                }
            },
            proto::TunnelErrorCode::AuthRejected => TunnelErrorCode::AuthRejected,
            proto::TunnelErrorCode::MissingSlug => TunnelErrorCode::MissingSlug,
            proto::TunnelErrorCode::UnknownSlug => TunnelErrorCode::UnknownSlug,
            proto::TunnelErrorCode::TunnelIdMismatch => TunnelErrorCode::TunnelIdMismatch,
            proto::TunnelErrorCode::TerminalBindingRejected => {
                TunnelErrorCode::TerminalBindingRejected
            },
        }
    }

    fn to_proto_error(&self, message: String) -> proto::TunnelError {
        let mut err = proto::TunnelError {
            message,
            code: proto::TunnelErrorCode::TunnelErrorUnspecified as i32,
            supported_min: 0,
            supported_max: 0,
            offered_version: 0,
        };
        match self {
            TunnelErrorCode::Unspecified => {},
            TunnelErrorCode::MalformedFrame => {
                err.code = proto::TunnelErrorCode::MalformedFrame as i32;
            },
            TunnelErrorCode::UnexpectedFrame => {
                err.code = proto::TunnelErrorCode::UnexpectedFrame as i32;
            },
            TunnelErrorCode::ProtocolVersionUnsupported {
                supported_min,
                supported_max,
                offered_version,
            } => {
                err.code = proto::TunnelErrorCode::ProtocolVersionUnsupported as i32;
                err.supported_min = *supported_min;
                err.supported_max = *supported_max;
                err.offered_version = *offered_version;
            },
            TunnelErrorCode::AuthRejected => {
                err.code = proto::TunnelErrorCode::AuthRejected as i32;
            },
            TunnelErrorCode::MissingSlug => {
                err.code = proto::TunnelErrorCode::MissingSlug as i32;
            },
            TunnelErrorCode::UnknownSlug => {
                err.code = proto::TunnelErrorCode::UnknownSlug as i32;
            },
            TunnelErrorCode::TunnelIdMismatch => {
                err.code = proto::TunnelErrorCode::TunnelIdMismatch as i32;
            },
            TunnelErrorCode::TerminalBindingRejected => {
                err.code = proto::TunnelErrorCode::TerminalBindingRejected as i32;
            },
        }
        err
    }
}

/// High-level Rust view of a control-tunnel frame.
#[derive(Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    Auth {
        token: String,
        session_name: String,
        protocol_version: u32,
        zellij_version: String,
        /// Phase 6 reconnect support: empty string on a fresh handshake,
        /// the previously-issued slug when reconnecting. The relay
        /// honours it if still free, falls back to a fresh slug otherwise.
        requested_slug: String,
        /// Per-slug role. When true the relay drops viewer-originated input.
        read_only: bool,
    },
    Established {
        public_url: String,
        slug: String,
        tunnel_id: String,
        terminal_binding_secret: String,
    },
    Error {
        message: String,
        code: TunnelErrorCode,
    },
    /// Relay → Zellij: a viewer's opaque SPAKE2 message opening a handshake.
    PakeChallenge {
        request_id: Vec<u8>,
        viewer_msg: Vec<u8>,
        link_id: Vec<u8>,
    },
    /// Zellij → Relay: the sharer's SPAKE2 message + confirmation tag, or
    /// `accepted: false` when the slug's credential is locked out/absent.
    PakeResponse {
        request_id: Vec<u8>,
        client_id: u32,
        accepted: bool,
        sharer_msg: Vec<u8>,
        sharer_confirm: Vec<u8>,
    },
    /// Relay → Zellij: the viewer's confirmation tag (second round-trip).
    PakeConfirm {
        request_id: Vec<u8>,
        viewer_confirm: Vec<u8>,
    },
    /// Zellij → Relay: final accept/reject after verifying the viewer's tag.
    PakeResult {
        request_id: Vec<u8>,
        client_id: u32,
        accepted: bool,
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
#[derive(Clone, Serialize, Deserialize)]
pub enum TerminalMessage {
    Ready {
        tunnel_id: String,
        binding_secret: String,
    },
    Error {
        message: String,
        code: TunnelErrorCode,
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

// Manual Debug impls: these messages carry secrets (`Auth.token`,
// `Established.terminal_binding_secret`, `Ready.binding_secret`) that must
// never reach logs via a stray `{:?}` (e.g. `tracing::warn!(?other, ...)`
// on an unexpected-message branch). Every other variant/field prints as the
// derived impl would. The non-secret `Error.code` prints normally.
impl std::fmt::Debug for ControlMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlMessage::Auth {
                token: _,
                session_name,
                protocol_version,
                zellij_version,
                requested_slug,
                read_only,
            } => f
                .debug_struct("Auth")
                .field("token", &"<redacted>")
                .field("session_name", session_name)
                .field("protocol_version", protocol_version)
                .field("zellij_version", zellij_version)
                .field("requested_slug", requested_slug)
                .field("read_only", read_only)
                .finish(),
            ControlMessage::Established {
                public_url,
                slug,
                tunnel_id,
                terminal_binding_secret: _,
            } => f
                .debug_struct("Established")
                .field("public_url", public_url)
                .field("slug", slug)
                .field("tunnel_id", tunnel_id)
                .field("terminal_binding_secret", &"<redacted>")
                .finish(),
            ControlMessage::Error { message, code } => f
                .debug_struct("Error")
                .field("message", message)
                .field("code", code)
                .finish(),
            ControlMessage::PakeChallenge {
                request_id,
                viewer_msg,
                link_id,
            } => f
                .debug_struct("PakeChallenge")
                .field("request_id", request_id)
                .field("viewer_msg", viewer_msg)
                .field("link_id", link_id)
                .finish(),
            ControlMessage::PakeResponse {
                request_id,
                client_id,
                accepted,
                sharer_msg,
                sharer_confirm,
            } => f
                .debug_struct("PakeResponse")
                .field("request_id", request_id)
                .field("client_id", client_id)
                .field("accepted", accepted)
                .field("sharer_msg", sharer_msg)
                .field("sharer_confirm", sharer_confirm)
                .finish(),
            ControlMessage::PakeConfirm {
                request_id,
                viewer_confirm,
            } => f
                .debug_struct("PakeConfirm")
                .field("request_id", request_id)
                .field("viewer_confirm", viewer_confirm)
                .finish(),
            ControlMessage::PakeResult {
                request_id,
                client_id,
                accepted,
            } => f
                .debug_struct("PakeResult")
                .field("request_id", request_id)
                .field("client_id", client_id)
                .field("accepted", accepted)
                .finish(),
            ControlMessage::ClientDisconnected { client_id } => f
                .debug_struct("ClientDisconnected")
                .field("client_id", client_id)
                .finish(),
            ControlMessage::ControlFrameData { client_id, data } => f
                .debug_struct("ControlFrameData")
                .field("client_id", client_id)
                .field("data", data)
                .finish(),
        }
    }
}

impl std::fmt::Debug for TerminalMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TerminalMessage::Ready {
                tunnel_id,
                binding_secret: _,
            } => f
                .debug_struct("Ready")
                .field("tunnel_id", tunnel_id)
                .field("binding_secret", &"<redacted>")
                .finish(),
            TerminalMessage::Error { message, code } => f
                .debug_struct("Error")
                .field("message", message)
                .field("code", code)
                .finish(),
            TerminalMessage::TerminalFrameData { client_id, data } => f
                .debug_struct("TerminalFrameData")
                .field("client_id", client_id)
                .field("data", data)
                .finish(),
        }
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
                requested_slug,
                read_only,
            } => Payload::Auth(proto::TunnelAuth {
                credential: Some(proto::tunnel_auth::Credential::Token(token)),
                session_name,
                protocol_version,
                zellij_version,
                requested_slug,
                read_only,
            }),
            ControlMessage::Established {
                public_url,
                slug,
                tunnel_id,
                terminal_binding_secret,
            } => Payload::Established(proto::TunnelEstablished {
                public_url,
                slug,
                tunnel_id,
                terminal_binding_secret,
            }),
            ControlMessage::Error { message, code } => {
                Payload::Error(code.to_proto_error(message))
            },
            ControlMessage::PakeChallenge {
                request_id,
                viewer_msg,
                link_id,
            } => Payload::PakeChallenge(proto::PakeChallenge {
                request_id,
                viewer_msg,
                link_id,
            }),
            ControlMessage::PakeResponse {
                request_id,
                client_id,
                accepted,
                sharer_msg,
                sharer_confirm,
            } => Payload::PakeResponse(proto::PakeResponse {
                request_id,
                client_id,
                accepted,
                sharer_msg,
                sharer_confirm,
            }),
            ControlMessage::PakeConfirm {
                request_id,
                viewer_confirm,
            } => Payload::PakeConfirm(proto::PakeConfirm {
                request_id,
                viewer_confirm,
            }),
            ControlMessage::PakeResult {
                request_id,
                client_id,
                accepted,
            } => Payload::PakeResult(proto::PakeResult {
                request_id,
                client_id,
                accepted,
            }),
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
            Some(Payload::Auth(a)) => {
                let token = match a.credential {
                    Some(proto::tunnel_auth::Credential::Token(t)) => t,
                    None => return Err(anyhow!("TunnelAuth missing credential")),
                };
                Ok(ControlMessage::Auth {
                    token,
                    session_name: a.session_name,
                    protocol_version: a.protocol_version,
                    zellij_version: a.zellij_version,
                    requested_slug: a.requested_slug,
                    read_only: a.read_only,
                })
            },
            Some(Payload::Established(e)) => Ok(ControlMessage::Established {
                public_url: e.public_url,
                slug: e.slug,
                tunnel_id: e.tunnel_id,
                terminal_binding_secret: e.terminal_binding_secret,
            }),
            Some(Payload::Error(e)) => Ok(ControlMessage::Error {
                code: TunnelErrorCode::from_proto(&e),
                message: e.message,
            }),
            Some(Payload::PakeChallenge(c)) => Ok(ControlMessage::PakeChallenge {
                request_id: c.request_id,
                viewer_msg: c.viewer_msg,
                link_id: c.link_id,
            }),
            Some(Payload::PakeResponse(r)) => Ok(ControlMessage::PakeResponse {
                request_id: r.request_id,
                client_id: r.client_id,
                accepted: r.accepted,
                sharer_msg: r.sharer_msg,
                sharer_confirm: r.sharer_confirm,
            }),
            Some(Payload::PakeConfirm(c)) => Ok(ControlMessage::PakeConfirm {
                request_id: c.request_id,
                viewer_confirm: c.viewer_confirm,
            }),
            Some(Payload::PakeResult(r)) => Ok(ControlMessage::PakeResult {
                request_id: r.request_id,
                client_id: r.client_id,
                accepted: r.accepted,
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
            TerminalMessage::Ready {
                tunnel_id,
                binding_secret,
            } => Payload::Ready(proto::TunnelReady {
                tunnel_id,
                binding_secret,
            }),
            TerminalMessage::Error { message, code } => {
                Payload::Error(code.to_proto_error(message))
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
                binding_secret: r.binding_secret,
            }),
            Some(Payload::Error(e)) => Ok(TerminalMessage::Error {
                code: TunnelErrorCode::from_proto(&e),
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
