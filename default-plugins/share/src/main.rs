mod list;
mod overlay;
mod screen_devices;
mod screen_local;
mod screen_main;
mod ui_components;

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use zellij_tile::prelude::*;

use screen_devices::{DeviceRow, DevicesView};
use ui_components::{Block, NavItem};
use screen_local::LocalView;
use screen_main::OnlineView;

static MESSAGE_DISMISS_DURATION: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Screen {
    #[default]
    Main,
    Devices,
    Local,
}

const ALL_SCREENS: [Screen; 3] = [Screen::Main, Screen::Devices, Screen::Local];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prompt {
    NewLink { read_only: bool },
    NewEnrollment { read_only: bool },
    NewToken { read_only: bool },
    RenameToken,
}

impl Prompt {
    fn label(&self) -> &'static str {
        match self {
            Prompt::NewLink { read_only: false } => "New full-control link: ",
            Prompt::NewLink { read_only: true } => "New read-only link: ",
            Prompt::NewEnrollment { read_only: false } => "New full-control enrollment: ",
            Prompt::NewEnrollment { read_only: true } => "New read-only enrollment: ",
            Prompt::NewToken { read_only: false } => "New token name: ",
            Prompt::NewToken { read_only: true } => "New read-only token name: ",
            Prompt::RenameToken => "Rename token: ",
        }
    }
}

#[derive(Debug, Default)]
struct Input {
    prompt: Option<Prompt>,
    buffer: String,
}

impl Input {
    fn start(&mut self, prompt: Prompt) {
        self.prompt = Some(prompt);
        self.buffer.clear();
    }

    fn active(&self) -> bool {
        self.prompt.is_some()
    }

    fn cancel(&mut self) -> bool {
        self.buffer.clear();
        self.prompt.take().is_some()
    }

    fn take(&mut self) -> Option<(Prompt, String)> {
        let prompt = self.prompt.take()?;
        Some((prompt, std::mem::take(&mut self.buffer)))
    }

    fn view(&self) -> Option<(String, &str)> {
        self.prompt
            .map(|prompt| (prompt.label().to_owned(), self.buffer.as_str()))
    }
}

#[derive(Debug, Default)]
struct WebServerState {
    started: bool,
    sharing: WebSharing,
    different_version_error: Option<String>,
    ip: Option<IpAddr>,
    port: Option<u16>,
    base_url: String,
    capability: bool,
    relay_share_status: Option<RelayShareStatus>,
}

#[derive(Debug, Default)]
struct UiState {
    hover_coordinates: Option<(usize, usize)>,
    clickable_urls: HashMap<CoordinatesInLine, String>,
    link_executable: Option<&'static str>,
}

#[derive(Debug, Default)]
struct App {
    web: WebServerState,
    ui: UiState,
    screen: Screen,
    session_name: Option<String>,
    own_plugin_id: Option<u32>,

    links: Vec<GuestLink>,
    link_selected: Option<usize>,

    devices: Vec<EnrolledDevice>,
    enrollments: Vec<GuestLink>,
    device_selected: Option<usize>,

    tokens: Vec<(String, String, bool)>,
    token_selected: Option<usize>,
    new_token: Option<(String, String)>,

    admissions: Vec<PendingAdmission>,
    admission_selected: Option<usize>,
    admission_dismissed: bool,

    input: Input,
    signin: Option<String>,
    pending_mint: Option<bool>,
    revealed_link: Option<Vec<u8>>,
    confirming_revoke_all: bool,
    help_visible: bool,

    info: Option<String>,
    error: Option<String>,
    message_timer_armed: bool,
    selection_support: bool,
}

register_plugin!(App);

