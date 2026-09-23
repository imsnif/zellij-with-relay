use std::collections::HashMap;
use zellij_tile::prelude::*;

use crate::list::{Access, Row, Tone};
use crate::ui_components::{footer, render_centered, Block, StatusTone};
use crate::CoordinatesInLine;

const ADMISSION_HELP_SINGLE: &str = "<a> admit   <r> reject   <Esc> back";
const ADMISSION_HELP_MULTI: &str =
    "<a> admit   <r> reject   <Up>/<Down> select   <Esc> back";

const VERIFY: &str = "Admit only if this exact PIN appears on the joiner's own screen. If it \
    differs, the connection is not secure and should be rejected.";
const CONTESTED: &str = "More than one joiner is using a single-use link. That link may have \
    been compromised, so check every PIN.";

const SIGNIN_INTRO: &str = "zellij.online needs a host token before it will open a tunnel.";
const SIGNIN_NOTE: &str = "The token is kept for this session only. To keep it across restarts, \
    set relay_tunnel_auth_token in the config file or export \
    ZELLIJ_RELAY_TUNNEL_AUTH_TOKEN.";
const SIGNIN_HELP: &str = "<ENTER> sign in and share   <Esc> cancel";
const TOKEN_LABEL: &str = "Token: ";

const HELP_TITLE: &str = "About online sharing";
const HELP_BODY: [&str; 5] = [
    "Sharing opens a tunnel from this machine to zellij.online. The relay only forwards \
     ciphertext; it cannot read the session.",
    "Nobody can join with the tunnel alone. Each guest needs an invite link, which carries a \
     secret and works exactly once.",
    "When a guest connects, a six digit PIN appears here and on their screen. The session opens \
     only after both PINs are compared and the join is admitted.",
    "Read-only guests cannot type into the session. Full-control guests can.",
    "Enrolled devices skip the PIN on later connections. Revoking a link or a device \
     disconnects it immediately.",
];
const HELP_HELP: &str = "<Esc> back";
const JOIN_LABEL: &str = "Join:    ";
const SIGNIN_LABEL: &str = "Sign in: ";
const ABOUT_LABEL: &str = "About:   ";

fn no_clicks() -> HashMap<CoordinatesInLine, String> {
    HashMap::new()
}

pub fn render_admission(
    rows: usize,
    cols: usize,
    admissions: &[PendingAdmission],
    selected: Option<usize>,
    message: Option<(&str, bool)>,
) {
    let blocks = if admissions.len() == 1 {
        single_admission_blocks(&admissions[0], message)
    } else {
        multiple_admission_blocks(admissions, selected, message)
    };
    render_centered(blocks, rows, cols, None, &mut no_clicks());
}

fn single_admission_blocks(
    admission: &PendingAdmission,
    message: Option<(&str, bool)>,
) -> Vec<Block> {
    let label = if admission.label.is_empty() {
        "an unlabelled link".to_owned()
    } else {
        format!("\"{}\"", admission.label)
    };
    let title = format!(
        "Someone is joining with {} ({}s left)",
        label, admission.seconds_remaining
    );

    let (grant, tone) = if admission.read_only {
        (
            "Admitting grants read-only access to this session.",
            StatusTone::Good,
        )
    } else {
        (
            "Admitting grants FULL CONTROL of this session.",
            StatusTone::Alert,
        )
    };

    vec![
        Block::Title(title),
        Block::Blank,
        Block::field("PIN  ", &spaced(&admission.sas), StatusTone::Neutral),
        Block::Blank,
        Block::paragraph(VERIFY),
        Block::Blank,
        Block::status(grant, tone),
        Block::Blank,
        footer(message, JOIN_LABEL, ADMISSION_HELP_SINGLE),
    ]
}

fn multiple_admission_blocks(
    admissions: &[PendingAdmission],
    selected: Option<usize>,
    message: Option<(&str, bool)>,
) -> Vec<Block> {
    let title = format!("{} people are joining", admissions.len());
    let mut blocks = vec![Block::Title(title), Block::Blank];

    if admissions.iter().any(|a| a.contested) {
        blocks.push(Block::status(CONTESTED, StatusTone::Alert));
        blocks.push(Block::Blank);
    }

    let rows: Vec<Row> = admissions
        .iter()
        .map(|admission| {
            let label = if admission.label.is_empty() {
                "(unlabelled)".to_owned()
            } else {
                admission.label.clone()
            };
            let access = if admission.read_only {
                Access::ReadOnly
            } else {
                Access::Full
            };
            Row::new(label).access(access).status(
                format!("PIN {}   {}s left", admission.sas, admission.seconds_remaining),
                Tone::Neutral,
            )
        })
        .collect();

    blocks.push(Block::List { rows, selected });
    blocks.push(Block::Blank);
    if let Some(admission) = selected.and_then(|index| admissions.get(index)) {
        let (grant, tone) = if admission.read_only {
            (
                "Admitting grants read-only access to this session.",
                StatusTone::Good,
            )
        } else {
            (
                "Admitting grants FULL CONTROL of this session.",
                StatusTone::Alert,
            )
        };
        blocks.push(Block::status(grant, tone));
        blocks.push(Block::Blank);
    }
    blocks.push(Block::paragraph(VERIFY));
    blocks.push(Block::Blank);
    blocks.push(footer(message, JOIN_LABEL, ADMISSION_HELP_MULTI));
    blocks
}

fn spaced(sas: &str) -> String {
    sas.chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn render_signin(rows: usize, cols: usize, buffer: &str, message: Option<(&str, bool)>) {
    let masked: String = std::iter::repeat_n('*', buffer.chars().count()).collect();

    let blocks = vec![
        Block::paragraph(SIGNIN_INTRO),
        Block::Blank,
        Block::Prompt {
            label: TOKEN_LABEL.to_owned(),
            buffer: masked,
            hint: String::new(),
        },
        Block::Blank,
        Block::paragraph(SIGNIN_NOTE),
        Block::Blank,
        footer(message, SIGNIN_LABEL, SIGNIN_HELP),
    ];
    render_centered(blocks, rows, cols, None, &mut no_clicks());
}

pub fn render_help_screen(rows: usize, cols: usize) {
    let mut blocks = vec![Block::Title(HELP_TITLE.to_owned()), Block::Blank];
    for paragraph in HELP_BODY {
        blocks.push(Block::paragraph(paragraph));
        blocks.push(Block::Blank);
    }
    blocks.push(Block::hints(ABOUT_LABEL, HELP_HELP));
    render_centered(blocks, rows, cols, None, &mut no_clicks());
}

pub fn render_no_capability(rows: usize, cols: usize) {
    let blocks = vec![Block::status(
        "This version of Zellij was compiled without web sharing capabilities.",
        StatusTone::Alert,
    )];
    render_centered(blocks, rows, cols, None, &mut no_clicks());
}
