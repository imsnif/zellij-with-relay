use crate::errors::prelude::*;
use crate::web_server_commands::{
    InstructionForWebServer as RustInstructionForWebServer, VersionInfo, WebServerResponse,
};
use crate::web_server_contract::web_server_contract::{
    instruction_for_web_server, web_server_response, GetRelayTunnelStatusMsg,
    InstructionForWebServer as ProtoInstructionForWebServer, QueryVersionMsg,
    RelayTunnelErrorMsg, RelayTunnelEstablishedMsg, RelayTunnelStatusReportMsg,
    RelayTunnelStoppedMsg, RevokeRelayTokenMsg, ShutdownWebServerMsg, StartRelayTunnelMsg,
    StopRelayTunnelMsg, VersionResponseMsg, WebServerResponse as ProtoWebServerResponse,
};

// Convert Rust InstructionForWebServer to protobuf
impl From<RustInstructionForWebServer> for ProtoInstructionForWebServer {
    fn from(instruction: RustInstructionForWebServer) -> Self {
        let instruction = match instruction {
            RustInstructionForWebServer::ShutdownWebServer => {
                instruction_for_web_server::Instruction::ShutdownWebServer(ShutdownWebServerMsg {})
            },
            RustInstructionForWebServer::QueryVersion => {
                instruction_for_web_server::Instruction::QueryVersion(QueryVersionMsg {})
            },
            RustInstructionForWebServer::StartRelayTunnel {
                client_id,
                session_name,
                relay_url,
                zellij_version,
                relay_tunnel_auth_token,
            } => instruction_for_web_server::Instruction::StartRelayTunnel(StartRelayTunnelMsg {
                client_id: client_id as u32,
                session_name,
                relay_url,
                zellij_version,
                relay_tunnel_auth_token,
            }),
            RustInstructionForWebServer::StopRelayTunnel { client_id } => {
                instruction_for_web_server::Instruction::StopRelayTunnel(StopRelayTunnelMsg {
                    client_id: client_id as u32,
                })
            },
            RustInstructionForWebServer::GetRelayTunnelStatus { client_id } => {
                instruction_for_web_server::Instruction::GetRelayTunnelStatus(
                    GetRelayTunnelStatusMsg {
                        client_id: client_id as u32,
                    },
                )
            },
            RustInstructionForWebServer::RevokeRelayToken { token_hash } => {
                instruction_for_web_server::Instruction::RevokeRelayToken(RevokeRelayTokenMsg {
                    token_hash,
                })
            },
        };

        ProtoInstructionForWebServer {
            instruction: Some(instruction),
        }
    }
}

// Convert protobuf InstructionForWebServer to Rust
impl TryFrom<ProtoInstructionForWebServer> for RustInstructionForWebServer {
    type Error = anyhow::Error;

    fn try_from(proto_instruction: ProtoInstructionForWebServer) -> Result<Self> {
        match proto_instruction.instruction {
            Some(instruction_for_web_server::Instruction::ShutdownWebServer(_)) => {
                Ok(RustInstructionForWebServer::ShutdownWebServer)
            },
            Some(instruction_for_web_server::Instruction::QueryVersion(_)) => {
                Ok(RustInstructionForWebServer::QueryVersion)
            },
            Some(instruction_for_web_server::Instruction::StartRelayTunnel(msg)) => {
                Ok(RustInstructionForWebServer::StartRelayTunnel {
                    client_id: msg.client_id as u16,
                    session_name: msg.session_name,
                    relay_url: msg.relay_url,
                    zellij_version: msg.zellij_version,
                    relay_tunnel_auth_token: msg.relay_tunnel_auth_token,
                })
            },
            Some(instruction_for_web_server::Instruction::StopRelayTunnel(msg)) => {
                Ok(RustInstructionForWebServer::StopRelayTunnel {
                    client_id: msg.client_id as u16,
                })
            },
            Some(instruction_for_web_server::Instruction::GetRelayTunnelStatus(msg)) => {
                Ok(RustInstructionForWebServer::GetRelayTunnelStatus {
                    client_id: msg.client_id as u16,
                })
            },
            Some(instruction_for_web_server::Instruction::RevokeRelayToken(msg)) => {
                Ok(RustInstructionForWebServer::RevokeRelayToken {
                    token_hash: msg.token_hash,
                })
            },
            None => Err(anyhow!("Missing instruction in InstructionForWebServer")),
        }
    }
}

// Convert Rust WebServerResponse to protobuf
impl From<WebServerResponse> for ProtoWebServerResponse {
    fn from(response: WebServerResponse) -> Self {
        let response = match response {
            WebServerResponse::Version(version_info) => {
                web_server_response::Response::Version(VersionResponseMsg {
                    version: version_info.version,
                    ip: version_info.ip,
                    port: version_info.port as u32,
                })
            },
            WebServerResponse::RelayTunnelEstablished {
                client_id,
                public_url,
                slug,
                tunnel_id,
            } => web_server_response::Response::RelayTunnelEstablished(RelayTunnelEstablishedMsg {
                client_id: client_id as u32,
                public_url,
                slug,
                tunnel_id,
            }),
            WebServerResponse::RelayTunnelStopped { client_id } => {
                web_server_response::Response::RelayTunnelStopped(RelayTunnelStoppedMsg {
                    client_id: client_id as u32,
                })
            },
            WebServerResponse::RelayTunnelError { client_id, message } => {
                web_server_response::Response::RelayTunnelError(RelayTunnelErrorMsg {
                    client_id: client_id as u32,
                    message,
                })
            },
            WebServerResponse::RelayTunnelStatusReport {
                client_id,
                status_url,
            } => web_server_response::Response::RelayTunnelStatusReport(
                RelayTunnelStatusReportMsg {
                    client_id: client_id as u32,
                    status_url,
                },
            ),
        };

        ProtoWebServerResponse {
            response: Some(response),
        }
    }
}

