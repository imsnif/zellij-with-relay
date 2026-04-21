//! Emit strategy: walk the grid, write CUP at the start of each row, emit SGR
//! only when attributes change, emit characters, pad or clip according to viewer
//! dimensions, restore cursor + visibility at the end.

use alloc::string::String;
use alloc::vec::Vec;

use crate::grid::{Color, SgrAttrs, UnderlineStyle, WideFlag};
use crate::ClipState;

pub(crate) fn emit(state: &ClipState, viewer_rows: u16, viewer_cols: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(b"\x1b[2J");
    // Force a leading SGR reset so the first diff is meaningful.
    out.extend_from_slice(b"\x1b[0m");
    let mut last_sgr = SgrAttrs::default();

    let copy_rows = core::cmp::min(viewer_rows, state.grid.rows);
    let copy_cols = core::cmp::min(viewer_cols, state.grid.cols);

    for row in 0..copy_rows {
        write_cup(&mut out, row + 1, 1);
        let mut col: u16 = 0;
        while col < copy_cols {
            let cell = state.grid.at(row, col);
            if matches!(cell.wide, WideFlag::Trail) {
                // Trail cells are rendered by the preceding Lead. Emit a space
                // at the same column in case xterm has drifted.
                col += 1;
                continue;
            }
            // Wide Lead at the last copied column: the trail half falls outside
            // the viewer viewport; replace with a single space to avoid bisection.
            if matches!(cell.wide, WideFlag::Lead) && col + 1 >= copy_cols {
                emit_sgr_diff(&mut out, &last_sgr, &cell.sgr);
                last_sgr = cell.sgr;
                out.push(b' ');
                col += 1;
                continue;
            }
            emit_sgr_diff(&mut out, &last_sgr, &cell.sgr);
            last_sgr = cell.sgr;
            push_utf8(&mut out, cell.ch);
            col += if matches!(cell.wide, WideFlag::Lead) { 2 } else { 1 };
        }
    }

    // Padding rows / columns (when viewer > session).
    if viewer_cols > state.grid.cols {
        // Pad the right margin of every real row with spaces in default SGR.
        let default = SgrAttrs::default();
        for row in 0..copy_rows {
            write_cup(&mut out, row + 1, state.grid.cols + 1);
            emit_sgr_diff(&mut out, &last_sgr, &default);
            last_sgr = default;
            for _ in state.grid.cols..viewer_cols {
                out.push(b' ');
            }
        }
    }
    if viewer_rows > state.grid.rows {
        let default = SgrAttrs::default();
        for row in state.grid.rows..viewer_rows {
            write_cup(&mut out, row + 1, 1);
            emit_sgr_diff(&mut out, &last_sgr, &default);
            last_sgr = default;
            for _ in 0..viewer_cols {
                out.push(b' ');
            }
        }
    }

    // Restore cursor. Clamp the session cursor into the viewer rect.
    let cur_row = core::cmp::min(state.cursor.row, viewer_rows.saturating_sub(1));
    let cur_col = core::cmp::min(state.cursor.col, viewer_cols.saturating_sub(1));
    write_cup(&mut out, cur_row + 1, cur_col + 1);

    if state.cursor_visible {
        out.extend_from_slice(b"\x1b[?25h");
    } else {
        out.extend_from_slice(b"\x1b[?25l");
    }

    // Any sequences we didn't otherwise interpret get replayed at the end as a
    // safety valve. The order doesn't matter for most private-mode / OSC
    // controls, and it means future serializer additions keep flowing through
    // without requiring a clip.wasm rebuild.
    if !state.passthrough_tail.is_empty() {
        out.extend_from_slice(&state.passthrough_tail);
    }

    out
}

fn write_cup(out: &mut Vec<u8>, row_1based: u16, col_1based: u16) {
    let mut tmp = String::new();
    use core::fmt::Write;
    let _ = write!(&mut tmp, "\x1b[{};{}H", row_1based, col_1based);
    out.extend_from_slice(tmp.as_bytes());
}

fn push_utf8(out: &mut Vec<u8>, ch: char) {
    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    out.extend_from_slice(s.as_bytes());
}

fn emit_sgr_diff(out: &mut Vec<u8>, prev: &SgrAttrs, next: &SgrAttrs) {
    if prev == next {
        return;
    }
    // A full reset + selective reapply is shorter and more robust than tracking
    // per-bit on/off transitions. Terminals flatten it to the same result.
    let mut params: Vec<String> = Vec::new();
    params.push(String::from("0"));
    if next.bold {
        params.push(String::from("1"));
    }
    if next.faint {
        params.push(String::from("2"));
    }
    if next.italic {
        params.push(String::from("3"));
    }
    match next.underline {
        UnderlineStyle::None => {},
        UnderlineStyle::Single => params.push(String::from("4")),
        UnderlineStyle::Double => params.push(String::from("21")),
        UnderlineStyle::Curly => params.push(String::from("4:3")),
        UnderlineStyle::Dotted => params.push(String::from("4:4")),
        UnderlineStyle::Dashed => params.push(String::from("4:5")),
    }
    if next.blink {
        params.push(String::from("5"));
    }
    if next.inverse {
        params.push(String::from("7"));
    }
    if next.hidden {
        params.push(String::from("8"));
    }
    if next.strike {
        params.push(String::from("9"));
    }
    push_color_params(&mut params, &next.fg, 30, 39, 90, 38);
    push_color_params(&mut params, &next.bg, 40, 49, 100, 48);
    if !matches!(next.underline_color, Color::Default) {
        if let Color::Indexed(idx) = next.underline_color {
            params.push(format_params(&[58, 5, idx as u32]));
        } else if let Color::Rgb(r, g, b) = next.underline_color {
            params.push(format_params(&[58, 2, r as u32, g as u32, b as u32]));
        }
    }
    out.extend_from_slice(b"\x1b[");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            out.push(b';');
        }
        out.extend_from_slice(p.as_bytes());
    }
    out.push(b'm');
}

fn push_color_params(
    out: &mut Vec<String>,
    color: &Color,
    named_base: u32,
    default_code: u32,
    bright_base: u32,
    ext_code: u32,
) {
    match color {
        Color::Default => {
            // No emission needed — SGR 0 already reset everything.
            let _ = default_code;
        },
        Color::Named(n) => {
            let code = if *n < 8 {
                named_base + *n as u32
            } else {
                bright_base + (*n as u32 - 8)
            };
            out.push(format_num(code));
        },
        Color::Indexed(idx) => {
            out.push(format_params(&[ext_code, 5, *idx as u32]));
        },
        Color::Rgb(r, g, b) => {
            out.push(format_params(&[
                ext_code, 2, *r as u32, *g as u32, *b as u32,
            ]));
        },
    }
}

fn format_num(n: u32) -> String {
    let mut s = String::new();
    use core::fmt::Write;
    let _ = write!(&mut s, "{}", n);
    s
}

fn format_params(parts: &[u32]) -> String {
    let mut s = String::new();
    use core::fmt::Write;
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            s.push(';');
        }
        let _ = write!(&mut s, "{}", p);
    }
    s
}
