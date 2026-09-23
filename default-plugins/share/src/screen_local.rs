use std::collections::HashMap;
use std::net::IpAddr;
use zellij_tile::prelude::*;

use crate::list::{Access, Row, Tone};
use crate::ui_components::{footer, render_centered, Block, StatusTone};
use crate::CoordinatesInLine;

const INTRO: &str = "Serves this session from a web server on this machine. No relay and no \
    account, but the network and its encryption are your responsibility.";
const TOKENS_TITLE: &str = "Login tokens";
const NEW_TOKEN_ACTIONS: &str = "<n> new   <o> new read-only";
const TOKEN_LABEL: &str = "Token  ";
const TOKEN_LABEL_HINT: &str = "Token:   ";
const SERVER_LABEL: &str = "Server:  ";
const TOKEN_NOTE: &str = "Copy this now. It is not stored and cannot be shown again.";
const NEW_TOKEN_HINT: &str = "(<ENTER> unnamed)";
const RENAME_HINT: &str = "(<ENTER> submit)";
const NOT_ENCRYPTED: &str = "This connection is not encrypted.";
const CONFIRM_REVOKE_ALL: &str = "Revoke every login token?   <y> yes   <Esc> no";

const WEB_SERVER_LABEL: &str = "Web server:   ";
const ADDRESS_LABEL: &str = "Address:      ";
const SESSION_LABEL: &str = "This session: ";
const JOIN_LABEL: &str = "Join at:      ";

pub struct LocalView<'a> {
    pub rows: usize,
    pub cols: usize,
    pub nav: Vec<crate::ui_components::NavItem>,
    pub server_started: bool,
    pub server_base_url: &'a str,
    pub server_version_error: Option<&'a str>,
    pub server_ip: Option<IpAddr>,
    pub server_port: Option<u16>,
    pub session_name: Option<&'a str>,
    pub sharing: WebSharing,
    pub tokens: &'a [(String, String, bool)],
    pub selected: Option<usize>,
    pub new_token: Option<(&'a str, &'a str)>,
    pub prompt: Option<(String, &'a str)>,
    pub confirming_revoke_all: bool,
    pub message: Option<(&'a str, bool)>,
    pub hover: Option<(usize, usize)>,
}

pub fn render(view: LocalView<'_>, clickable: &mut HashMap<CoordinatesInLine, String>) {
    let unencrypted = view.server_base_url.starts_with("http://");

    let mut blocks = vec![
        Block::Nav(view.nav.clone()),
        Block::Blank,
        Block::paragraph(INTRO),
        Block::Blank,
    ];

    blocks.extend(server_blocks(&view, unencrypted));
    blocks.push(session_block(&view));
    if view.sharing.web_clients_allowed() && view.server_started {
        blocks.push(join_block(&view, unencrypted));
    }
    blocks.push(Block::Blank);
    blocks.push(Block::Title(TOKENS_TITLE.to_owned()));

    let rows = token_rows(view.tokens);
    if rows.is_empty() {
        blocks.push(Block::Empty {
            message: "No login tokens yet, so nobody can log in from a browser.".to_owned(),
            keys: NEW_TOKEN_ACTIONS.to_owned(),
        });
        blocks.push(Block::Blank);
    } else {
        blocks.push(Block::List {
            rows,
            selected: view.selected,
        });
        blocks.push(Block::Blank);
        let detail = selection_blocks(&view);
        if !detail.is_empty() {
            blocks.extend(detail);
            blocks.push(Block::Blank);
        }
    }

    if view.confirming_revoke_all {
        blocks.push(Block::keys(CONFIRM_REVOKE_ALL));
    } else {
        blocks.push(match &view.prompt {
            Some((label, buffer)) => Block::Prompt {
                label: label.clone(),
                buffer: (*buffer).to_owned(),
                hint: if label.starts_with("New") {
                    NEW_TOKEN_HINT
                } else {
                    RENAME_HINT
                }
                .to_owned(),
            },
            None => Block::hints(TOKEN_LABEL_HINT, &actions(&view)),
        });
    }
    blocks.push(footer(view.message, SERVER_LABEL, &help_line(&view)));

    render_centered(blocks, view.rows, view.cols, view.hover, clickable);
}

fn help_line(view: &LocalView<'_>) -> String {
    let sharing = match view.sharing {
        WebSharing::Disabled => "",
        WebSharing::On => "<SPACE> stop sharing   ",
        WebSharing::Off => "<SPACE> share session   ",
    };
    let server = if view.server_started {
        "<Ctrl c> stop server   "
    } else {
        ""
    };
    format!("{}{}<Ctrl x> revoke all   <Esc> close", sharing, server)
}

fn server_blocks(view: &LocalView<'_>, unencrypted: bool) -> Vec<Block> {
    if let Some(version) = view.server_version_error {
        return vec![
            Block::field(
                WEB_SERVER_LABEL,
                &format!("RUNNING INCOMPATIBLE VERSION {}", version),
                StatusTone::Alert,
            ),
            Block::keys("<Ctrl c> stop the other server"),
        ];
    }

    if !view.server_started {
        return vec![
            Block::field(WEB_SERVER_LABEL, "NOT RUNNING", StatusTone::Alert),
            Block::keys("<ENTER> start the web server"),
        ];
    }

    let mut blocks = vec![
        Block::field(WEB_SERVER_LABEL, "RUNNING", StatusTone::Good),
        Block::url(ADDRESS_LABEL, view.server_base_url, view.server_base_url),
    ];
    if unencrypted {
        blocks.push(Block::status(NOT_ENCRYPTED, StatusTone::Alert));
    }
    blocks
}

fn session_block(view: &LocalView<'_>) -> Block {
    let (value, tone) = match view.sharing {
        WebSharing::On => ("SHARING", StatusTone::Good),
        WebSharing::Off => ("NOT SHARING", StatusTone::Neutral),
        WebSharing::Disabled => ("SHARING IS DISABLED BY CONFIGURATION", StatusTone::Alert),
    };
    Block::field(SESSION_LABEL, value, tone)
}

fn join_block(view: &LocalView<'_>, unencrypted: bool) -> Block {
    let url = session_url(view, unencrypted);
    Block::url(JOIN_LABEL, &url, &url)
}

fn session_url(view: &LocalView<'_>, unencrypted: bool) -> String {
    let ip = view
        .server_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "UNDEFINED".to_owned());
    let port = view
        .server_port
        .map(|port| port.to_string())
        .unwrap_or_else(|| "UNDEFINED".to_owned());
    let scheme = if unencrypted { "http" } else { "https" };
    format!(
        "{}://{}:{}/{}",
        scheme,
        ip,
        port,
        view.session_name.unwrap_or("")
    )
}

