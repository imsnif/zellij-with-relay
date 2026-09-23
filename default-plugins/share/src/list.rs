use zellij_tile::prelude::*;

use crate::ui_components::{elide, pad};

const TOP_LEVEL_OFFSET: usize = 3;
const CHILD_LEVEL_OFFSET: usize = 5;
const MIN_LABEL_WIDTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Full,
    ReadOnly,
    None,
}

impl Access {
    fn label(&self) -> &'static str {
        match self {
            Access::Full => "full control",
            Access::ReadOnly => "read-only",
            Access::None => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Alert,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub label: String,
    pub access: Access,
    pub status: String,
    pub tone: Tone,
}

impl Row {
    pub fn new(label: impl Into<String>) -> Self {
        Row {
            label: label.into(),
            access: Access::None,
            status: String::new(),
            tone: Tone::Neutral,
        }
    }

    pub fn access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }

    pub fn status(mut self, status: impl Into<String>, tone: Tone) -> Self {
        self.status = status.into();
        self.tone = tone;
        self
    }
}

pub struct ListArea {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

struct Columns {
    label: usize,
    access: usize,
    status: usize,
    inline: bool,
}

fn columns(rows: &[Row], width: usize) -> Columns {
    let natural_label = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(1)
        .max(1);
    let access_width = rows
        .iter()
        .map(|r| r.access.label().chars().count())
        .max()
        .unwrap_or(0);
    let status_width = rows
        .iter()
        .map(|r| r.status.chars().count())
        .max()
        .unwrap_or(0);

    let attributes = if access_width > 0 { 2 + access_width } else { 0 }
        + if status_width > 0 { 2 + status_width } else { 0 };
    let inline = attributes == 0 || MIN_LABEL_WIDTH + attributes <= width;
    let label = if inline {
        natural_label.min(width.saturating_sub(attributes)).max(1)
    } else {
        natural_label.min(width).max(1)
    };

    Columns {
        label,
        access: access_width,
        status: status_width,
        inline,
    }
}

fn attribute_lines(row: &Row, cols: &Columns, width: usize) -> Vec<(String, bool)> {
    let access = row.access.label();
    let mut lines = Vec::new();
    if access.is_empty() && row.status.is_empty() {
        return lines;
    }
    if access.is_empty() {
        lines.push((row.status.clone(), true));
        return lines;
    }
    if row.status.is_empty() {
        lines.push((access.to_owned(), false));
        return lines;
    }
    let combined = format!("{}   {}", pad(access, cols.access), row.status);
    if combined.chars().count() <= width {
        lines.push((combined, false));
    } else {
        lines.push((access.to_owned(), false));
        lines.push((row.status.clone(), true));
    }
    lines
}

fn tone_range(item: NestedListItem, tone: Tone, range: std::ops::Range<usize>) -> NestedListItem {
    match tone {
        Tone::Good => item.success_color_range(range),
        Tone::Alert => item.error_color_range(range),
        Tone::Neutral => item.unbold_range(range),
    }
}

fn access_range(item: NestedListItem, access: Access, range: std::ops::Range<usize>) -> NestedListItem {
    match access {
        Access::Full => item.error_color_range(range),
        Access::ReadOnly => item.success_color_range(range),
        Access::None => item,
    }
}

fn row_items(row: &Row, cols: &Columns, child_width: usize, selected: bool) -> Vec<NestedListItem> {
    if cols.inline {
        return vec![inline_item(row, cols, selected)];
    }

    let mut items = vec![{
        let mut item = NestedListItem::new(elide(&row.label, cols.label));
        if selected {
            item = item.selected();
        }
        item
    }];

    for (line, status_only) in attribute_lines(row, cols, child_width) {
        let shown = elide(&line, child_width);
        let shown_width = shown.chars().count();
        let mut item = NestedListItem::new(&shown).indent(1);
        if status_only {
            item = tone_range(item, row.tone, 0..shown_width);
        } else {
            let access_width = row.access.label().chars().count().min(shown_width);
            item = access_range(item, row.access, 0..access_width);
            if !row.status.is_empty() {
                let start = cols.access + 3;
                if start < shown_width {
                    item = tone_range(item, row.tone, start..shown_width);
                }
            }
        }
        if selected {
            item = item.selected();
        }
        items.push(item);
    }
    items
}

fn inline_item(row: &Row, cols: &Columns, selected: bool) -> NestedListItem {
    let mut line = pad(&elide(&row.label, cols.label), cols.label);
    let mut access_span = None;
    let mut status_span = None;

    if cols.access > 0 {
        line.push_str("  ");
        let start = line.chars().count();
        let label = row.access.label();
        line.push_str(&pad(label, cols.access));
        if !label.is_empty() {
            access_span = Some(start..start + label.chars().count());
        }
    }
    if cols.status > 0 && !row.status.is_empty() {
        line.push_str("  ");
        let start = line.chars().count();
        line.push_str(&row.status);
        status_span = Some(start..start + row.status.chars().count());
    }

    let mut item = NestedListItem::new(line.trim_end());
    if let Some(span) = access_span {
        item = access_range(item, row.access, span);
    }
    if let Some(span) = status_span {
        item = tone_range(item, row.tone, span);
    }
    if selected {
        item = item.selected();
    }
    item
}

pub fn natural_width(rows: &[Row]) -> usize {
    let label = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0);
    let access = rows
        .iter()
        .map(|r| r.access.label().chars().count())
        .max()
        .unwrap_or(0);
    let status = rows
        .iter()
        .map(|r| r.status.chars().count())
        .max()
        .unwrap_or(0);
    label
        + if access > 0 { access + 2 } else { 0 }
        + if status > 0 { status + 2 } else { 0 }
        + TOP_LEVEL_OFFSET
}

pub fn natural_height(rows: &[Row], width: usize) -> usize {
    if width <= TOP_LEVEL_OFFSET {
        return rows.len();
    }
    let cols = columns(rows, width - TOP_LEVEL_OFFSET);
    if cols.inline {
        return rows.len();
    }
    let child_width = width.saturating_sub(CHILD_LEVEL_OFFSET).max(1);
    rows.iter()
        .map(|row| 1 + attribute_lines(row, &cols, child_width).len())
        .sum()
}

pub fn render_rows(rows: &[Row], selected: Option<usize>, area: ListArea) {
    if rows.is_empty() || area.height == 0 || area.width <= TOP_LEVEL_OFFSET {
        return;
    }

    let cols = columns(rows, area.width - TOP_LEVEL_OFFSET);
    let child_width = area.width.saturating_sub(CHILD_LEVEL_OFFSET).max(1);

    let mut items: Vec<NestedListItem> = Vec::new();
    let mut group_start = 0;
    let mut group_end = 0;

    for (index, row) in rows.iter().enumerate() {
        let is_selected = selected == Some(index);
        if is_selected {
            group_start = items.len();
        }
        items.extend(row_items(row, &cols, child_width, is_selected));
        if is_selected {
            group_end = items.len();
        }
    }

    let total = items.len();
    let start = if total <= area.height {
        0
    } else {
        let mut candidate = group_start.saturating_sub(1);
        if candidate + area.height < group_end {
            candidate = group_end - area.height;
        }
        candidate.min(group_start).min(total - area.height)
    };
    let end = (start + area.height).min(total);

    let window: Vec<NestedListItem> = items.drain(start..end).collect();
    print_nested_list_with_coordinates(
        window,
        area.x,
        area.y,
        Some(area.width),
        Some(area.height),
    );
}
