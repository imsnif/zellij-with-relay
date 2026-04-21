//! CSI parser covering the subset emitted by
//! `zellij_server::output::Output::serialize_with_size`.
//!
//! Supported sequences:
//!   * `ESC [ <r> ; <c> H`            — CUP (Cursor Position), 1-based
//!   * `ESC [ <params> m`             — SGR
//!   * `ESC [ 2 J`                    — ED 2 (clear screen)
//!   * `ESC [ 0 K` / `ESC [ K`        — EL 0 (erase to end of line)
//!   * `ESC [ 2 K`                    — EL 2 (erase line)
//!   * `ESC [ ? 25 l` / `ESC [ ? 25 h` — cursor hide / show
//!   * `ESC [ <r> ; <c> ; <z> t`      — sixel text update (pass-through)
//!
//! Anything outside this subset — including unknown CSI, OSC, ST, DCS, SS2, SS3,
//! etc. — is passed through verbatim to the emitted output via `passthrough_tail`
//! so the viewer can still observe future serializer additions.
//!
//! Chunk-boundary safety: if a chunk ends mid-CSI, the partial bytes are retained
//! in `ClipState.partial_csi` and prepended to the next chunk.

use alloc::vec::Vec;

use crate::grid::{Cell, Color, Cursor, SgrAttrs, UnderlineStyle, WideFlag};
use crate::ClipState;

/// Public error surface. The parser itself is infallible (unknown sequences are
/// passed through); this type exists so consumers of a future, stricter API have
/// a stable identifier to catch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipError {}

pub(crate) fn apply(state: &mut ClipState, ansi: &[u8]) {
    let mut pending = core::mem::take(&mut state.partial_csi);
    pending.extend_from_slice(ansi);
    let bytes = pending.as_slice();

    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC. Need at least the intro byte that follows.
            if i + 1 >= bytes.len() {
                state.partial_csi = bytes[i..].to_vec();
                return;
            }
            let next = bytes[i + 1];
            match next {
                b'[' => {
                    // CSI
                    match parse_csi(&bytes[i + 2..]) {
                        CsiResult::Incomplete => {
                            state.partial_csi = bytes[i..].to_vec();
                            return;
                        },
                        CsiResult::Parsed {
                            consumed,
                            prefix,
                            params,
                            final_byte,
                        } => {
                            apply_csi(state, prefix, &params, final_byte, &bytes[i..i + 2 + consumed]);
                            i += 2 + consumed;
                        },
                    }
                },
                b']' => {
                    // OSC: find ST (ESC \) or BEL (0x07). Pass through verbatim.
                    match find_osc_end(&bytes[i + 2..]) {
                        None => {
                            state.partial_csi = bytes[i..].to_vec();
                            return;
                        },
                        Some(consumed) => {
                            state
                                .passthrough_tail
                                .extend_from_slice(&bytes[i..i + 2 + consumed]);
                            i += 2 + consumed;
                        },
                    }
                },
                b'P' | b'X' | b'^' | b'_' => {
                    // DCS / SOS / PM / APC: ST-terminated. Pass through verbatim.
                    match find_st(&bytes[i + 2..]) {
                        None => {
                            state.partial_csi = bytes[i..].to_vec();
                            return;
                        },
                        Some(consumed) => {
                            state
                                .passthrough_tail
                                .extend_from_slice(&bytes[i..i + 2 + consumed]);
                            i += 2 + consumed;
                        },
                    }
                },
                _ => {
                    // Short ESC sequence (ESC + single byte). Pass through verbatim.
                    state.passthrough_tail.extend_from_slice(&bytes[i..i + 2]);
                    i += 2;
                },
            }
        } else {
            // Plain text. Decode a single UTF-8 scalar and write at cursor.
            match decode_utf8_scalar(&bytes[i..]) {
                Utf8Scalar::Incomplete => {
                    state.partial_csi = bytes[i..].to_vec();
                    return;
                },
                Utf8Scalar::Done { ch, consumed } => {
                    write_char(state, ch);
                    i += consumed;
                },
                Utf8Scalar::Invalid { consumed } => {
                    // Malformed UTF-8; skip the offending byte(s).
                    i += consumed;
                },
            }
        }
    }
}