impl ZellijPlugin for App {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        subscribe(&[
            EventType::Key,
            EventType::ModeUpdate,
            EventType::WebServerStatus,
            EventType::Mouse,
            EventType::RunCommandResult,
            EventType::FailedToStartWebServer,
            EventType::Timer,
            EventType::PastedText,
        ]);
        self.own_plugin_id = Some(get_plugin_ids().plugin_id);
        self.refresh_tokens();
        self.query_link_executable();
        self.sync_pane_title();
    }

    fn update(&mut self, event: Event) -> bool {
        if !self.web.capability && !matches!(event, Event::ModeUpdate(_)) {
            return false;
        }

        let should_render = match event {
            Event::Timer(_) => self.handle_message_dismissal(),
            Event::ModeUpdate(mode_info) => self.handle_mode_update(mode_info),
            Event::WebServerStatus(status) => self.handle_web_server_status(status),
            Event::Key(key) => self.handle_key_input(key),
            Event::Mouse(mouse_event) => self.handle_mouse_event(mouse_event),
            Event::RunCommandResult(exit_code, _stdout, _stderr, context) => {
                self.handle_command_result(exit_code, context)
            },
            Event::FailedToStartWebServer(error) => {
                self.error = Some(error);
                true
            },
            Event::PastedText(text) => self.handle_pasted_text(text),
            _ => false,
        };
        self.arm_message_timer_if_needed();
        self.sync_selection_support();
        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        let should_render = match pipe_message.name.as_str() {
            "share_admission_pending" => self.handle_admission_pending_pipe(),
            "share_devices_changed" => self.handle_devices_changed_pipe(),
            _ => false,
        };
        self.arm_message_timer_if_needed();
        self.sync_selection_support();
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if !self.web.capability {
            overlay::render_no_capability(rows, cols);
            return;
        }

        let mut clickable = std::mem::take(&mut self.ui.clickable_urls);
        clickable.clear();

        let message = self.message();
        let message = message
            .as_ref()
            .map(|(text, is_error)| (text.as_str(), *is_error));

        if self.help_visible {
            overlay::render_help_screen(rows, cols);
        } else if let Some(buffer) = self.signin.as_deref() {
            overlay::render_signin(rows, cols, buffer, message);
        } else if self.admission_overlay_visible() {
            overlay::render_admission(
                rows,
                cols,
                &self.admissions,
                self.admission_selected,
                message,
            );
        } else {
            let frame = ui_components::shared_frame(
                ALL_SCREENS
                    .iter()
                    .map(|screen| self.screen_blocks(*screen, message))
                    .collect(),
                rows,
                cols,
            );
            ui_components::render_in_frame(
                self.screen_blocks(self.screen, message),
                frame,
                rows,
                cols,
                self.ui.hover_coordinates,
                &mut clickable,
            );
        }

        self.ui.clickable_urls = clickable;
    }
}

impl App {
    fn message(&self) -> Option<(String, bool)> {
        if let Some(error) = &self.error {
            Some((error.clone(), true))
        } else {
            self.info.clone().map(|info| (info, false))
        }
    }

    fn screen_blocks(&self, screen: Screen, message: Option<(&str, bool)>) -> Vec<Block> {
        let active = screen == self.screen;
        let message = message.filter(|_| active);
        let prompt = if active { self.input.view() } else { None };
        match screen {
            Screen::Main => screen_main::blocks(OnlineView {
                nav: self.nav_items(),
                status: self.web.relay_share_status.as_ref(),
                pending: self.pending_mint.is_some(),
                links: &self.links,
                selected: self.link_selected,
                revealed: self.revealed_link.as_deref(),
                prompt,
                pending_admissions: if self.admission_dismissed {
                    self.admissions.len()
                } else {
                    0
                },
                message,
            }),
            Screen::Devices => screen_devices::blocks(DevicesView {
                nav: self.nav_items(),
                devices: &self.devices,
                enrollments: &self.enrollments,
                selected: self.device_selected,
                revealed: self.revealed_link.as_deref(),
                prompt,
                message,
            }),
            Screen::Local => screen_local::blocks(LocalView {
                nav: self.nav_items(),
                server_started: self.web.started,
                server_base_url: &self.web.base_url,
                server_version_error: self.web.different_version_error.as_deref(),
                server_ip: self.web.ip,
                server_port: self.web.port,
                session_name: self.session_name.as_deref(),
                sharing: self.web.sharing,
                tokens: &self.tokens,
                selected: self.token_selected,
                new_token: self
                    .new_token
                    .as_ref()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
                prompt,
                confirming_revoke_all: active && self.confirming_revoke_all,
                message,
            }),
        }
    }

    fn relay_live(&self) -> bool {
        matches!(
            &self.web.relay_share_status,
            Some(RelayShareStatus::Connected { url: Some(_) })
        )
    }

    fn relay_auth_rejected(&self) -> bool {
        matches!(
            &self.web.relay_share_status,
            Some(RelayShareStatus::Failed {
                reason: RelayFailureReason::AuthRejected,
                ..
            })
        )
    }

    fn admission_overlay_visible(&self) -> bool {
        !self.admissions.is_empty() && !self.admission_dismissed
    }

    fn has_message(&self) -> bool {
        self.info.is_some() || self.error.is_some()
    }

    fn arm_message_timer_if_needed(&mut self) {
        if self.has_message() && !self.message_timer_armed {
            self.message_timer_armed = true;
            set_timeout(MESSAGE_DISMISS_DURATION);
        }
    }

    fn handle_message_dismissal(&mut self) -> bool {
        self.message_timer_armed = false;
        if self.has_message() {
            self.info = None;
            self.error = None;
            return true;
        }
        false
    }

    fn clear_messages(&mut self) -> bool {
        let cleared = self.info.take().is_some() || self.error.take().is_some();
        if cleared {
            self.message_timer_armed = false;
        }
        cleared
    }

