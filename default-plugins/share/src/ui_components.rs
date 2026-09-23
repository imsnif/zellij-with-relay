use std::collections::HashMap;
use zellij_tile::prelude::*;

use crate::CoordinatesInLine;



pub fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in words {
        let wc = word.chars().count();
        if current.is_empty() {
            current = word.to_string();
        } else if current.chars().count() + 1 + wc <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

pub fn chunk(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
}

pub const HINT_SEP: &str = "   ";

pub fn wrap_hints(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.chars().count() <= width {
        return vec![text.to_string()];
    }
    let separator_width = HINT_SEP.chars().count();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for part in text.split(HINT_SEP) {
        let needed = current.chars().count() + separator_width + part.chars().count();
        if !current.is_empty() && needed > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(HINT_SEP);
        }
        current.push_str(part);
    }
    if !current.is_empty() {
        lines.push(current);
    }

    if lines.iter().any(|line| line.chars().count() > width) {
        return word_wrap(text, width);
    }
    lines
}

pub const HIDDEN: &str = "<hidden>";

pub fn mask_secret(url: &str) -> String {
    let Some(fragment_start) = url.find('#') else {
        return url.to_string();
    };
    let (head, tail) = url.split_at(fragment_start + 1);
    let masked = tail
        .split('&')
        .map(|part| {
            if part.starts_with("k=") {
                format!("k={}", HIDDEN)
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{}{}", head, masked)
}

pub fn truncate_end(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let kept: String = text.chars().take(max - 3).collect();
    format!("{}...", kept)
}

pub fn wrap_bullet(line: &str, width: usize) -> Vec<String> {
    if line.chars().count() <= width {
        return vec![line.to_string()];
    }
    let indent = line
        .find('>')
        .map(|index| {
            let rest = &line[index + 1..];
            index + 1 + (rest.len() - rest.trim_start().len())
        })
        .filter(|indent| indent + 12 <= width)
        .unwrap_or(0);
    let body_width = width.saturating_sub(indent).max(1);
    word_wrap(&line[indent..], body_width)
        .into_iter()
        .enumerate()
        .map(|(position, part)| {
            if position == 0 {
                format!("{}{}", &line[..indent], part)
            } else {
                format!("{}{}", " ".repeat(indent), part)
            }
        })
        .collect()
}

pub fn wrap_after_label(text: &str, body_width: usize, chunked: bool) -> Vec<String> {
    let body_width = body_width.max(1);
    if chunked {
        chunk(text, body_width)
    } else {
        word_wrap(text, body_width)
    }
}

pub fn elide(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let keep = max - 3;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let chars: Vec<char> = text.chars().collect();
    let head_part: String = chars[..head].iter().collect();
    let tail_part: String = chars[count - tail..].iter().collect();
    format!("{}...{}", head_part, tail_part)
}

pub fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count >= width {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.extend(std::iter::repeat_n(' ', width - count));
    out
}

pub const VALUE: usize = 1;
pub const TITLE: usize = 2;
pub const KEY: usize = 3;

pub fn quiet(line: &str) -> Text {
    Text::new(line).unbold_range(..)
}

pub fn highlight_keys(line: &str) -> Text {
    let mut text = Text::new(line);
    let chars: Vec<char> = line.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '<' {
            if let Some(offset) = chars[index..].iter().position(|c| *c == '>') {
                text = text.color_range(KEY, index..index + offset + 1);
                index += offset + 1;
                continue;
            }
        }
        index += 1;
    }
    text
}

pub fn public_url(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Neutral,
    Good,
    Alert,
}

const PREFERRED_PROSE_WIDTH: usize = 72;
const PREFERRED_FOOTER_WIDTH: usize = 96;
const MIN_LIST_ROWS: usize = 3;

pub enum Block {
    Blank,
    Title(String),
    Status {
        text: String,
        tone: StatusTone,
    },
    Field {
        label: String,
        value: String,
        tone: StatusTone,
    },
    Paragraph {
        text: String,
    },
    Url {
        prefix: String,
        prefix_tone: Option<StatusTone>,
        display: String,
        target: String,
        crop: bool,
    },
    Keys(String),
    Hints {
        label: String,
        hints: String,
    },
    Nav(Vec<NavItem>),
    Bullets(Vec<String>),
    Prompt {
        label: String,
        buffer: String,
        hint: String,
    },
    Message {
        text: String,
        is_error: bool,
    },
    List {
        rows: Vec<crate::list::Row>,
        selected: Option<usize>,
    },
    Empty {
        message: String,
        keys: String,
    },
}

#[derive(Debug, Clone)]
pub struct NavItem {
    pub label: String,
    pub selected: bool,
    pub enabled: bool,
}

const NAV_KEY: &str = "<TAB>";
const NAV_GAP: usize = 2;
const RIBBON_PADDING: usize = 4;

fn nav_width(items: &[NavItem]) -> usize {
    NAV_KEY.chars().count()
        + NAV_GAP
        + items
            .iter()
            .map(|item| item.label.chars().count() + RIBBON_PADDING)
            .sum::<usize>()
}

impl Block {
    pub fn hints(label: &str, hints: &str) -> Self {
        Block::Hints {
            label: label.to_owned(),
            hints: hints.to_owned(),
        }
    }

    pub fn status(text: &str, tone: StatusTone) -> Self {
        Block::Status {
            text: text.to_owned(),
            tone,
        }
    }

    pub fn paragraph(text: &str) -> Self {
        Block::Paragraph {
            text: text.to_owned(),
        }
    }

    pub fn url(prefix: &str, display: &str, target: &str) -> Self {
        Block::Url {
            prefix: prefix.to_owned(),
            prefix_tone: None,
            display: display.to_owned(),
            target: target.to_owned(),
            crop: false,
        }
    }

    pub fn cropped_url(prefix: &str, display: &str, target: &str) -> Self {
        Block::Url {
            prefix: prefix.to_owned(),
            prefix_tone: None,
            display: display.to_owned(),
            target: target.to_owned(),
            crop: true,
        }
    }

    pub fn status_url(prefix: &str, tone: StatusTone, display: &str, target: &str) -> Self {
        Block::Url {
            prefix: prefix.to_owned(),
            prefix_tone: Some(tone),
            display: display.to_owned(),
            target: target.to_owned(),
            crop: false,
        }
    }

    pub fn keys(text: &str) -> Self {
        Block::Keys(text.to_owned())
    }

    pub fn field(label: &str, value: &str, tone: StatusTone) -> Self {
        Block::Field {
            label: label.to_owned(),
            value: value.to_owned(),
            tone,
        }
    }

    fn rigid_width(&self) -> usize {
        match self {
            Block::Blank => 0,
            Block::Title(text) | Block::Keys(text) | Block::Status { text, .. } => {
                text.chars().count()
            },
            Block::Hints { .. } => 0,
            Block::Nav(items) => nav_width(items),
            Block::Field { label, value, .. } => label.chars().count() + value.chars().count(),
            Block::Paragraph { .. } => 0,
            Block::Url {
                prefix,
                display,
                crop,
                ..
            } => {
                if *crop {
                    0
                } else {
                    prefix.chars().count() + display.chars().count()
                }
            },
            Block::Bullets(lines) => lines
                .iter()
                .map(|line| line.chars().count() + 3)
                .max()
                .unwrap_or(0),
            Block::Prompt {
                label,
                buffer,
                hint,
            } => label.chars().count() + buffer.chars().count() + hint.chars().count() + 2,
            Block::Message { .. } => 0,
            Block::List { rows, .. } => crate::list::natural_width(rows),
            Block::Empty { keys, .. } => keys.chars().count() + 1,
        }
    }

    fn soft_width(&self) -> usize {
        match self {
            Block::Paragraph { .. } => PREFERRED_PROSE_WIDTH,
            Block::Hints { label, hints } => (label.chars().count() + hints.chars().count())
                .min(PREFERRED_FOOTER_WIDTH),
            Block::Url {
                prefix,
                display,
                crop: true,
                ..
            } => (prefix.chars().count() + display.chars().count()).min(PREFERRED_PROSE_WIDTH),
            Block::Message { text, .. } => text.chars().count().min(PREFERRED_FOOTER_WIDTH),
            Block::Empty { message, .. } => {
                (message.chars().count() + 1).min(PREFERRED_PROSE_WIDTH)
            },
            _ => 0,
        }
    }

    fn height(&self, width: usize) -> usize {
        match self {
            Block::Paragraph { text, .. } => word_wrap(text, width).len(),
            Block::Message { text, .. } => word_wrap(text, width).len(),
            Block::Title(text) | Block::Status { text, .. } => word_wrap(text, width).len(),
            Block::Keys(text) => wrap_hints(text, width).len(),
            Block::Hints { label, hints } => {
                let label_width = label.chars().count();
                wrap_hints(hints, width.saturating_sub(label_width).max(1)).len()
            },
            Block::Field { label, value, .. } => {
                let label_width = label.chars().count();
                if label_width + 4 >= width {
                    1 + word_wrap(value, width).len()
                } else {
                    wrap_after_label(value, width - label_width, false).len()
                }
            },
            Block::Url { crop: true, .. } => 1,
            Block::Url {
                prefix, display, ..
            } => {
                let prefix_width = prefix.chars().count();
                if prefix_width + 8 >= width {
                    1 + chunk(display, width).len()
                } else {
                    wrap_after_label(display, width - prefix_width, true).len()
                }
            },
            Block::Bullets(lines) => lines
                .iter()
                .map(|line| wrap_bullet(line, width.saturating_sub(3)).len())
                .sum(),
            Block::Empty { message, keys } => {
                let inner = width.saturating_sub(1);
                word_wrap(message, inner).len() + wrap_hints(keys, inner).len()
            },
            Block::List { rows, .. } => crate::list::natural_height(rows, width),
            _ => 1,
        }
    }
}

fn pad_prose(blocks: Vec<Block>) -> Vec<Block> {
    let mut padded: Vec<Block> = Vec::with_capacity(blocks.len() + 4);
    for block in blocks {
        let previous_is_blank = padded
            .last()
            .map(|previous| matches!(previous, Block::Blank))
            .unwrap_or(true);
        let previous_is_prose = padded
            .last()
            .map(|previous| matches!(previous, Block::Paragraph { .. }))
            .unwrap_or(false);
        let starts_prose = matches!(block, Block::Paragraph { .. });
        let follows_prose = previous_is_prose && !matches!(block, Block::Blank);
        if (starts_prose && !previous_is_blank) || follows_prose {
            padded.push(Block::Blank);
        }
        padded.push(block);
    }
    padded
}

pub fn render_centered(
    blocks: Vec<Block>,
    rows: usize,
    cols: usize,
    hover: Option<(usize, usize)>,
    clickable: &mut HashMap<CoordinatesInLine, String>,
) {
    if rows == 0 || cols == 0 {
        return;
    }
    let blocks = pad_prose(blocks);
    let available = cols.saturating_sub(2).max(1);

    let rigid = blocks
        .iter()
        .map(|block| block.rigid_width())
        .max()
        .unwrap_or(0);
    let soft = blocks
        .iter()
        .map(|block| block.soft_width())
        .max()
        .unwrap_or(0);
    let width = rigid.max(soft).max(1).min(available);

    let mut heights: Vec<usize> = blocks.iter().map(|block| block.height(width)).collect();

    if let Some(index) = blocks
        .iter()
        .position(|block| matches!(block, Block::List { .. }))
    {
        let mut fixed: usize = heights
            .iter()
            .enumerate()
            .filter(|(position, _)| *position != index)
            .map(|(_, height)| *height)
            .sum();
        let wanted = heights[index].min(MIN_LIST_ROWS);

        for position in 0..blocks.len() {
            if fixed + wanted <= rows {
                break;
            }
            if position != index && matches!(blocks[position], Block::Paragraph { .. }) {
                fixed -= heights[position];
                heights[position] = 0;
                let next = position + 1;
                if next != index
                    && matches!(blocks.get(next), Some(Block::Blank))
                    && heights[next] > 0
                {
                    fixed -= heights[next];
                    heights[next] = 0;
                }
            }
        }
        for position in 0..blocks.len() {
            if fixed + wanted <= rows {
                break;
            }
            if position != index && matches!(blocks[position], Block::Blank) {
                fixed -= heights[position];
                heights[position] = 0;
            }
        }
        heights[index] = heights[index].min(rows.saturating_sub(fixed));
    }

    let total: usize = heights.iter().sum();
    let mut y = rows.saturating_sub(total) / 2;
    let x = (cols.saturating_sub(width)) / 2;

    for (block, height) in blocks.into_iter().zip(heights) {
        if height == 0 {
            continue;
        }
        if y >= rows {
            break;
        }
        draw_block(block, x, y, width, height.min(rows - y), hover, clickable);
        y += height;
    }
}

fn tone_text(line: &str, tone: StatusTone) -> Text {
    match tone {
        StatusTone::Good => Text::new(line).success_color_range(..),
        StatusTone::Alert => Text::new(line).error_color_range(..),
        StatusTone::Neutral => quiet(line),
    }
}

fn draw_block(
    block: Block,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    hover: Option<(usize, usize)>,
    clickable: &mut HashMap<CoordinatesInLine, String>,
) {
    match block {
        Block::Blank => {},
        Block::Status { text, tone } => {
            for (offset, line) in word_wrap(&text, width).into_iter().take(height).enumerate() {
                print_text_with_coordinates(
                    tone_text(&line, tone),
                    x,
                    y + offset,
                    Some(width),
                    Some(1),
                );
            }
        },
        Block::Title(text) => {
            for (offset, line) in word_wrap(&text, width).into_iter().take(height).enumerate() {
                print_text_with_coordinates(
                    Text::new(&line).color_range(TITLE, ..),
                    x,
                    y + offset,
                    Some(width),
                    Some(1),
                );
            }
        },
        Block::Field { label, value, tone } => {
            let label_width = label.chars().count();
            let stacked = label_width + 4 >= width;
            if stacked {
                print_text_with_coordinates(
                    quiet(label.trim_end()),
                    x,
                    y,
                    Some(width),
                    Some(1),
                );
                for (offset, line) in word_wrap(&value, width).into_iter().enumerate() {
                    if offset + 1 >= height {
                        break;
                    }
                    print_text_with_coordinates(
                        tone_text(&line, tone),
                        x,
                        y + offset + 1,
                        Some(width),
                        Some(1),
                    );
                }
                return;
            }
            let body = wrap_after_label(&value, width - label_width, false);
            for (offset, line) in body.into_iter().take(height).enumerate() {
                let composed = if offset == 0 {
                    format!("{}{}", label, line)
                } else {
                    format!("{}{}", " ".repeat(label_width), line)
                };
                let element = tone_text(&composed, tone).unbold_range(0..label_width);
                print_text_with_coordinates(
                    element,
                    x,
                    y + offset,
                    Some(width),
                    Some(1),
                );
            }
        },
        Block::Paragraph { text } => {
            for (offset, line) in word_wrap(&text, width).into_iter().take(height).enumerate() {
                print_text_with_coordinates(quiet(&line), x, y + offset, Some(width), Some(1));
            }
        },
        Block::Url {
            prefix,
            prefix_tone,
            display,
            target,
            crop,
        } => {
            if crop {
                let prefix_width = prefix.chars().count().min(width);
                let shown = truncate_end(&display, width.saturating_sub(prefix_width));
                let shown_width = shown.chars().count();
                let composed = format!("{}{}", prefix, shown);
                let element = Text::new(&composed)
                    .color_range(VALUE, prefix_width..prefix_width + shown_width)
                    .unbold_range(0..prefix_width);
                print_text_with_coordinates(element, x, y, Some(width), Some(1));
                if !target.is_empty() {
                    let url_x = x + prefix_width;
                    clickable.insert(CoordinatesInLine::new(url_x, y, shown_width), target);
                    if hovering_on_line(url_x, y, shown_width, hover) {
                        render_text_with_underline(url_x, y, &shown);
                    }
                }
                return;
            }
            let prefix_width = prefix.chars().count();
            let stacked = prefix_width + 8 >= width;
            let (indent, first_row) = if stacked {
                if !prefix.trim().is_empty() {
                    let element = match prefix_tone {
                        Some(tone) => tone_text(prefix.trim_end(), tone),
                        None => quiet(prefix.trim_end()),
                    };
                    print_text_with_coordinates(element, x, y, Some(width), Some(1));
                    (0, 1)
                } else {
                    (0, 0)
                }
            } else {
                (prefix_width, 0)
            };

            let body_width = width.saturating_sub(indent).max(1);
            for (offset, line) in chunk(&display, body_width).into_iter().enumerate() {
                let row = first_row + offset;
                if row >= height {
                    break;
                }
                let line_width = line.chars().count();
                let composed = if offset == 0 && indent > 0 {
                    format!("{}{}", prefix, line)
                } else {
                    format!("{}{}", " ".repeat(indent), line)
                };
                let mut element =
                    Text::new(&composed).color_range(VALUE, indent..indent + line_width);
                if offset == 0 && indent > 0 {
                    element = match prefix_tone {
                        Some(StatusTone::Good) => element.success_color_range(0..prefix_width),
                        Some(StatusTone::Alert) => element.error_color_range(0..prefix_width),
                        Some(StatusTone::Neutral) | None => element.unbold_range(0..prefix_width),
                    };
                }
                print_text_with_coordinates(element, x, y + row, Some(width), Some(1));
                if !target.is_empty() {
                    let url_x = x + indent;
                    let url_y = y + row;
                    clickable.insert(
                        CoordinatesInLine::new(url_x, url_y, line_width),
                        target.clone(),
                    );
                    if hovering_on_line(url_x, url_y, line_width, hover) {
                        render_text_with_underline(url_x, url_y, &line);
                    }
                }
            }
        },
        Block::Keys(text) => {
            for (offset, line) in wrap_hints(&text, width).into_iter().take(height).enumerate() {
                print_text_with_coordinates(
                    highlight_keys(&line),
                    x,
                    y + offset,
                    Some(width),
                    Some(1),
                );
            }
        },
        Block::Bullets(lines) => {
            let inner = width.saturating_sub(3).max(1);
            let mut row = 0;
            for line in lines {
                let key_end = line.find('>').map(|offset| offset + 1);
                for (index, part) in wrap_bullet(&line, inner).into_iter().enumerate() {
                    if row >= height {
                        break;
                    }
                    let composed = if index == 0 {
                        format!(" > {}", part)
                    } else {
                        format!("   {}", part)
                    };
                    let mut element = Text::new(&composed);
                    if index == 0 {
                        if let Some(end) = key_end {
                            element = element.color_range(KEY, 3..3 + end);
                        }
                    }
                    print_text_with_coordinates(element, x, y + row, Some(width), Some(1));
                    row += 1;
                }
            }
        },
        Block::Prompt {
            label,
            buffer,
            hint,
        } => {
            let label_width = label.chars().count();
            let full = format!("{}{}_ {}", label, buffer, hint);
            let line = if full.chars().count() <= width {
                full
            } else {
                let without_hint = format!("{}{}_", label, buffer);
                if without_hint.chars().count() <= width {
                    without_hint
                } else {
                    let room = width.saturating_sub(label_width + 1);
                    let tail: String = buffer
                        .chars()
                        .skip(buffer.chars().count().saturating_sub(room))
                        .collect();
                    format!("{}{}_", label, tail)
                }
            };
            print_text_with_coordinates(
                highlight_keys(&line).color_range(TITLE, 0..label_width.min(width)),
                x,
                y,
                Some(width),
                Some(1),
            );
        },
        Block::Nav(items) => {
            print_text_with_coordinates(
                Text::new(NAV_KEY).color_range(KEY, ..),
                x,
                y,
                Some(NAV_KEY.chars().count()),
                Some(1),
            );
            let mut offset = NAV_KEY.chars().count() + NAV_GAP;
            for item in items {
                if offset + item.label.chars().count() + RIBBON_PADDING > width {
                    break;
                }
                let mut ribbon = Text::new(&item.label);
                if item.selected {
                    ribbon = ribbon.selected();
                }
                if !item.enabled {
                    ribbon = ribbon.disabled();
                }
                print_ribbon_with_coordinates(ribbon, x + offset, y, None, None);
                offset += item.label.chars().count() + RIBBON_PADDING;
            }
        },
        Block::Hints { label, hints } => {
            let label_width = label.chars().count();
            let body_width = width.saturating_sub(label_width).max(1);
            for (offset, line) in wrap_hints(&hints, body_width)
                .into_iter()
                .take(height)
                .enumerate()
            {
                if offset == 0 {
                    print_text_with_coordinates(
                        quiet(&label),
                        x,
                        y,
                        Some(label_width),
                        Some(1),
                    );
                }
                print_text_with_coordinates(
                    highlight_keys(&line),
                    x + label_width,
                    y + offset,
                    Some(body_width),
                    Some(1),
                );
            }
        },
        Block::Message { text, is_error } => {
            for (offset, line) in word_wrap(&text, width).into_iter().take(height).enumerate() {
                let element = if is_error {
                    Text::new(&line).error_color_range(..)
                } else {
                    Text::new(&line).success_color_range(..)
                };
                print_text_with_coordinates(element, x, y + offset, Some(width), Some(1));
            }
        },
        Block::List { rows, selected } => {
            crate::list::render_rows(
                &rows,
                selected,
                crate::list::ListArea {
                    x,
                    y,
                    width,
                    height,
                },
            );
        },
        Block::Empty { message, keys } => {
            let inner = width.saturating_sub(1).max(1);
            let mut row = 0;
            for line in word_wrap(&message, inner) {
                if row >= height {
                    break;
                }
                print_text_with_coordinates(
                    quiet(&line),
                    x + 1,
                    y + row,
                    Some(inner),
                    Some(1),
                );
                row += 1;
            }
            for line in wrap_hints(&keys, inner) {
                if row >= height {
                    break;
                }
                print_text_with_coordinates(
                    highlight_keys(&line),
                    x + 1,
                    y + row,
                    Some(inner),
                    Some(1),
                );
                row += 1;
            }
        },
    }
}

pub fn footer(message: Option<(&str, bool)>, label: &str, hints: &str) -> Block {
    match message {
        Some((text, is_error)) => Block::Message {
            text: text.to_owned(),
            is_error,
        },
        None => Block::hints(label, hints),
    }
}

pub fn hovering_on_line(
    x: usize,
    y: usize,
    width: usize,
    hover_coordinates: Option<(usize, usize)>,
) -> bool {
    match hover_coordinates {
        Some((hover_x, hover_y)) => hover_y == y && hover_x <= x + width && hover_x > x,
        None => false,
    }
}

pub fn render_text_with_underline(url_x: usize, url_y: usize, url_text: &str) {
    print!(
        "\u{1b}[{};{}H\u{1b}[m\u{1b}[4m{}",
        url_y + 1,
        url_x + 1,
        url_text,
    );
}