enum CsiResult {
    Incomplete,
    Parsed {
        consumed: usize,
        prefix: Option<u8>,
        params: Vec<u32>,
        final_byte: u8,
    },
}

fn parse_csi(bytes: &[u8]) -> CsiResult {
    let mut pos = 0;
    let prefix = if !bytes.is_empty() && matches!(bytes[0], b'?' | b'>' | b'<' | b'=') {
        let p = bytes[0];
        pos += 1;
        Some(p)
    } else {
        None
    };

    let mut params: Vec<u32> = Vec::new();
    let mut current: Option<u32> = None;
    while pos < bytes.len() {
        let b = bytes[pos];
        match b {
            b'0'..=b'9' => {
                let v = (b - b'0') as u32;
                current = Some(current.unwrap_or(0).saturating_mul(10).saturating_add(v));
                pos += 1;
            },
            b';' | b':' => {
                // Treat ':' and ';' identically for parameter separation. Real
                // subparameters (38:2:..:..) collapse into the same flat list,
                // which SGR parsing handles because it also accepts ';' for 38;2.
                params.push(current.unwrap_or(0));
                current = None;
                pos += 1;
            },
            0x20..=0x2f => {
                // Intermediate bytes. Pass as part of the CSI but unused here.
                pos += 1;
            },
            0x40..=0x7e => {
                // Final byte.
                if let Some(v) = current {
                    params.push(v);
                }
                return CsiResult::Parsed {
                    consumed: pos + 1,
                    prefix,
                    params,
                    final_byte: b,
                };
            },
            _ => {
                // Malformed — give up and treat as final byte.
                if let Some(v) = current {
                    params.push(v);
                }
                return CsiResult::Parsed {
                    consumed: pos + 1,
                    prefix,
                    params,
                    final_byte: b,
                };
            },
        }
    }
    CsiResult::Incomplete
}

fn find_osc_end(bytes: &[u8]) -> Option<usize> {
    // Terminated by BEL (0x07) or ST (ESC \).
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x07 {
            return Some(i + 1);
        }
        if b == 0x1b {
            if i + 1 >= bytes.len() {
                return None;
            }
            if bytes[i + 1] == b'\\' {
                return Some(i + 2);
            }
        }
        i += 1;
    }
    None
}

fn find_st(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 >= bytes.len() {
                return None;
            }
            if bytes[i + 1] == b'\\' {
                return Some(i + 2);
            }
        }
        i += 1;
    }
    None
}

enum Utf8Scalar {
    Done { ch: char, consumed: usize },
    Incomplete,
    Invalid { consumed: usize },
}

fn decode_utf8_scalar(bytes: &[u8]) -> Utf8Scalar {
    if bytes.is_empty() {
        return Utf8Scalar::Incomplete;
    }
    let b0 = bytes[0];
    let expected = if b0 < 0x80 {
        1
    } else if b0 & 0xe0 == 0xc0 {
        2
    } else if b0 & 0xf0 == 0xe0 {
        3
    } else if b0 & 0xf8 == 0xf0 {
        4
    } else {
        return Utf8Scalar::Invalid { consumed: 1 };
    };
    if bytes.len() < expected {
        return Utf8Scalar::Incomplete;
    }
    match core::str::from_utf8(&bytes[..expected]) {
        Ok(s) => {
            let ch = s.chars().next().unwrap();
            Utf8Scalar::Done {
                ch,
                consumed: expected,
            }
        },
        Err(_) => Utf8Scalar::Invalid { consumed: 1 },
    }
}