fn selected_token<'a>(view: &LocalView<'a>) -> Option<&'a (String, String, bool)> {
    view.selected.and_then(|index| view.tokens.get(index))
}

fn new_token_value<'a>(view: &LocalView<'a>) -> Option<&'a str> {
    let (name, _, _) = selected_token(view)?;
    view.new_token
        .filter(|(token_name, _)| token_name == name)
        .map(|(_, value)| value)
}

fn selection_blocks(view: &LocalView<'_>) -> Vec<Block> {
    match new_token_value(view) {
        Some(value) => vec![
            Block::url(TOKEN_LABEL, value, ""),
            Block::status(TOKEN_NOTE, StatusTone::Alert),
        ],
        None => Vec::new(),
    }
}

fn actions(view: &LocalView<'_>) -> String {
    if selected_token(view).is_none() {
        return NEW_TOKEN_ACTIONS.to_owned();
    }
    let copy = if new_token_value(view).is_some() {
        "<ENTER> copy   "
    } else {
        ""
    };
    format!("{}<r> rename   <x> revoke   {}", copy, NEW_TOKEN_ACTIONS)
}

fn token_rows(tokens: &[(String, String, bool)]) -> Vec<Row> {
    tokens
        .iter()
        .map(|(name, created, read_only)| {
            let access = if *read_only {
                Access::ReadOnly
            } else {
                Access::Full
            };
            Row::new(name).access(access).status(
                created.chars().take(19).collect::<String>(),
                Tone::Neutral,
            )
        })
        .collect()
}