    fn nav_items(&self) -> Vec<NavItem> {
        let devices_enabled = self.relay_live();
        vec![
            NavItem {
                label: "Online".to_owned(),
                selected: self.screen == Screen::Main,
                enabled: true,
            },
            NavItem {
                label: format!("Devices ({})", self.devices.len()),
                selected: self.screen == Screen::Devices,
                enabled: devices_enabled,
            },
            NavItem {
                label: "Local".to_owned(),
                selected: self.screen == Screen::Local,
                enabled: true,
            },
        ]
    }

    fn next_screen(&self) -> Screen {
        let order = ALL_SCREENS;
        let current = order
            .iter()
            .position(|screen| *screen == self.screen)
            .unwrap_or(0);
        for step in 1..=order.len() {
            let candidate = order[(current + step) % order.len()];
            if candidate != Screen::Devices || self.relay_live() {
                return candidate;
            }
        }
        self.screen
    }

    fn sync_pane_title(&self) {
        if let Some(plugin_id) = self.own_plugin_id {
            rename_plugin_pane(plugin_id, "Share Session");
        }
    }

    fn link_is_revealed(&self, link_id: &[u8]) -> bool {
        self.revealed_link.as_deref() == Some(link_id)
    }

    fn toggle_reveal(&mut self, link_id: Option<Vec<u8>>) {
        self.revealed_link = match (link_id, self.revealed_link.take()) {
            (Some(target), Some(current)) if current == target => None,
            (target, _) => target,
        };
    }

    fn secret_on_screen(&self) -> bool {
        if self.help_visible || self.signin.is_some() || self.admission_overlay_visible() {
            return false;
        }
        match self.screen {
            Screen::Main => self
                .selected_link()
                .is_some_and(|link| !link.active && self.link_is_revealed(&link.link_id)),
            Screen::Devices => self
                .selected_enrollment()
                .is_some_and(|link| self.link_is_revealed(&link.link_id)),
            Screen::Local => self.new_token.as_ref().is_some_and(|(name, _)| {
                self.selected_token().map(|(n, _, _)| n.as_str()) == Some(name.as_str())
            }),
        }
    }

    fn sync_selection_support(&mut self) {
        let wanted = self.secret_on_screen();
        if wanted != self.selection_support {
            self.selection_support = wanted;
            set_self_mouse_selection_support(wanted);
        }
    }

    fn handle_mode_update(&mut self, mode_info: ModeInfo) -> bool {
        let mut should_render = false;

        if self.session_name != mode_info.session_name {
            self.session_name = mode_info.session_name;
            should_render = true;
        }

        if let Some(web_sharing) = mode_info.web_sharing {
            if self.web.sharing != web_sharing {
                self.web.sharing = web_sharing;
                should_render = true;
            }
        }

        if let Some(web_server_ip) = mode_info.web_server_ip {
            self.web.ip = Some(web_server_ip);
            should_render = true;
        }

        if let Some(web_server_port) = mode_info.web_server_port {
            self.web.port = Some(web_server_port);
            should_render = true;
        }

        if let Some(capability) = mode_info.web_server_capability {
            let gained = capability && !self.web.capability;
            self.web.capability = capability;
            if gained {
                query_web_server_status();
                self.refresh_tokens();
            }
            should_render = true;
        }

        if self.web.relay_share_status != mode_info.relay_share_status {
            let was_live = self.relay_live();
            self.web.relay_share_status = mode_info.relay_share_status;
            let now_live = self.relay_live();

            if now_live && !was_live {
                self.refresh_links();
                self.refresh_devices();
                if let Some(read_only) = self.pending_mint.take() {
                    let label = generate_random_name();
                    self.mint_link(read_only, label, false, true);
                }
            } else if !now_live && was_live {
                self.links.clear();
                self.link_selected = None;
                self.devices.clear();
                self.enrollments.clear();
                self.device_selected = None;
                self.admissions.clear();
                self.admission_selected = None;
                self.revealed_link = None;
                if matches!(
                    self.input.prompt,
                    Some(Prompt::NewLink { .. }) | Some(Prompt::NewEnrollment { .. })
                ) {
                    self.input.cancel();
                }
                if self.screen == Screen::Devices {
                    self.screen = Screen::Main;
                }
            }

            match &self.web.relay_share_status {
                Some(RelayShareStatus::Failed {
                    reason: RelayFailureReason::AuthRejected,
                    ..
                }) => {
                    if self.pending_mint.is_some() && self.signin.is_none() {
                        self.signin = Some(String::new());
                    }
                },
                Some(RelayShareStatus::Failed { .. }) => self.pending_mint = None,
                _ => {},
            }

            should_render = true;
        }

        should_render
    }

