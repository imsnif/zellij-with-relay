#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InstructionForWebServer {
    #[prost(oneof="instruction_for_web_server::Instruction", tags="1, 2, 3, 4")]
    pub instruction: ::core::option::Option<instruction_for_web_server::Instruction>,
}
/// Nested message and enum types in `InstructionForWebServer`.
pub mod instruction_for_web_server {
    #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Instruction {
        #[prost(message, tag="1")]
        ShutdownWebServer(super::ShutdownWebServerMsg),
        #[prost(message, tag="2")]
        QueryVersion(super::QueryVersionMsg),
        #[prost(message, tag="3")]
        StartRelayTunnel(super::StartRelayTunnelMsg),
        #[prost(message, tag="4")]
        StopRelayTunnel(super::StopRelayTunnelMsg),
    }
}
/// Empty for now, but allows for future parameters like graceful timeout
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ShutdownWebServerMsg {
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct QueryVersionMsg {
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StartRelayTunnelMsg {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
    #[prost(string, tag="2")]
    pub session_name: ::prost::alloc::string::String,
    #[prost(string, tag="3")]
    pub relay_url: ::prost::alloc::string::String,
    #[prost(string, tag="4")]
    pub zellij_version: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StopRelayTunnelMsg {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct WebServerResponse {
    #[prost(oneof="web_server_response::Response", tags="1, 2, 3, 4")]
    pub response: ::core::option::Option<web_server_response::Response>,
}
/// Nested message and enum types in `WebServerResponse`.
pub mod web_server_response {
    #[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Response {
        #[prost(message, tag="1")]
        Version(super::VersionResponseMsg),
        #[prost(message, tag="2")]
        RelayTunnelEstablished(super::RelayTunnelEstablishedMsg),
        #[prost(message, tag="3")]
        RelayTunnelStopped(super::RelayTunnelStoppedMsg),
        #[prost(message, tag="4")]
        RelayTunnelError(super::RelayTunnelErrorMsg),
    }
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct VersionResponseMsg {
    #[prost(string, tag="1")]
    pub version: ::prost::alloc::string::String,
    #[prost(string, tag="2")]
    pub ip: ::prost::alloc::string::String,
    #[prost(uint32, tag="3")]
    pub port: u32,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RelayTunnelEstablishedMsg {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
    #[prost(string, tag="2")]
    pub public_url: ::prost::alloc::string::String,
    #[prost(string, tag="3")]
    pub slug: ::prost::alloc::string::String,
    #[prost(string, tag="4")]
    pub tunnel_id: ::prost::alloc::string::String,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RelayTunnelStoppedMsg {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
}
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct RelayTunnelErrorMsg {
    #[prost(uint32, tag="1")]
    pub client_id: u32,
    #[prost(string, tag="2")]
    pub message: ::prost::alloc::string::String,
}
