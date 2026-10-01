use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use zellij_utils::channels::{self, ChannelWithContext, SenderWithContext};
use zellij_utils::errors::{prelude::*, ContextType, RelayContext};
use zellij_utils::input::config::Config;
use zellij_utils::input::options::Options;

use crate::screen::ScreenInstruction;
use crate::thread_bus::Bus;
use crate::{ClientId, ServerInstruction};

#[derive(Debug, Clone)]
pub struct RelayShareRequest {
    pub relay_url: String,
    pub relay_tunnel_auth_token: String,
    pub config: Config,
    pub config_options: Options,
    pub config_file_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
enum RelayInstruction {
    StartShare(RelayShareRequest),
    StopShare(ClientId),
    Exit,
}

impl From<&RelayInstruction> for RelayContext {
    fn from(instruction: &RelayInstruction) -> Self {
        match instruction {
            RelayInstruction::StartShare(..) => RelayContext::StartShare,
            RelayInstruction::StopShare(..) => RelayContext::StopShare,
            RelayInstruction::Exit => RelayContext::Exit,
        }
    }
}

pub struct RelayConnection {
    sender: SenderWithContext<RelayInstruction>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RelayConnection {
    pub fn spawn(
        to_screen: &SenderWithContext<ScreenInstruction>,
        to_server: &SenderWithContext<ServerInstruction>,
        relay_share_active: Arc<AtomicBool>,
        session_name: String,
        zellij_version: String,
    ) -> Self {
        let (sender, receiver): ChannelWithContext<RelayInstruction> = channels::unbounded();
        let sender = SenderWithContext::new(sender);
        let thread = thread::Builder::new()
            .name("relay_connections".to_string())
            .spawn({
                let bus = Bus::new(
                    vec![receiver],
                    Some(to_screen),
                    None,
                    None,
                    Some(to_server),
                    None,
                    None,
                    None,
                );
                move || {
                    relay_connections_main(bus, relay_share_active, session_name, zellij_version)
                        .fatal()
                }
            })
            .unwrap();
        RelayConnection {
            sender,
            thread: Some(thread),
        }
    }

    pub fn start_share(&self, request: RelayShareRequest) {
        let _ = self.sender.send(RelayInstruction::StartShare(request));
    }

    pub fn stop_share(&self, client_id: ClientId) {
        let _ = self.sender.send(RelayInstruction::StopShare(client_id));
    }
}

impl Drop for RelayConnection {
    fn drop(&mut self) {
        log::info!("relay-debug: RelayConnection dropped, sending Exit");
        let _ = self.sender.send(RelayInstruction::Exit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        log::info!("relay-debug: relay_connections thread joined");
    }
}

fn relay_connections_main(
    bus: Bus<RelayInstruction>,
    relay_share_active: Arc<AtomicBool>,
    session_name: String,
    zellij_version: String,
) -> Result<()> {
    let err_context = || "failed to manage relay connections".to_string();
    let runtime = crate::global_async_runtime::get_tokio_runtime();
    let registry_lock = Arc::new(tokio::sync::Mutex::new(()));

    loop {
        let (event, mut err_ctx) = bus.recv().with_context(err_context)?;
        err_ctx.add_call(ContextType::Relay((&event).into()));
        match event {
            RelayInstruction::StartShare(request) => {
                start_share(
                    &bus,
                    &relay_share_active,
                    &registry_lock,
                    &session_name,
                    &zellij_version,
                    request,
                );
            },
            RelayInstruction::StopShare(client_id) => {
                log::info!("relay-debug: StopShare from client_id={}", client_id);
                relay_share_active.store(false, Ordering::Relaxed);
                let senders = bus.senders.clone();
                let registry_lock = registry_lock.clone();
                runtime.spawn(async move {
                    let _guard = registry_lock.lock().await;
                    zellij_relay_client::stop_relay_tunnel().await;
                    drop(_guard);
                    let _ = senders.send_to_screen(ScreenInstruction::RelayShareStatusChange(None));
                });
            },
            RelayInstruction::Exit => {
                log::info!(
                    "relay-debug: relay_connections Exit, relay_share_active={}, tunnel is NOT explicitly stopped here",
                    relay_share_active.load(Ordering::Relaxed)
                );
                return Ok(());
            },
        }
    }
}

fn start_share(
    bus: &Bus<RelayInstruction>,
    relay_share_active: &Arc<AtomicBool>,
    registry_lock: &Arc<tokio::sync::Mutex<()>>,
    session_name: &str,
    zellij_version: &str,
    request: RelayShareRequest,
) {
    use zellij_relay_client::relay_share_status;

    let runtime = crate::global_async_runtime::get_tokio_runtime();

    if relay_share_active.load(Ordering::Relaxed) {
        let senders = bus.senders.clone();
        runtime.spawn(async move {
            let _ = senders.send_to_screen(ScreenInstruction::RelayShareStatusChange(
                relay_share_status().await,
            ));
        });
        return;
    }
    relay_share_active.store(true, Ordering::Relaxed);

    let RelayShareRequest {
        relay_url,
        relay_tunnel_auth_token,
        config,
        config_options,
        config_file_path,
    } = request;

    let senders = bus.senders.clone();
    let relay_share_active = relay_share_active.clone();
    let registry_lock = registry_lock.clone();
    let session_name = session_name.to_string();
    let zellij_version = zellij_version.to_string();

    runtime.spawn(async move {
        use std::sync::{Arc as RtArc, Mutex as RtMutex};
        use zellij_browser_bridge::factory::LocalSessionSource;
        use zellij_browser_bridge::io::LoopbackLinkFactory;
        use zellij_browser_bridge::{BrowserBridge, SessionSource};
        use zellij_relay_client::relay_error::failure_reason_from_error;
        use zellij_relay_client::{
            relay_share_status, start_relay_tunnel, stop_relay_tunnel, RelayPluginEvent,
            RelayTunnelStatus,
        };
        use zellij_utils::data::RelayShareStatus;

        let (status_tx, mut status_rx) =
            tokio::sync::mpsc::unbounded_channel::<RelayTunnelStatus>();
        let (relay_event_tx, mut relay_event_rx) =
            tokio::sync::mpsc::unbounded_channel::<RelayPluginEvent>();

        {
            let senders = senders.clone();
            tokio::spawn(async move {
                while let Some(event) = relay_event_rx.recv().await {
                    match event {
                        RelayPluginEvent::AdmissionPending => {
                            let _ = senders
                                .send_to_screen(ScreenInstruction::LaunchOrFocusSharePlugin);
                        },
                        RelayPluginEvent::DevicesChanged => {
                            let _ = senders
                                .send_to_screen(ScreenInstruction::RefreshSharePlugin);
                        },
                        RelayPluginEvent::GuestConnectionsChanged => {
                            let _ = senders
                                .send_to_screen(ScreenInstruction::RefreshSharePlugin);
                        },
                    }
                }
            });
        }

        let primary = {
            let _guard = registry_lock.lock().await;
            stop_relay_tunnel().await;

            let default_size = zellij_utils::pane_size::Size { rows: 24, cols: 80 };
            let session_source: RtArc<dyn SessionSource> = RtArc::new(LocalSessionSource);
            let bridge = BrowserBridge::new(session_source);
            let os_api_factory = RtArc::new(LoopbackLinkFactory::new(default_size));
            let config_arc = RtArc::new(RtMutex::new(config));
            let config_file_path = config_file_path.unwrap_or_default();

            start_relay_tunnel(
                relay_url.clone(),
                session_name.clone(),
                zellij_version.clone(),
                relay_tunnel_auth_token.clone(),
                status_tx.clone(),
                relay_event_tx.clone(),
                bridge.clone(),
                os_api_factory.clone(),
                config_arc.clone(),
                config_options.clone(),
                config_file_path.clone(),
            )
            .await
        };

        let established = match &primary {
            Ok(_) => true,
            Err(e) => {
                log::error!("Relay tunnel error: {:#}", e);
                relay_share_active.store(false, Ordering::Relaxed);
                false
            },
        };

        drop(status_tx);
        drop(relay_event_tx);

        let combined = if established {
            relay_share_status().await
        } else {
            primary.err().map(|e| RelayShareStatus::Failed {
                reason: failure_reason_from_error(&e),
                message: format!("{}", e),
            })
        };
        let _ = senders.send_to_screen(ScreenInstruction::RelayShareStatusChange(combined.clone()));

        if established {
            let mut last_forwarded_status = combined;
            while status_rx.recv().await.is_some() {
                let next = relay_share_status().await;
                if next != last_forwarded_status {
                    let _ = senders
                        .send_to_screen(ScreenInstruction::RelayShareStatusChange(next.clone()));
                    last_forwarded_status = next;
                }
            }
            relay_share_active.store(false, Ordering::Relaxed);
            log::info!("relay-debug: relay status channel closed, share marked inactive");
        }
    });
}