fn apply_csi(
    state: &mut ClipState,
    prefix: Option<u8>,
    params: &[u32],
    final_byte: u8,
    raw: &[u8],
) {
    match (prefix, final_byte) {
        (None, b'H') | (None, b'f') => {
            let row = params.first().copied().unwrap_or(1);
            let col = params.get(1).copied().unwrap_or(1);
            let row_zero = if row == 0 { 0 } else { row - 1 };
            let col_zero = if col == 0 { 0 } else { col - 1 };
            let max_row = state.grid.rows.saturating_sub(1);
            let max_col = state.grid.cols.saturating_sub(1);
            state.cursor = Cursor {
                row: (row_zero as u16).min(max_row),
                col: (col_zero as u16).min(max_col),
            };
        },
        (None, b'm') => {
            apply_sgr(&mut state.sgr, params);
        },
        (None, b'J') => {
            let mode = params.first().copied().unwrap_or(0);
            match mode {
                2 | 3 => {
                    state.grid.clear_all();
                    state.cursor = Cursor { row: 0, col: 0 };
                },
                0 => {
                    // Clear from cursor to end of screen.
                    state
                        .grid
                        .clear_line_from(state.cursor.row, state.cursor.col);
                    if state.cursor.row + 1 < state.grid.rows {
                        state.grid.clear_lines_from(state.cursor.row + 1);
                    }
                },
                1 => {
                    // Clear from start of screen to cursor.
                    for r in 0..state.cursor.row {
                        state.grid.clear_line(r);
                    }
                    let c = state.cursor.col;
                    for col in 0..=c.min(state.grid.cols.saturating_sub(1)) {
                        state.grid.set(state.cursor.row, col, Cell::default());
                    }
                },
                _ => {},
            }
        },
        (None, b'K') => {
            let mode = params.first().copied().unwrap_or(0);
            match mode {
                0 => state
                    .grid
                    .clear_line_from(state.cursor.row, state.cursor.col),
                1 => {
                    let c = state.cursor.col;
                    for col in 0..=c.min(state.grid.cols.saturating_sub(1)) {
                        state.grid.set(state.cursor.row, col, Cell::default());
                    }
                },
                2 => state.grid.clear_line(state.cursor.row),
                _ => {},
            }
        },
        (Some(b'?'), b'h') | (Some(b'?'), b'l') => {
            for p in params {
                if *p == 25 {
                    state.cursor_visible = final_byte == b'h';
                }
            }
            // Non-25 private modes are captured verbatim as a safety valve so
            // xterm.js still observes them.
            if !params.iter().any(|p| *p == 25) {
                state.passthrough_tail.extend_from_slice(raw);
            }
        },
        _ => {
            // Unknown CSI: pass through verbatim.
            state.passthrough_tail.extend_from_slice(raw);
        },
    }
}

fn apply_sgr(sgr: &mut SgrAttrs, params: &[u32]) {
    if params.is_empty() {
        *sgr = SgrAttrs::default();
        return;
    }
    let mut i = 0;
    while i < params.len() {
        let p = params[i];
        match p {
            0 => *sgr = SgrAttrs::default(),
            1 => sgr.bold = true,
            2 => sgr.faint = true,
            3 => sgr.italic = true,
            4 => sgr.underline = UnderlineStyle::Single,
            5 => sgr.blink = true,
            7 => sgr.inverse = true,
            8 => sgr.hidden = true,
            9 => sgr.strike = true,
            21 => sgr.underline = UnderlineStyle::Double,
            22 => {
                sgr.bold = false;
                sgr.faint = false;
            },
            23 => sgr.italic = false,
            24 => sgr.underline = UnderlineStyle::None,
            25 => sgr.blink = false,
            27 => sgr.inverse = false,
            28 => sgr.hidden = false,
            29 => sgr.strike = false,
            30..=37 => sgr.fg = Color::Named(p as u8 - 30),
            38 => {
                if let Some((color, advance)) = parse_ext_color(&params[i + 1..]) {
                    sgr.fg = color;
                    i += advance;
                }
            },
            39 => sgr.fg = Color::Default,
            40..=47 => sgr.bg = Color::Named(p as u8 - 40),
            48 => {
                if let Some((color, advance)) = parse_ext_color(&params[i + 1..]) {
                    sgr.bg = color;
                    i += advance;
                }
            },
            49 => sgr.bg = Color::Default,
            58 => {
                if let Some((color, advance)) = parse_ext_color(&params[i + 1..]) {
                    sgr.underline_color = color;
                    i += advance;
                }
            },
            59 => sgr.underline_color = Color::Default,
            90..=97 => sgr.fg = Color::Named(p as u8 - 90 + 8),
            100..=107 => sgr.bg = Color::Named(p as u8 - 100 + 8),
            _ => {},
        }
        i += 1;
    }
}

