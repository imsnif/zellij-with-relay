/// Tunnel handshake: sent by the Zellij instance as the first message on the
/// control WebSocket.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TunnelAuth {
    /// placeholder during Phase 1 (future: real token)
    #[prost(string, tag="1")]
    pub token: ::prost::alloc::string::String,
    #[prost(string, tag="2")]
    pub session_name: ::prost::alloc::string::String,
    #[prost(uint32, tag="3")]
    pub protocol_version: u32,
    #[prost(string, tag="4")]
    pub zellij_version: ::prost::alloc::string::String,
}
/// Relay response to a successful TunnelAuth.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TunnelEstablished {
    #[prost(string, tag="1")]
    pub public_url: ::prost::alloc::string::String,
    #[prost(string, tag="2")]
    pub slug: ::prost::alloc::string::String,
    #[prost(string, tag="3")]
    pub tunnel_id: ::prost::alloc::string::String,
}
/// Error surface on either the control or the terminal tunnel.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TunnelError {
    #[prost(string, tag="1")]
    pub message: ::prost::alloc::string::String,
}
/// Terminal tunnel linking message sent by the Zellij instance once the
/// terminal WebSocket is opened for a previously-established tunnel.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TunnelReady {
    #[prost(string, tag="1")]
    pub tunnel_id: ::prost::alloc::string::String,
}
/// Relay → Zellij: validate this token hash and, if accepted, allocate a
/// client_id. The relay echoes `request_id` in the matching AuthResponse so
/// multiple in-flight challenges can be demultiplexed.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AuthChallenge {
    #[prost(bytes="vec", tag="1")]
    pub request_id: ::prost::alloc::vec::Vec<u8>,
    #[prost(string, tag="2")]
    pub token_hash: ::prost::alloc::string::String,
}
/// Zellij → Relay: result of an AuthChallenge.
///
/// `e2e_encrypted` tells the relay (and, via `/session`, the viewer) whether
/// the Zellij side will encrypt the TerminalFrameData payloads it sends for
/// this client. Relay-path viewers always see `true` in Phase 3+; local-web
/// viewers see `true` only when the sharer opted in via `encrypt_web_sharing`.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AuthResponse {
    #[prost(bytes="vec", tag="1")]
    pub request_id: ::prost::alloc::vec::Vec<u8>,
    #[prost(uint32, tag="2")]
    pub client_id: u32,
    #[prost(bool, tag="3")]
    pub accepted: bool,
    #[prost(bool, tag="4")]
    pub is_read_only: bool,
    #[prost(string, tag="5")]
    pub session_token_hash: ::prost::alloc::string::String,
    #[prost(bool, tag="6")]
    pub e2e_encrypted: bool,
}
/// Relay → Zellij: a new viewer has attached to the tunnel.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClientConnected {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
}
/// Relay → Zellij (or Zellij → Relay on server-initiated disconnect): the
/// viewer is gone. After this frame, no additional frames for `client_id`
/// will be generated.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClientDisconnected {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
}
/// Per-client control-plane payload (text WebSocket frames in the local
/// web client's JSON control protocol).
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlFrameData {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
    #[prost(bytes="vec", tag="2")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}
/// Per-client terminal-plane payload. Phase 2: plaintext. Phase 3+: ciphertext
/// (`nonce || ciphertext`).
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TerminalFrameData {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
    #[prost(bytes="vec", tag="2")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}
/// Envelope for all control-tunnel messages. All Phase 1 messages that travel
/// on the control tunnel are wrapped in this oneof so that Phase 2+ can add
/// variants (AuthChallenge, ClientConnected, ControlFrame, ...) without
/// migrating the wire format.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlFrame {
    #[prost(oneof="control_frame::Payload", tags="1, 2, 3, 4, 5, 6, 7, 8")]
    pub payload: ::core::option::Option<control_frame::Payload>,
}
/// Nested message and enum types in `ControlFrame`.
pub mod control_frame {
    #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Payload {
        #[prost(message, tag="1")]
        Auth(super::TunnelAuth),
        #[prost(message, tag="2")]
        Established(super::TunnelEstablished),
        #[prost(message, tag="3")]
        Error(super::TunnelError),
        #[prost(message, tag="4")]
        AuthChallenge(super::AuthChallenge),
        #[prost(message, tag="5")]
        AuthResponse(super::AuthResponse),
        #[prost(message, tag="6")]
        ClientConnected(super::ClientConnected),
        #[prost(message, tag="7")]
        ClientDisconnected(super::ClientDisconnected),
        #[prost(message, tag="8")]
        ControlFrameData(super::ControlFrameData),
    }
}
/// Envelope for all terminal-tunnel messages.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TerminalFrame {
    #[prost(oneof="terminal_frame::Payload", tags="1, 2, 3")]
    pub payload: ::core::option::Option<terminal_frame::Payload>,
}
/// Nested message and enum types in `TerminalFrame`.
pub mod terminal_frame {
    #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Payload {
        #[prost(message, tag="1")]
        Ready(super::TunnelReady),
        #[prost(message, tag="2")]
        Error(super::TunnelError),
        #[prost(message, tag="3")]
        TerminalFrameData(super::TerminalFrameData),
    }
}