// Convert protobuf WebServerResponse to Rust
impl TryFrom<ProtoWebServerResponse> for WebServerResponse {
    type Error = anyhow::Error;

    fn try_from(proto_response: ProtoWebServerResponse) -> Result<Self> {
        match proto_response.response {
            Some(web_server_response::Response::Version(version_msg)) => {
                Ok(WebServerResponse::Version(VersionInfo {
                    version: version_msg.version,
                    ip: version_msg.ip,
                    port: version_msg.port as u16,
                }))
            },
            Some(web_server_response::Response::RelayTunnelEstablished(msg)) => {
                Ok(WebServerResponse::RelayTunnelEstablished {
                    client_id: msg.client_id as u16,
                    public_url: msg.public_url,
                    slug: msg.slug,
                    tunnel_id: msg.tunnel_id,
                })
            },
            Some(web_server_response::Response::RelayTunnelStopped(msg)) => {
                Ok(WebServerResponse::RelayTunnelStopped {
                    client_id: msg.client_id as u16,
                })
            },
            Some(web_server_response::Response::RelayTunnelError(msg)) => {
                Ok(WebServerResponse::RelayTunnelError {
                    client_id: msg.client_id as u16,
                    message: msg.message,
                })
            },
            Some(web_server_response::Response::RelayTunnelStatusReport(msg)) => {
                Ok(WebServerResponse::RelayTunnelStatusReport {
                    client_id: msg.client_id as u16,
                    status_url: msg.status_url,
                })
            },
            None => Err(anyhow!("Missing response in WebServerResponse")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_server_commands::{InstructionForWebServer, WebServerResponse};

    fn roundtrip_instruction(original: InstructionForWebServer) -> InstructionForWebServer {
        let proto: ProtoInstructionForWebServer = original.into();
        proto.try_into().expect("roundtrip ok")
    }

    fn roundtrip_response(original: WebServerResponse) -> WebServerResponse {
        let proto: ProtoWebServerResponse = original.into();
        proto.try_into().expect("roundtrip ok")
    }

    #[test]
    fn start_relay_tunnel_roundtrip() {
        let original = InstructionForWebServer::StartRelayTunnel {
            client_id: 7,
            session_name: "foo".into(),
            relay_url: "ws://x".into(),
            zellij_version: "0.45.0".into(),
            relay_tunnel_auth_token: "secret-token".into(),
        };
        let decoded = roundtrip_instruction(original.clone());
        match (original, decoded) {
            (
                InstructionForWebServer::StartRelayTunnel {
                    client_id: a1,
                    session_name: a2,
                    relay_url: a3,
                    zellij_version: a4,
                    relay_tunnel_auth_token: a5,
                },
                InstructionForWebServer::StartRelayTunnel {
                    client_id: b1,
                    session_name: b2,
                    relay_url: b3,
                    zellij_version: b4,
                    relay_tunnel_auth_token: b5,
                },
            ) => {
                assert_eq!(a1, b1);
                assert_eq!(a2, b2);
                assert_eq!(a3, b3);
                assert_eq!(a4, b4);
                assert_eq!(a5, b5);
            },
            (_, other) => panic!("expected StartRelayTunnel, got {:?}", other),
        }

        use crate::web_server_contract::web_server_contract::instruction_for_web_server::Instruction;
        let proto: ProtoInstructionForWebServer = InstructionForWebServer::StartRelayTunnel {
            client_id: 1,
            session_name: "n".into(),
            relay_url: "r".into(),
            zellij_version: "v".into(),
            relay_tunnel_auth_token: String::new(),
        }
        .into();
        assert!(matches!(
            proto.instruction,
            Some(Instruction::StartRelayTunnel(_))
        ));
    }

    #[test]
    fn stop_relay_tunnel_roundtrip() {
        let original = InstructionForWebServer::StopRelayTunnel { client_id: 42 };
        let decoded = roundtrip_instruction(original.clone());
        match decoded {
            InstructionForWebServer::StopRelayTunnel { client_id } => {
                assert_eq!(client_id, 42);
            },
            other => panic!("expected StopRelayTunnel, got {:?}", other),
        }
    }

    #[test]
    fn relay_tunnel_established_response_roundtrip() {
        let original = WebServerResponse::RelayTunnelEstablished {
            client_id: 3,
            public_url: "http://x/r/abc".into(),
            slug: "abc".into(),
            tunnel_id: "t-1".into(),
        };
        match roundtrip_response(original) {
            WebServerResponse::RelayTunnelEstablished {
                client_id,
                public_url,
                slug,
                tunnel_id,
            } => {
                assert_eq!(client_id, 3);
                assert_eq!(public_url, "http://x/r/abc");
                assert_eq!(slug, "abc");
                assert_eq!(tunnel_id, "t-1");
            },
            other => panic!("wrong variant: {:?}", other),
        }
    }

    #[test]
    fn relay_tunnel_stopped_response_roundtrip() {
        let original = WebServerResponse::RelayTunnelStopped { client_id: 9 };
        match roundtrip_response(original) {
            WebServerResponse::RelayTunnelStopped { client_id } => assert_eq!(client_id, 9),
            other => panic!("wrong variant: {:?}", other),
        }
    }

    #[test]
    fn relay_tunnel_error_response_roundtrip() {
        let original = WebServerResponse::RelayTunnelError {
            client_id: 11,
            message: "nope".into(),
        };
        match roundtrip_response(original) {
            WebServerResponse::RelayTunnelError { client_id, message } => {
                assert_eq!(client_id, 11);
                assert_eq!(message, "nope");
            },
            other => panic!("wrong variant: {:?}", other),
        }
    }
}
