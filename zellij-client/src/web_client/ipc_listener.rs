use axum_server::Handle;
use interprocess::local_socket::traits::tokio::Listener;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zellij_utils::consts::{ipc_bind_async, WEBSERVER_SOCKET_PATH};
use zellij_utils::input::{config::Config, options::Options};
use zellij_utils::prost::Message;
use zellij_utils::web_server_commands::{InstructionForWebServer, VersionInfo, WebServerResponse};
use zellij_utils::web_server_contract::web_server_contract::InstructionForWebServer as ProtoInstructionForWebServer;
use zellij_utils::web_server_contract::web_server_contract::WebServerResponse as ProtoWebServerResponse;

use crate::web_client::types::{ClientOsApiFactory, ConnectionTable, SessionManager};

pub async fn create_webserver_receiver(
    id: &str,
) -> Result<interprocess::local_socket::tokio::Stream, Box<dyn std::error::Error + Send + Sync>> {
    std::fs::create_dir_all(&WEBSERVER_SOCKET_PATH.as_path())?;
    let socket_path = WEBSERVER_SOCKET_PATH.join(format!("{}", id));

    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path).await?;
    }

    let listener = ipc_bind_async(&socket_path)?;
    let stream = listener.accept().await?;
    Ok(stream)
}

pub async fn receive_webserver_instruction(
    receiver: &mut interprocess::local_socket::tokio::Stream,
) -> std::io::Result<InstructionForWebServer> {
    // Read length prefix (4 bytes)
    let mut len_bytes = [0u8; 4];
    receiver.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    // Read protobuf message
    let mut buffer = vec![0u8; len];
    receiver.read_exact(&mut buffer).await?;

    // Decode protobuf message
    let proto_instruction = ProtoInstructionForWebServer::decode(&buffer[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Convert to Rust type
    proto_instruction
        .try_into()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

pub async fn send_webserver_response(
    sender: &mut interprocess::local_socket::tokio::Stream,
    response: WebServerResponse,
) -> std::io::Result<()> {
    let proto_response: ProtoWebServerResponse = response.into();
    let encoded = proto_response.encode_to_vec();
    let len = encoded.len() as u32;

    sender.write_all(&len.to_le_bytes()).await?;
    sender.write_all(&encoded).await?;
    sender.flush().await?;

    Ok(())
}

pub struct RelayContext {
    pub connection_table: Arc<Mutex<ConnectionTable>>,
    pub os_api_factory: Arc<dyn ClientOsApiFactory>,
    pub session_manager: Arc<dyn SessionManager>,
    pub config: Arc<Mutex<Config>>,
    pub config_options: Options,
    pub config_file_path: PathBuf,
}

pub async fn listen_to_web_server_instructions(
    server_handle: Handle,
    id: &str,
    web_server_ip: IpAddr,
    web_server_port: u16,
    relay_ctx: RelayContext,
) {
    loop {
        let receiver = create_webserver_receiver(id).await;
        match receiver {
            Ok(mut receiver) => match receive_webserver_instruction(&mut receiver).await {
                Ok(instruction) => match instruction {
                    InstructionForWebServer::ShutdownWebServer => {
                        server_handle.shutdown();
                        break;
                    },
                    InstructionForWebServer::QueryVersion => {
                        let response = WebServerResponse::Version(VersionInfo {
                            version: zellij_utils::consts::VERSION.to_string(),
                            ip: web_server_ip.to_string(),
                            port: web_server_port,
                        });
                        let _ = send_webserver_response(&mut receiver, response).await;
                    },
                    InstructionForWebServer::StartRelayTunnel {
                        client_id,
                        session_name,
                        relay_url,
                        zellij_version,
                    } => {
                        let response = match crate::web_client::relay::start_relay_tunnel(
                            client_id,
                            relay_url,
                            session_name,
                            zellij_version,
                            relay_ctx.connection_table.clone(),
                            relay_ctx.os_api_factory.clone(),
                            relay_ctx.session_manager.clone(),
                            relay_ctx.config.clone(),
                            relay_ctx.config_options.clone(),
                            relay_ctx.config_file_path.clone(),
                        )
                        .await
                        {
                            Ok(public_url) => WebServerResponse::RelayTunnelEstablished {
                                client_id,
                                public_url,
                                slug: String::new(),
                                tunnel_id: String::new(),
                            },
                            Err(e) => {
                                log::error!("Relay tunnel establish failed: {:#}", e);
                                WebServerResponse::RelayTunnelError {
                                    client_id,
                                    message: format!("{:#}", e),
                                }
                            },
                        };
                        let _ = send_webserver_response(&mut receiver, response).await;
                    },
                    InstructionForWebServer::StopRelayTunnel { client_id } => {
                        let _ =
                            crate::web_client::relay::stop_relay_tunnel(client_id).await;
                        let response = WebServerResponse::RelayTunnelStopped { client_id };
                        let _ = send_webserver_response(&mut receiver, response).await;
                    },
                    InstructionForWebServer::GetRelayTunnelStatus { client_id } => {
                        let status_url =
                            crate::web_client::relay::get_relay_tunnel_status_sentinel(
                                client_id,
                            )
                            .await;
                        let response = WebServerResponse::RelayTunnelStatusReport {
                            client_id,
                            status_url,
                        };
                        let _ = send_webserver_response(&mut receiver, response).await;
                    },
                },
                Err(e) => {
                    log::error!("Failed to process web server instruction: {}", e);
                },
            },
            Err(e) => {
                log::error!("Failed to listen to ipc channel: {}", e);
                break;
            },
        }
    }
}
