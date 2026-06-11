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
    /// Phase 6 reconnect support: when reconnecting after an unexpected
    /// drop, the client supplies the previously-issued slug so the relay
    /// can reuse it if still free. Empty string on a fresh handshake. The
    /// relay falls back to a freshly-generated slug if the requested one
    /// is already occupied.
    #[prost(string, tag="5")]
    pub requested_slug: ::prost::alloc::string::String,
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
/// Relay → Zellij: current number of r/o viewers in a fan-out group for a
/// given token hash. `count == 0` signals the group has become dormant and the
/// virtual watcher may be torn down.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ReadOnlyViewerUpdate {
    #[prost(string, tag="1")]
    pub token_hash: ::prost::alloc::string::String,
    #[prost(uint32, tag="2")]
    pub count: u32,
}
/// Zellij → Relay: session-viewport size for a relay-fan-out virtual watcher.
/// Sent when the watcher is first registered and on every sharer-side resize.
/// The relay updates its r/o group state and forwards to every viewer so the
/// client-side clippers can re-emit at the new dimensions.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct SessionSize {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
    #[prost(uint32, tag="2")]
    pub rows: u32,
    #[prost(uint32, tag="3")]
    pub cols: u32,
}
/// Zellij → Relay: tear down any state keyed on this token hash. Emitted when
/// a viewer-auth token is revoked locally on the sharer. The relay removes the
/// matching r/o fan-out group (force-disconnecting every viewer in the group)
/// or — for r/w — force-disconnects the single viewer whose session is keyed
/// on that hash. Also purges the token from any validated-hash cache so a
/// subsequent viewer presenting the same token is rejected at the auth layer.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RevokeToken {
    #[prost(string, tag="1")]
    pub token_hash: ::prost::alloc::string::String,
}
/// Envelope for all control-tunnel messages. All Phase 1 messages that travel
/// on the control tunnel are wrapped in this oneof so that Phase 2+ can add
/// variants (AuthChallenge, ClientConnected, ControlFrame, ...) without
/// migrating the wire format.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ControlFrame {
    #[prost(oneof="control_frame::Payload", tags="1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11")]
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
        #[prost(message, tag="9")]
        ReadOnlyViewerUpdate(super::ReadOnlyViewerUpdate),
        #[prost(message, tag="10")]
        SessionSize(super::SessionSize),
        #[prost(message, tag="11")]
        RevokeToken(super::RevokeToken),
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
