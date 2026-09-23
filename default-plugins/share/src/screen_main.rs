use zellij_tile::prelude::*;

use crate::list::{Access, Row, Tone};
use crate::ui_components::{
    footer, mask_secret, public_url, Block, StatusTone,
};

const INTRO: &str = "Share this session over the internet through zellij.online. Traffic is \
    end-to-end encrypted and you approve each guest with a PIN.";
const LIVE_INTRO: &str = "That address alone lets nobody in. Send a guest one of the invite \
    links below instead. Each link works once, and you approve the guest with a PIN before \
    they can join.";
const LINKS_TITLE: &str = "Invite links";
const SHARING_AT: &str = "Sharing at  ";
const LINK_LABEL: &str = "Link  ";

const IDLE_ACTIONS: [&str; 2] = [
    "<ENTER>  Share, and copy a read-only invite link",
    "<n>      Share, and copy a full-control invite link",
];

const NEW_LINK_ACTIONS: &str = "<n> new   <o> new read-only";
const LINK_LABEL_HINT: &str = "Link:    ";
const SHARE_LABEL_HINT: &str = "Share:   ";
const IDLE_SHARE_HINTS: &str = "<?> how it works   <Esc> close";
const CONNECTING_HINTS: &str = "<Esc> close";
const RECONNECTING_HINTS: &str = "<SPACE> stop sharing   <Esc> close";

const NEW_LINK_HINT: &str = "(<ENTER> random name)";
const SECRET_VISIBLE: &str = "Secret is visible on screen.";
const ACTIVE_NOTE: &str = "Used, and its guest is connected. Revoking disconnects them.";

pub struct OnlineView<'a> {
    pub nav: Vec<crate::ui_components::NavItem>,
    pub status: Option<&'a RelayShareStatus>,
    pub pending: bool,
    pub links: &'a [GuestLink],
    pub selected: Option<usize>,
    pub revealed: Option<&'a [u8]>,
    pub prompt: Option<(String, &'a str)>,
    pub pending_admissions: usize,
    pub message: Option<(&'a str, bool)>,
}

pub fn blocks(mut view: OnlineView<'_>) -> Vec<Block> {
    let nav = std::mem::take(&mut view.nav);
    let mut blocks = vec![Block::Nav(nav), Block::Blank];
    blocks.extend(match view.status {
        Some(RelayShareStatus::Connected { url: Some(url) }) => live_blocks(&view, url),
        Some(RelayShareStatus::Connected { url: None }) => transient_blocks(&view, None),
        Some(RelayShareStatus::Reconnecting { attempt }) => {
            transient_blocks(&view, Some(*attempt))
        },
        Some(RelayShareStatus::Failed { reason, message }) => failed_blocks(
            &view,
            message,
            matches!(reason, RelayFailureReason::AuthRejected),
        ),
        None if view.pending => transient_blocks(&view, None),
        None => idle_blocks(&view),
    });
    blocks
}

fn idle_blocks(view: &OnlineView<'_>) -> Vec<Block> {
    vec![
        Block::status("This session is not shared.", StatusTone::Alert),
        Block::Blank,
        Block::paragraph(INTRO),
        Block::Blank,
        Block::Bullets(IDLE_ACTIONS.iter().map(|line| line.to_string()).collect()),
        Block::Blank,
        footer(view.message, SHARE_LABEL_HINT, IDLE_SHARE_HINTS),
    ]
}

fn transient_blocks(view: &OnlineView<'_>, attempt: Option<u32>) -> Vec<Block> {
    let (status, tone, body, help) = match attempt {
        Some(attempt) => (
            format!("Reconnecting to zellij.online, attempt {}.", attempt),
            StatusTone::Alert,
            "The tunnel dropped. Invite links keep working once the connection returns.",
            RECONNECTING_HINTS,
        ),
        None => (
            "Connecting to zellij.online.".to_owned(),
            StatusTone::Neutral,
            "Opening a tunnel for this session.",
            CONNECTING_HINTS,
        ),
    };
    vec![
        Block::status(&status, tone),
        Block::Blank,
        Block::paragraph(body),
        Block::Blank,
        footer(view.message, SHARE_LABEL_HINT, help),
    ]
}

fn failed_blocks(view: &OnlineView<'_>, message: &str, auth_rejected: bool) -> Vec<Block> {
    let retry = if auth_rejected {
        "<l>      Sign in with a host token and share"
    } else {
        "<ENTER>  Try again"
    };
    vec![
        Block::status("This session is not shared.", StatusTone::Alert),
        Block::paragraph(message),
        Block::Blank,
        Block::Bullets(vec![retry.to_owned()]),
        Block::Blank,
        footer(view.message, SHARE_LABEL_HINT, IDLE_SHARE_HINTS),
    ]
}

fn live_blocks(view: &OnlineView<'_>, url: &str) -> Vec<Block> {
    let public = public_url(url);
    let rows = link_rows(view.links);

    let mut blocks = vec![
        Block::status_url(SHARING_AT, StatusTone::Good, public, public),
        Block::paragraph(LIVE_INTRO),
        Block::Blank,
        Block::Title(LINKS_TITLE.to_owned()),
    ];

    if rows.is_empty() {
        blocks.push(Block::Empty {
            message: "No invite links yet, so nobody can join.".to_owned(),
            keys: NEW_LINK_ACTIONS.to_owned(),
        });
        blocks.push(Block::Blank);
    } else {
        blocks.push(Block::List {
            rows,
            selected: view.selected,
        });
        blocks.push(Block::Blank);
        let detail = selection_blocks(view);
        if !detail.is_empty() {
            blocks.extend(detail);
            blocks.push(Block::Blank);
        }
    }

    blocks.push(match &view.prompt {
        Some((label, buffer)) => Block::Prompt {
            label: label.clone(),
            buffer: (*buffer).to_owned(),
            hint: NEW_LINK_HINT.to_owned(),
        },
        None if view.links.is_empty() => Block::hints(LINK_LABEL_HINT, &actions(view)),
        None => Block::hints_reserving(LINK_LABEL_HINT, &actions(view), &widest_actions()),
    });

    let pending = if view.pending_admissions > 0 {
        format!("<a> review {} pending   ", view.pending_admissions)
    } else {
        String::new()
    };
    let help = format!(
        "{}<SPACE> stop sharing   <?> how it works   <Esc> close",
        pending
    );
    blocks.push(footer(view.message, SHARE_LABEL_HINT, &help));
    blocks
}

fn selected_link<'a>(view: &OnlineView<'a>) -> Option<&'a GuestLink> {
    view.selected.and_then(|index| view.links.get(index))
}