    fn handle_web_server_status(&mut self, status: WebServerStatus) -> bool {
        match status {
            WebServerStatus::Online(base_url) => {
                self.web.base_url = base_url;
                self.web.started = true;
                self.web.different_version_error = None;
            },
            WebServerStatus::Offline => {
                self.web.started = false;
                self.web.different_version_error = None;
            },
            WebServerStatus::DifferentVersion(version) => {
                self.web.started = false;
                self.web.different_version_error = Some(version);
            },
        }
        true
    }

    fn handle_admission_pending_pipe(&mut self) -> bool {
        self.refresh_admissions();
        if self.admissions.is_empty() {
            return false;
        }
        self.admission_dismissed = false;
        if self.admission_selected.is_none() {
            self.admission_selected = Some(0);
        }
        true
    }

    fn handle_devices_changed_pipe(&mut self) -> bool {
        match self.screen {
            Screen::Main => self.refresh_links(),
            Screen::Devices => {
                self.refresh_devices();
                self.refresh_links();
            },
            Screen::Local => return false,
        }
        true
    }

    fn handle_key_input(&mut self, key: KeyWithModifier) -> bool {
        if self.has_message() && key.bare_key == BareKey::Esc && key.has_no_modifiers() {
            self.clear_messages();
            return true;
        }
        let cleared = self.clear_messages();

        let handled = if self.help_visible {
            self.handle_help_key(key)
        } else if self.signin.is_some() {
            self.handle_signin_key(key)
        } else if self.admission_overlay_visible() {
            self.handle_admission_key(key)
        } else {
            match self.screen {
                Screen::Main => self.handle_main_key(key),
                Screen::Devices => self.handle_devices_key(key),
                Screen::Local => self.handle_local_key(key),
            }
        };

        handled || cleared
    }

    fn handle_help_key(&mut self, key: KeyWithModifier) -> bool {
        if matches!(key.bare_key, BareKey::Esc | BareKey::Char('?')) && key.has_no_modifiers() {
            self.help_visible = false;
        }
        true
    }

