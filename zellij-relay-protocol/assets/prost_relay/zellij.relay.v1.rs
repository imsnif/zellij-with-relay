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
/// Envelope for all control-tunnel messages. All Phase 1 messages that travel
/// on the control tunnel are wrapped in this oneof so that Phase 2+ can add
/// variants (AuthChallenge, ClientConnected, ControlFrame, ...) without
/// migrating the wire format.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlFrame {
    #[prost(oneof="control_frame::Payload", tags="1, 2, 3")]
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
    }
}
/// Envelope for all terminal-tunnel messages.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TerminalFrame {
    #[prost(oneof="terminal_frame::Payload", tags="1, 2")]
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
    }
}
