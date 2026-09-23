use std::collections::HashMap;
use zellij_tile::prelude::*;

use crate::list::{Access, Row, Tone};
use crate::ui_components::{footer, mask_secret, render_centered, Block, StatusTone};
use crate::CoordinatesInLine;

const INTRO: &str = "Enrolled devices reconnect to this session without asking for admission \
    again. Enrollment links work once and carry a secret, so send them over a trusted channel.";
const ENROLL_ACTIONS: &str = "<e> enroll   <o> enroll read-only";
const DEVICE_LABEL: &str = "Device:  ";
const SHARE_LABEL: &str = "Share:   ";
const SHARE_HINTS: &str = "<?> how it works   <Esc> close";
const NEW_LINK_HINT: &str = "(<ENTER> random name)";
const LINK_LABEL: &str = "Link  ";
const SECRET_VISIBLE: &str = "Secret is visible on screen.";

pub enum DeviceRow<'a> {
    Device(&'a EnrolledDevice),
    Enrollment(&'a GuestLink),
}

pub struct DevicesView<'a> {
    pub rows: usize,
    pub cols: usize,
    pub nav: Vec<crate::ui_components::NavItem>,
    pub devices: &'a [EnrolledDevice],
    pub enrollments: &'a [GuestLink],
    pub selected: Option<usize>,
    pub revealed: Option<&'a [u8]>,
    pub prompt: Option<(String, &'a str)>,
    pub message: Option<(&'a str, bool)>,
    pub hover: Option<(usize, usize)>,
}

pub fn render(view: DevicesView<'_>, clickable: &mut HashMap<CoordinatesInLine, String>) {
    let rows = device_rows(view.devices, view.enrollments);
    let mut blocks = vec![
        Block::Nav(view.nav.clone()),
        Block::Blank,
        Block::paragraph(INTRO),
        Block::Blank,
    ];

    if rows.is_empty() {
        blocks.push(Block::Empty {
            message: "No devices are enrolled.".to_owned(),
            keys: ENROLL_ACTIONS.to_owned(),
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

    blocks.push(match &view.prompt {
        Some((label, buffer)) => Block::Prompt {
            label: label.clone(),
            buffer: (*buffer).to_owned(),
            hint: NEW_LINK_HINT.to_owned(),
        },
        None => Block::hints(DEVICE_LABEL, &actions(&view)),
    });
    blocks.push(footer(view.message, SHARE_LABEL, SHARE_HINTS));

    render_centered(blocks, view.rows, view.cols, view.hover, clickable);
}

pub fn row_at<'a>(
    devices: &'a [EnrolledDevice],
    enrollments: &'a [GuestLink],
    index: usize,
) -> Option<DeviceRow<'a>> {
    if index < devices.len() {
        devices.get(index).map(DeviceRow::Device)
    } else {
        enrollments
            .get(index - devices.len())
            .map(DeviceRow::Enrollment)
    }
}

fn selection_blocks(view: &DevicesView<'_>) -> Vec<Block> {
    let Some(DeviceRow::Enrollment(link)) = view
        .selected
        .and_then(|index| row_at(view.devices, view.enrollments, index))
    else {
        return Vec::new();
    };
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

fn actions(view: &DevicesView<'_>) -> String {
    match view
        .selected
        .and_then(|index| row_at(view.devices, view.enrollments, index))
    {
        Some(DeviceRow::Device(_)) => format!("<x> revoke   {}", ENROLL_ACTIONS),
        Some(DeviceRow::Enrollment(link)) => {
            let toggle = if view.revealed == Some(link.link_id.as_slice()) {
                "<s> hide"
            } else {
                "<s> reveal"
            };
            format!("<ENTER> copy   {}   <x> revoke   {}", toggle, ENROLL_ACTIONS)
        },
        None => ENROLL_ACTIONS.to_owned(),
    }
}

fn device_rows(devices: &[EnrolledDevice], enrollments: &[GuestLink]) -> Vec<Row> {
    let mut rows: Vec<Row> = devices
        .iter()
        .map(|device| {
            let access = if device.read_only {
                Access::ReadOnly
            } else {
                Access::Full
            };
            let (status, tone) = if device.connected {
                ("connected", Tone::Good)
            } else if device.last_used.is_some() {
                ("not connected", Tone::Neutral)
            } else {
                ("never connected", Tone::Neutral)
            };
            Row::new(&device.label).access(access).status(status, tone)
        })
        .collect();

    rows.extend(enrollments.iter().map(|link| {
        let access = if link.read_only {
            Access::ReadOnly
        } else {
            Access::Full
        };
        Row::new(&link.label)
            .access(access)
            .status("enrollment pending", Tone::Alert)
    }));

    rows
}