    fn handle_signin_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Char(c) if key.has_no_modifiers() => {
                if let Some(buffer) = self.signin.as_mut() {
                    buffer.push(c);
                }
                true
            },
            BareKey::Backspace if key.has_no_modifiers() => {
                if let Some(buffer) = self.signin.as_mut() {
                    buffer.pop();
                }
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                if let Some(buffer) = self.signin.take() {
                    let token = buffer.trim().to_owned();
                    if token.is_empty() {
                        self.signin = Some(buffer);
                        self.error = Some("Enter a host token, or press <Esc>.".to_owned());
                    } else {
                        set_relay_tunnel_auth_token(token);
                        share_current_session_to_relay();
                    }
                }
                true
            },
            BareKey::Esc if key.has_no_modifiers() => {
                self.signin = None;
                self.pending_mint = None;
                true
            },
            _ => false,
        }
    }

    fn handle_admission_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Esc if key.has_no_modifiers() => {
                self.admission_dismissed = true;
                true
            },
            BareKey::Down if key.has_no_modifiers() => {
                self.admission_selected = navigate(self.admission_selected, self.admissions.len(), 1);
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.admission_selected =
                    navigate(self.admission_selected, self.admissions.len(), -1);
                true
            },
            BareKey::Char('a') if key.has_no_modifiers() => self.resolve_selected_admission(true),
            BareKey::Char('r') if key.has_no_modifiers() => self.resolve_selected_admission(false),
            _ => false,
        }
    }

    fn resolve_selected_admission(&mut self, admit: bool) -> bool {
        let Some(admission) = self
            .admission_selected
            .and_then(|index| self.admissions.get(index))
        else {
            return false;
        };
        let client_id = admission.client_id;
        let code_confirmed = admit && !admission.read_only;
        match relay_resolve_admission(client_id, admit, code_confirmed) {
            Ok(()) => {
                self.info = Some(if admit { "Admitted." } else { "Rejected." }.to_owned());
                self.admissions.retain(|a| a.client_id != client_id);
                self.admission_selected = clamp(self.admission_selected, self.admissions.len());
                if self.admissions.is_empty() {
                    self.admission_dismissed = false;
                }
                self.refresh_links();
            },
            Err(e) => self.error = Some(e),
        }
        true
    }

    fn handle_main_key(&mut self, key: KeyWithModifier) -> bool {
        if self.input.active() {
            return self.handle_input_key(key);
        }

        match key.bare_key {
            BareKey::Esc if key.has_no_modifiers() => {
                close_self();
                return false;
            },
            BareKey::Tab if key.has_no_modifiers() => {
                let next = self.next_screen();
                self.go_to(next);
                return true;
            },
            BareKey::Char('?') if key.has_no_modifiers() => {
                self.help_visible = true;
                return true;
            },
            _ => {},
        }

        if self.relay_live() {
            self.handle_live_key(key)
        } else {
            self.handle_idle_key(key)
        }
    }

    fn handle_idle_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Enter if key.has_no_modifiers() => {
                if self.relay_auth_rejected() {
                    self.pending_mint = Some(true);
                    self.signin = Some(String::new());
                } else {
                    self.start_share(true);
                }
                true
            },
            BareKey::Char('n') if key.has_no_modifiers() => {
                self.start_share(false);
                true
            },
            BareKey::Char('l') if key.has_no_modifiers() => {
                self.pending_mint = Some(true);
                self.signin = Some(String::new());
                true
            },
            _ => false,
        }
    }

    fn handle_live_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                self.link_selected = navigate(self.link_selected, self.links.len(), 1);
                self.revealed_link = None;
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.link_selected = navigate(self.link_selected, self.links.len(), -1);
                self.revealed_link = None;
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.copy_selected_link();
                true
            },
            BareKey::Char('s') if key.has_no_modifiers() => {
                let target = self
                    .selected_link()
                    .filter(|link| !link.active)
                    .map(|link| link.link_id.clone());
                self.toggle_reveal(target);
                true
            },
            BareKey::Char('n') if key.has_no_modifiers() => {
                self.input.start(Prompt::NewLink { read_only: false });
                true
            },
            BareKey::Char('o') if key.has_no_modifiers() => {
                self.input.start(Prompt::NewLink { read_only: true });
                true
            },
            BareKey::Char('x') if key.has_no_modifiers() => {
                self.revoke_selected_link();
                true
            },
            BareKey::Char(' ') if key.has_no_modifiers() => {
                stop_sharing_current_session_from_relay();
                true
            },
            BareKey::Char('a') if key.has_no_modifiers() && !self.admissions.is_empty() => {
                self.admission_dismissed = false;
                true
            },
            _ => false,
        }
    }

    fn handle_devices_key(&mut self, key: KeyWithModifier) -> bool {
        if self.input.active() {
            return self.handle_input_key(key);
        }

        let total = self.devices.len() + self.enrollments.len();
        match key.bare_key {
            BareKey::Tab if key.has_no_modifiers() => {
                let next = self.next_screen();
                self.go_to(next);
                true
            },
            BareKey::Esc if key.has_no_modifiers() => {
                close_self();
                false
            },
            BareKey::Char('?') if key.has_no_modifiers() => {
                self.help_visible = true;
                true
            },
            BareKey::Down if key.has_no_modifiers() => {
                self.device_selected = navigate(self.device_selected, total, 1);
                self.revealed_link = None;
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.device_selected = navigate(self.device_selected, total, -1);
                self.revealed_link = None;
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.copy_selected_enrollment();
                true
            },
            BareKey::Char('s') if key.has_no_modifiers() => {
                let target = self.selected_enrollment().map(|link| link.link_id.clone());
                self.toggle_reveal(target);
                true
            },
            BareKey::Char('e') if key.has_no_modifiers() => {
                self.input.start(Prompt::NewEnrollment { read_only: false });
                true
            },
            BareKey::Char('o') if key.has_no_modifiers() => {
                self.input.start(Prompt::NewEnrollment { read_only: true });
                true
            },
            BareKey::Char('x') if key.has_no_modifiers() => {
                self.revoke_selected_device();
                true
            },
            _ => false,
        }
    }

    fn handle_local_key(&mut self, key: KeyWithModifier) -> bool {
        if self.input.active() {
            return self.handle_input_key(key);
        }

        if self.confirming_revoke_all {
            match key.bare_key {
                BareKey::Char('y') if key.has_no_modifiers() => {
                    self.confirming_revoke_all = false;
                    self.revoke_all_tokens();
                },
                BareKey::Esc if key.has_no_modifiers() => self.confirming_revoke_all = false,
                _ => return false,
            }
            return true;
        }

        match key.bare_key {
            BareKey::Tab if key.has_no_modifiers() => {
                let next = self.next_screen();
                self.go_to(next);
                true
            },
            BareKey::Esc if key.has_no_modifiers() => {
                close_self();
                false
            },
            BareKey::Enter if key.has_no_modifiers() => {
                if self.web.started {
                    self.copy_selected_token();
                } else {
                    start_web_server();
                }
                true
            },
            BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                stop_web_server();
                true
            },
            BareKey::Char('x') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                if self.tokens.is_empty() {
                    self.error = Some("There are no login tokens to revoke.".to_owned());
                } else {
                    self.confirming_revoke_all = true;
                }
                true
            },
            BareKey::Char(' ') if key.has_no_modifiers() => {
                match self.web.sharing {
                    WebSharing::Disabled => {
                        self.error =
                            Some("Web sharing is disabled by configuration.".to_owned());
                    },
                    WebSharing::On => stop_sharing_current_session(),
                    WebSharing::Off => share_current_session(),
                }
                true
            },
            BareKey::Down if key.has_no_modifiers() => {
                self.token_selected = navigate(self.token_selected, self.tokens.len(), 1);
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.token_selected = navigate(self.token_selected, self.tokens.len(), -1);
                true
            },
            BareKey::Char('n') if key.has_no_modifiers() => {
                self.input.start(Prompt::NewToken { read_only: false });
                true
            },
            BareKey::Char('o') if key.has_no_modifiers() => {
                self.input.start(Prompt::NewToken { read_only: true });
                true
            },
            BareKey::Char('r') if key.has_no_modifiers() => {
                if self.selected_token().is_some() {
                    self.input.start(Prompt::RenameToken);
                }
                true
            },
            BareKey::Char('x') if key.has_no_modifiers() => {
                self.revoke_selected_token();
                true
            },
            _ => false,
        }
    }

    fn handle_input_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Char(c) if key.has_no_modifiers() => {
                self.input.buffer.push(c);
                true
            },
            BareKey::Backspace if key.has_no_modifiers() => {
                self.input.buffer.pop();
                true
            },
            BareKey::Esc if key.has_no_modifiers() => {
                self.input.cancel();
                true
            },
            BareKey::Enter if key.has_no_modifiers() => self.submit_input(),
            _ => false,
        }
    }

    fn submit_input(&mut self) -> bool {
        let Some((prompt, buffer)) = self.input.take() else {
            return false;
        };
        let typed = buffer.trim().to_owned();
        match prompt {
            Prompt::NewLink { read_only } => {
                let label = if typed.is_empty() {
                    generate_random_name()
                } else {
                    typed
                };
                self.mint_link(read_only, label, false, true);
            },
            Prompt::NewEnrollment { read_only } => {
                let label = if typed.is_empty() {
                    generate_random_name()
                } else {
                    typed
                };
                self.mint_link(read_only, label, true, true);
            },
            Prompt::NewToken { read_only } => {
                let name = (!typed.is_empty()).then_some(typed);
                self.generate_token(name, read_only);
            },
            Prompt::RenameToken => self.rename_selected_token(typed),
        }
        true
    }

    fn start_share(&mut self, read_only: bool) {
        if self.web.sharing == WebSharing::Disabled {
            self.error = Some("Web sharing is disabled by configuration.".to_owned());
            return;
        }
        self.pending_mint = Some(read_only);
        share_current_session_to_relay();
    }

    fn go_to(&mut self, screen: Screen) {
        self.input.cancel();
        self.confirming_revoke_all = false;
        self.revealed_link = None;
        self.new_token = None;
        self.clear_messages();
        self.screen = screen;
        match screen {
            Screen::Main => self.refresh_links(),
            Screen::Devices => self.refresh_devices(),
            Screen::Local => self.refresh_tokens(),
        }
    }

    fn selected_link(&self) -> Option<&GuestLink> {
        self.link_selected.and_then(|index| self.links.get(index))
    }

    fn selected_enrollment(&self) -> Option<&GuestLink> {
        let index = self.device_selected?;
        index
            .checked_sub(self.devices.len())
            .and_then(|link_index| self.enrollments.get(link_index))
    }

    fn selected_token(&self) -> Option<&(String, String, bool)> {
        self.token_selected.and_then(|index| self.tokens.get(index))
    }

    fn copy_selected_link(&mut self) {
        let Some(link) = self.selected_link() else {
            return;
        };
        if link.active {
            self.error = Some("This link has already been used.".to_owned());
            return;
        }
        copy_to_clipboard(link.url.clone());
        self.info = Some("Invite link copied to clipboard.".to_owned());
    }

    fn copy_selected_enrollment(&mut self) {
        let Some(link) = self.selected_enrollment() else {
            return;
        };
        copy_to_clipboard(link.url.clone());
        self.info = Some("Enrollment link copied to clipboard.".to_owned());
    }

    fn copy_selected_token(&mut self) {
        if self.selected_token().is_none() {
            return;
        }
        let matching = self.new_token.clone().filter(|(name, _)| {
            self.selected_token().map(|(n, _, _)| n.as_str()) == Some(name.as_str())
        });
        match matching {
            Some((_, value)) => {
                copy_to_clipboard(value);
                self.info = Some("Login token copied to clipboard.".to_owned());
            },
            None => {
                self.error = Some(
                    "This token's value is not stored and cannot be shown again. Create a new \
                     one with <n>."
                        .to_owned(),
                )
            },
        }
    }

    fn mint_link(&mut self, read_only: bool, label: String, enroll: bool, copy: bool) {
        match relay_mint_guest_link(read_only, label, enroll) {
            Ok(link) => {
                let url = link.url.clone();
                self.revealed_link = None;
                if enroll {
                    self.refresh_devices();
                    if let Some(index) = self
                        .enrollments
                        .iter()
                        .position(|l| l.link_id == link.link_id)
                    {
                        self.device_selected = Some(self.devices.len() + index);
                    }
                } else {
                    self.refresh_links();
                    if let Some(index) =
                        self.links.iter().position(|l| l.link_id == link.link_id)
                    {
                        self.link_selected = Some(index);
                    }
                }
                if copy {
                    copy_to_clipboard(url);
                    self.info = Some(
                        if enroll {
                            "Enrollment link copied to clipboard."
                        } else {
                            "Invite link copied to clipboard."
                        }
                        .to_owned(),
                    );
                }
            },
            Err(e) => self.error = Some(e),
        }
    }

    fn revoke_selected_link(&mut self) {
        let Some(link) = self.selected_link() else {
            return;
        };
        let link_id = link.link_id.clone();
        match relay_revoke_guest_link(link_id) {
            Ok(()) => {
                self.revealed_link = None;
                self.refresh_links();
                self.info = Some("Invite link revoked.".to_owned());
            },
            Err(e) => self.error = Some(e),
        }
    }

    fn revoke_selected_device(&mut self) {
        let outcome = match screen_devices::row_at(
            &self.devices,
            &self.enrollments,
            match self.device_selected {
                Some(index) => index,
                None => return,
            },
        ) {
            Some(DeviceRow::Device(device)) => {
                Some(relay_revoke_device(device.device_id.clone()).map(|_| "Device revoked."))
            },
            Some(DeviceRow::Enrollment(link)) => Some(
                relay_revoke_guest_link(link.link_id.clone()).map(|_| "Enrollment link revoked."),
            ),
            None => None,
        };
        match outcome {
            Some(Ok(message)) => {
                self.revealed_link = None;
                self.refresh_devices();
                self.info = Some(message.to_owned());
            },
            Some(Err(e)) => self.error = Some(e),
            None => {},
        }
    }

    fn generate_token(&mut self, name: Option<String>, read_only: bool) {
        self.refresh_tokens();
        let before: Vec<String> = self.tokens.iter().map(|(n, _, _)| n.clone()).collect();
        let requested = name.clone();
        match generate_web_login_token(name, read_only) {
            Ok(token) => {
                self.refresh_tokens();
                let created = requested
                    .filter(|name| self.tokens.iter().any(|(n, _, _)| n == name))
                    .or_else(|| {
                        let mut fresh = self
                            .tokens
                            .iter()
                            .map(|(n, _, _)| n.clone())
                            .filter(|n| !before.contains(n));
                        fresh.next().filter(|_| fresh.next().is_none())
                    });
                copy_to_clipboard(token.clone());
                match created {
                    Some(name) => {
                        if let Some(index) = self.tokens.iter().position(|(n, _, _)| *n == name) {
                            self.token_selected = Some(index);
                        }
                        self.new_token = Some((name, token));
                        self.info =
                            Some("Login token created and copied to clipboard.".to_owned());
                    },
                    None => {
                        self.new_token = None;
                        self.info = Some(
                            "Login token created and copied to clipboard. Paste it now; it \
                             cannot be shown again."
                                .to_owned(),
                        );
                    },
                }
            },
            Err(e) => self.error = Some(e),
        }
    }

    fn rename_selected_token(&mut self, new_name: String) {
        if new_name.is_empty() {
            return;
        }
        let Some((old_name, _, _)) = self.selected_token().cloned() else {
            return;
        };
        match rename_web_token(&old_name, &new_name) {
            Ok(_) => {
                if let Some((name, _)) = self.new_token.as_mut() {
                    if *name == old_name {
                        *name = new_name.clone();
                    }
                }
                self.refresh_tokens();
                if let Some(index) = self.tokens.iter().position(|(n, _, _)| *n == new_name) {
                    self.token_selected = Some(index);
                }
                self.info = Some("Login token renamed.".to_owned());
            },
            Err(e) => self.error = Some(e),
        }
    }

    fn revoke_selected_token(&mut self) {
        let Some((name, _, _)) = self.selected_token().cloned() else {
            return;
        };
        match revoke_web_login_token(&name) {
            Ok(_) => {
                if self.new_token.as_ref().is_some_and(|(n, _)| *n == name) {
                    self.new_token = None;
                }
                self.refresh_tokens();
                self.info = Some("Revoked. Connected clients are not affected.".to_owned());
            },
            Err(e) => self.error = Some(e),
        }
    }

    fn revoke_all_tokens(&mut self) {
        match revoke_all_web_tokens() {
            Ok(_) => {
                self.new_token = None;
                self.refresh_tokens();
                self.info =
                    Some("All tokens revoked. Connected clients are not affected.".to_owned());
            },
            Err(e) => self.error = Some(e),
        }
    }

    fn refresh_tokens(&mut self) {
        match list_web_login_tokens() {
            Ok(tokens) => {
                self.tokens = tokens;
                self.token_selected = clamp(self.token_selected, self.tokens.len());
            },
            Err(e) => self.error = Some(format!("Failed to retrieve login tokens: {}", e)),
        }
    }

    fn refresh_links(&mut self) {
        if !self.relay_live() {
            self.links.clear();
            self.link_selected = None;
            return;
        }
        match relay_list_guest_links() {
            Ok(links) => {
                self.links = links.into_iter().filter(|link| !link.enroll).collect();
                self.link_selected = clamp(self.link_selected, self.links.len());
            },
            Err(e) => self.error = Some(format!("Failed to retrieve invite links: {}", e)),
        }
    }

    fn refresh_devices(&mut self) {
        if !self.relay_live() {
            self.devices.clear();
            self.enrollments.clear();
            self.device_selected = None;
            return;
        }
        match relay_list_devices() {
            Ok(devices) => self.devices = devices,
            Err(e) => self.error = Some(format!("Failed to retrieve devices: {}", e)),
        }
        match relay_list_guest_links() {
            Ok(links) => {
                self.enrollments = links.into_iter().filter(|link| link.enroll).collect();
            },
            Err(e) => self.error = Some(format!("Failed to retrieve enrollment links: {}", e)),
        }
        self.device_selected = clamp(
            self.device_selected,
            self.devices.len() + self.enrollments.len(),
        );
    }

    fn refresh_admissions(&mut self) {
        if !self.relay_live() {
            self.admissions.clear();
            self.admission_selected = None;
            return;
        }
        match relay_list_pending_admissions() {
            Ok(admissions) => {
                self.admissions = admissions;
                self.admission_selected = clamp(self.admission_selected, self.admissions.len());
                if self.admissions.is_empty() {
                    self.admission_dismissed = false;
                }
            },
            Err(e) => self.error = Some(format!("Failed to retrieve pending joins: {}", e)),
        }
    }

    fn handle_mouse_event(&mut self, event: Mouse) -> bool {
        match event {
            Mouse::LeftClick(line, column) => self.handle_link_click(line, column),
            Mouse::Hover(line, column) => {
                self.ui.hover_coordinates = Some((column, line as usize));
                true
            },
            _ => false,
        }
    }

    fn handle_link_click(&mut self, line: isize, column: usize) -> bool {
        for (coordinates, url) in &self.ui.clickable_urls {
            if coordinates.contains(column, line as usize) {
                if let Some(executable) = self.ui.link_executable {
                    run_command(&[executable, url], Default::default());
                }
                return true;
            }
        }
        false
    }

    fn handle_command_result(
        &mut self,
        exit_code: Option<i32>,
        context: BTreeMap<String, String>,
    ) -> bool {
        if context.contains_key("xdg_open_cli") && exit_code == Some(0) {
            self.ui.link_executable = Some("xdg-open");
        } else if context.contains_key("open_cli") && exit_code == Some(0) {
            self.ui.link_executable = Some("open");
        }
        false
    }

    fn handle_pasted_text(&mut self, text: String) -> bool {
        let target = if let Some(buffer) = self.signin.as_mut() {
            buffer
        } else if self.input.active() {
            &mut self.input.buffer
        } else {
            return false;
        };
        for c in text.chars() {
            if c == '\n' || c == '\r' {
                continue;
            }
            target.push(c);
        }
        true
    }

    fn query_link_executable(&self) {
        let mut xdg_context = BTreeMap::new();
        xdg_context.insert("xdg_open_cli".to_owned(), String::new());
        run_command(&["xdg-open", "--help"], xdg_context);

        let mut open_context = BTreeMap::new();
        open_context.insert("open_cli".to_owned(), String::new());
        run_command(&["open", "--help"], open_context);
    }
}

fn navigate(selected: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let current = selected.unwrap_or(0);
    let next = if delta > 0 {
        if current + 1 >= len {
            0
        } else {
            current + 1
        }
    } else if current == 0 {
        len - 1
    } else {
        current - 1
    };
    Some(next)
}

fn clamp(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(selected.unwrap_or(0).min(len - 1))
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct CoordinatesInLine {
    x: usize,
    y: usize,
    width: usize,
}

impl CoordinatesInLine {
    pub fn new(x: usize, y: usize, width: usize) -> Self {
        CoordinatesInLine { x, y, width }
    }

    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x <= self.x + self.width && self.y == y
    }
}