fn parse_ext_color(params: &[u32]) -> Option<(Color, usize)> {
    match params.first().copied()? {
        5 => {
            let idx = params.get(1).copied()? as u8;
            Some((Color::Indexed(idx), 2))
        },
        2 => {
            let r = params.get(1).copied()? as u8;
            let g = params.get(2).copied()? as u8;
            let b = params.get(3).copied()? as u8;
            Some((Color::Rgb(r, g, b), 4))
        },
        _ => None,
    }
}

fn write_char(state: &mut ClipState, ch: char) {
    // Handle a few control characters the serializer may emit.
    match ch {
        '\r' => {
            state.cursor.col = 0;
            return;
        },
        '\n' => {
            if state.cursor.row + 1 < state.grid.rows {
                state.cursor.row += 1;
            }
            return;
        },
        '\x08' => {
            if state.cursor.col > 0 {
                state.cursor.col -= 1;
            }
            return;
        },
        '\t' => {
            let tabstop = ((state.cursor.col / 8) + 1) * 8;
            state.cursor.col = tabstop.min(state.grid.cols.saturating_sub(1));
            return;
        },
        c if (c as u32) < 0x20 => return,
        _ => {},
    }

    if state.grid.rows == 0 || state.grid.cols == 0 {
        return;
    }

    let width = char_width(ch);
    if width == 0 {
        // Zero-width (combining) characters: skip — grid is flat, no combining
        // support. The user-visible effect on clipped output is negligible.
        return;
    }
    let row = state.cursor.row;
    let col = state.cursor.col;
    if row >= state.grid.rows || col >= state.grid.cols {
        return;
    }
    if width == 2 {
        if col + 1 >= state.grid.cols {
            // Wide char at last column: write a space instead.
            state.grid.set(
                row,
                col,
                Cell {
                    ch: ' ',
                    wide: WideFlag::Normal,
                    sgr: state.sgr,
                },
            );
            advance_cursor(state, 1);
            return;
        }
        state.grid.set(
            row,
            col,
            Cell {
                ch,
                wide: WideFlag::Lead,
                sgr: state.sgr,
            },
        );
        state.grid.set(
            row,
            col + 1,
            Cell {
                ch: ' ',
                wide: WideFlag::Trail,
                sgr: state.sgr,
            },
        );
        advance_cursor(state, 2);
    } else {
        state.grid.set(
            row,
            col,
            Cell {
                ch,
                wide: WideFlag::Normal,
                sgr: state.sgr,
            },
        );
        advance_cursor(state, 1);
    }
}

fn advance_cursor(state: &mut ClipState, n: u16) {
    let new_col = state.cursor.col as u32 + n as u32;
    if new_col >= state.grid.cols as u32 {
        // Clamp at the right margin; the serializer always repositions via CUP
        // at the start of each row so implicit wrap is not needed.
        state.cursor.col = state.grid.cols.saturating_sub(1);
    } else {
        state.cursor.col = new_col as u16;
    }
}

/// Minimal East Asian Width approximation. Returns 2 for the principal CJK and
/// emoji ranges, 0 for combining marks / zero-width joiners, otherwise 1.
/// The set is deliberately conservative — `unicode-width` would bloat the wasm.
fn char_width(ch: char) -> u16 {
    let cp = ch as u32;
    // Zero-width
    if cp == 0x200B
        || cp == 0x200C
        || cp == 0x200D
        || cp == 0xFEFF
        || (0x0300..=0x036F).contains(&cp)
        || (0x1AB0..=0x1AFF).contains(&cp)
        || (0x1DC0..=0x1DFF).contains(&cp)
        || (0x20D0..=0x20FF).contains(&cp)
        || (0xFE00..=0xFE0F).contains(&cp)
        || (0xFE20..=0xFE2F).contains(&cp)
    {
        return 0;
    }
    // Wide: CJK, Hangul, fullwidth forms, emoji.
    if (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0x303E).contains(&cp)
        || (0x3041..=0x33FF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xA000..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp)
        || (0x20000..=0x2FFFD).contains(&cp)
        || (0x30000..=0x3FFFD).contains(&cp)
    {
        return 2;
    }
    1
}