fn selection_blocks(view: &OnlineView<'_>) -> Vec<Block> {
    let Some(link) = selected_link(view) else {
        return Vec::new();
    };
    if link.active {
        return vec![Block::paragraph(ACTIVE_NOTE)];
    }
    if view.revealed != Some(link.link_id.as_slice()) {
        return vec![Block::cropped_url(
            LINK_LABEL,
            &mask_secret(&link.url),
            "",
        )];
    }
    vec![
        Block::url(LINK_LABEL, &link.url, &link.url),
        Block::status(SECRET_VISIBLE, StatusTone::Alert),
    ]
}

fn actions(view: &OnlineView<'_>) -> String {
    match selected_link(view) {
        Some(link) if link.active => format!("<x> revoke   {}", NEW_LINK_ACTIONS),
        Some(link) => {
            let toggle = if view.revealed == Some(link.link_id.as_slice()) {
                "<s> hide"
            } else {
                "<s> reveal"
            };
            format!(
                "<ENTER> copy   {}   <x> revoke   {}",
                toggle, NEW_LINK_ACTIONS
            )
        },
        None => NEW_LINK_ACTIONS.to_owned(),
    }
}

fn widest_actions() -> String {
    format!(
        "<ENTER> copy   <s> reveal   <x> revoke   {}",
        NEW_LINK_ACTIONS
    )
}

fn link_rows(links: &[GuestLink]) -> Vec<Row> {
    links
        .iter()
        .map(|link| {
            let access = if link.read_only {
                Access::ReadOnly
            } else {
                Access::Full
            };
            let (status, tone) = if link.active {
                ("guest connected", Tone::Good)
            } else if link.spent {
                ("used", Tone::Neutral)
            } else {
                ("not used yet", Tone::Neutral)
            };
            Row::new(&link.label).access(access).status(status, tone)
        })
        .collect()
}
