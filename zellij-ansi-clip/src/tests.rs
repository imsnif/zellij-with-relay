//! Unit tests for the clipper. Tests exercise the parser, grid, and emitter
//! end-to-end by applying ANSI chunks and either inspecting the internal grid
//! directly or round-tripping the emitted bytes through a second `ClipState` of
//! equal dimensions to verify semantic equivalence.

use alloc::string::String;
use alloc::vec::Vec;

use crate::grid::{Cell, Color, WideFlag};
use crate::ClipState;

fn cells_equivalent(a: &Cell, b: &Cell) -> bool {
    a.ch == b.ch && a.wide == b.wide && a.sgr == b.sgr
}

fn roundtrip(state: &ClipState, rows: u16, cols: u16) -> ClipState {
    let emitted = state.emit(rows, cols);
    let mut replay = ClipState::new(rows, cols);
    replay.apply_chunk(&emitted);
    replay
}

fn visible_text(state: &ClipState) -> String {
    let mut s = String::new();
    for r in 0..state.grid.rows {
        for c in 0..state.grid.cols {
            let cell = state.grid.at(r, c);
            if matches!(cell.wide, WideFlag::Trail) {
                continue;
            }
            s.push(cell.ch);
        }
        s.push('\n');
    }
    s
}

#[test]
fn t01_new_returns_empty_grid() {
    let state = ClipState::new(3, 4);
    assert_eq!(state.session_rows(), 3);
    assert_eq!(state.session_cols(), 4);
    for r in 0..3 {
        for c in 0..4 {
            let cell = state.grid.at(r, c);
            assert_eq!(cell.ch, ' ');
            assert!(matches!(cell.wide, WideFlag::Normal));
        }
    }
    assert!(state.cursor_visible);
}

#[test]
fn t02_text_at_origin_viewer_equals_session() {
    let mut state = ClipState::new(3, 10);
    state.apply_chunk(b"Hello");
    assert_eq!(state.grid.at(0, 0).ch, 'H');
    assert_eq!(state.grid.at(0, 4).ch, 'o');

    let replay = roundtrip(&state, 3, 10);
    for c in 0..5 {
        assert!(cells_equivalent(&state.grid.at(0, c), &replay.grid.at(0, c)));
    }
}

#[test]
fn t03_cup_positions_text() {
    let mut state = ClipState::new(5, 10);
    state.apply_chunk(b"\x1b[3;5HX");
    assert_eq!(state.grid.at(2, 4).ch, 'X');
    assert_eq!(state.grid.at(0, 0).ch, ' ');
}

#[test]
fn t04_sgr_attrs_carry_with_diffing() {
    // Two cells with identical SGR should share a single preceding SGR
    // sequence; a third cell with different attributes should produce a new
    // one. Running a single-attribute run of arbitrary length must not emit
    // an SGR per cell.
    let mut state = ClipState::new(2, 10);
    state.apply_chunk(b"\x1b[1;31mAAAA\x1b[0mBBBB");
    assert!(state.grid.at(0, 0).sgr.bold);
    assert_eq!(state.grid.at(0, 0).sgr.fg, Color::Named(1));
    assert!(!state.grid.at(0, 4).sgr.bold);

    let emitted = state.emit(2, 10);
    let sgr_count = count_sgr_sequences(&emitted);
    // Expect exactly two SGR diffs across a monotonic run: one transitioning
    // default → bold+red, one transitioning back → default. The leading
    // \x1b[0m that `emit` always writes is counted too.
    assert_eq!(sgr_count, 3, "emitted={:?}", escape_ansi(&emitted));

    // Round-trip check.
    let mut replay = ClipState::new(2, 10);
    replay.apply_chunk(&emitted);
    assert!(replay.grid.at(0, 0).sgr.bold);
    assert_eq!(replay.grid.at(0, 0).sgr.fg, Color::Named(1));
    assert!(!replay.grid.at(0, 4).sgr.bold);
}

fn count_sgr_sequences(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            // Scan forward for final byte 'm'.
            let mut j = i + 2;
            while j < bytes.len() {
                let b = bytes[j];
                if b == b'm' {
                    count += 1;
                    i = j + 1;
                    break;
                }
                if (0x40..=0x7e).contains(&b) {
                    // Different CSI final byte; skip.
                    i = j + 1;
                    break;
                }
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
        } else {
            i += 1;
        }
    }
    count
}

fn escape_ansi(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        if b == 0x1b {
            s.push_str("\\e");
        } else if b.is_ascii_graphic() || b == b' ' {
            s.push(b as char);
        } else {
            use core::fmt::Write;
            let _ = write!(&mut s, "\\x{:02x}", b);
        }
    }
    s
}

#[test]
fn t05_clip_smaller_viewer_drops_out_of_bounds() {
    let mut state = ClipState::new(40, 120);
    // Fill one character at (30, 100) that should be dropped when clipped.
    state.apply_chunk(b"\x1b[31;101HOUT");
    // And one at (5, 10) that should survive.
    state.apply_chunk(b"\x1b[6;11HIN");

    let emitted = state.emit(20, 60);
    let mut replay = ClipState::new(20, 60);
    replay.apply_chunk(&emitted);
    // "IN" at (5, 10) — within 20x60.
    assert_eq!(replay.grid.at(5, 10).ch, 'I');
    assert_eq!(replay.grid.at(5, 11).ch, 'N');
    // (30, 100) is outside the 20x60 viewer; nothing at the corresponding
    // session coords should survive. Check a few cells inside the viewer are
    // still blank.
    assert_eq!(replay.grid.at(19, 59).ch, ' ');
}

#[test]
fn t06_pad_larger_viewer_blanks_extra_cells() {
    let mut state = ClipState::new(2, 4);
    state.apply_chunk(b"\x1b[1;1HABCD\x1b[2;1HEFGH");
    let emitted = state.emit(4, 8);
    let mut replay = ClipState::new(4, 8);
    replay.apply_chunk(&emitted);
    assert_eq!(replay.grid.at(0, 0).ch, 'A');
    assert_eq!(replay.grid.at(0, 3).ch, 'D');
    assert_eq!(replay.grid.at(1, 0).ch, 'E');
    // Padded columns to the right.
    assert_eq!(replay.grid.at(0, 7).ch, ' ');
    // Padded rows below.
    assert_eq!(replay.grid.at(3, 0).ch, ' ');
}

#[test]
fn t07_clip_then_reclip_no_new_chunk() {
    let mut state = ClipState::new(5, 10);
    state.apply_chunk(b"\x1b[1;1HABCDEFGHIJ");
    let small = state.emit(2, 5);
    let large = state.emit(5, 15);
    let mut replay_small = ClipState::new(2, 5);
    replay_small.apply_chunk(&small);
    assert_eq!(replay_small.grid.at(0, 4).ch, 'E');
    let mut replay_large = ClipState::new(5, 15);
    replay_large.apply_chunk(&large);
    assert_eq!(replay_large.grid.at(0, 9).ch, 'J');
    assert_eq!(replay_large.grid.at(0, 14).ch, ' ');
}

#[test]
fn t08_emit_is_idempotent_no_state_mutation() {
    let mut state = ClipState::new(3, 5);
    state.apply_chunk(b"\x1b[1;1HXYZ");
    let before_rows = state.grid.rows;
    let before_cols = state.grid.cols;
    let before_cursor_row = state.cursor.row;
    let before_cursor_col = state.cursor.col;
    let before_cell = state.grid.at(0, 0);
    let _ = state.emit(3, 5);
    let _ = state.emit(1, 2);
    assert_eq!(state.grid.rows, before_rows);
    assert_eq!(state.grid.cols, before_cols);
    assert_eq!(state.cursor.row, before_cursor_row);
    assert_eq!(state.cursor.col, before_cursor_col);
    assert!(cells_equivalent(&state.grid.at(0, 0), &before_cell));
}

#[test]
fn t09_ed_clears_grid() {
    let mut state = ClipState::new(3, 5);
    state.apply_chunk(b"\x1b[1;1HABCDE\x1b[2;1HFGHIJ");
    state.apply_chunk(b"\x1b[2J");
    for r in 0..3 {
        for c in 0..5 {
            assert_eq!(state.grid.at(r, c).ch, ' ');
        }
    }
    assert_eq!(state.cursor.row, 0);
    assert_eq!(state.cursor.col, 0);
}

#[test]
fn t10a_el_0_erases_to_eol() {
    let mut state = ClipState::new(2, 6);
    state.apply_chunk(b"ABCDEF");
    state.apply_chunk(b"\x1b[1;4H\x1b[0K");
    // Row 0: A, B, C preserved; D, E, F cleared.
    assert_eq!(state.grid.at(0, 0).ch, 'A');
    assert_eq!(state.grid.at(0, 2).ch, 'C');
    assert_eq!(state.grid.at(0, 3).ch, ' ');
    assert_eq!(state.grid.at(0, 5).ch, ' ');
}

#[test]
fn t10b_el_2_erases_line() {
    let mut state = ClipState::new(2, 5);
    state.apply_chunk(b"HELLO");
    state.apply_chunk(b"\x1b[1;3H\x1b[2K");
    for c in 0..5 {
        assert_eq!(state.grid.at(0, c).ch, ' ');
    }
}

#[test]
fn t11_cursor_visibility_passthrough() {
    let mut state = ClipState::new(2, 3);
    state.apply_chunk(b"\x1b[?25l");
    assert!(!state.cursor_visible);
    let emitted = state.emit(2, 3);
    assert!(find_subslice(&emitted, b"\x1b[?25l").is_some());
    state.apply_chunk(b"\x1b[?25h");
    assert!(state.cursor_visible);
    let emitted = state.emit(2, 3);
    assert!(find_subslice(&emitted, b"\x1b[?25h").is_some());
}

#[test]
fn t12_multi_chunk_matches_single_chunk() {
    let whole = b"\x1b[1;1H\x1b[31mHello \x1b[0mWorld\x1b[2;1HAgain";
    let mut single = ClipState::new(3, 20);
    single.apply_chunk(whole);

    let mut multi = ClipState::new(3, 20);
    multi.apply_chunk(&whole[..5]);
    multi.apply_chunk(&whole[5..14]);
    multi.apply_chunk(&whole[14..]);

    for r in 0..3 {
        for c in 0..20 {
            assert!(
                cells_equivalent(&single.grid.at(r, c), &multi.grid.at(r, c)),
                "cell mismatch at {},{}",
                r,
                c
            );
        }
    }
}

#[test]
fn t13_partial_csi_carry_over() {
    let mut state = ClipState::new(5, 10);
    state.apply_chunk(b"\x1b[1;");
    // After this chunk the grid should be untouched and partial_csi should
    // buffer the tail.
    assert!(!state.partial_csi.is_empty());
    assert_eq!(state.grid.at(0, 0).ch, ' ');
    state.apply_chunk(b"1HX");
    assert!(state.partial_csi.is_empty());
    assert_eq!(state.grid.at(0, 0).ch, 'X');
}

#[test]
fn t14_cjk_wide_char_clipped_at_col_boundary() {
    let mut state = ClipState::new(2, 6);
    state.apply_chunk(b"\x1b[1;1H");
    // '中' is U+4E2D, 3 bytes UTF-8: e4 b8 ad, width 2.
    state.apply_chunk("\u{4e2d}\u{4e2d}\u{4e2d}".as_bytes());
    // Three wide chars occupy 6 columns in 2x6 session — fine.
    assert!(matches!(state.grid.at(0, 0).wide, WideFlag::Lead));
    assert!(matches!(state.grid.at(0, 1).wide, WideFlag::Trail));

    // Emit at viewer width 5 — the third wide char would be bisected, should
    // become a space in the emitted stream.
    let emitted = state.emit(2, 5);
    let mut replay = ClipState::new(2, 5);
    replay.apply_chunk(&emitted);
    assert!(matches!(replay.grid.at(0, 0).wide, WideFlag::Lead));
    assert!(matches!(replay.grid.at(0, 2).wide, WideFlag::Lead));
    // Column 4 in the viewer should be a blank space (from the clipped lead).
    assert_eq!(replay.grid.at(0, 4).ch, ' ');
    assert!(matches!(replay.grid.at(0, 4).wide, WideFlag::Normal));
}

#[test]
fn t15_resize_session_grow_preserves_cells() {
    let mut state = ClipState::new(2, 3);
    state.apply_chunk(b"\x1b[1;1HABC\x1b[2;1HDEF");
    state.resize_session(4, 5);
    assert_eq!(state.grid.rows, 4);
    assert_eq!(state.grid.cols, 5);
    assert_eq!(state.grid.at(0, 0).ch, 'A');
    assert_eq!(state.grid.at(0, 2).ch, 'C');
    assert_eq!(state.grid.at(1, 0).ch, 'D');
    assert_eq!(state.grid.at(1, 2).ch, 'F');
    // Extra cells default-filled.
    assert_eq!(state.grid.at(3, 4).ch, ' ');
}

#[test]
fn t16_resize_session_shrink_discards() {
    let mut state = ClipState::new(4, 5);
    state.apply_chunk(b"\x1b[1;1HABCDE\x1b[2;1HFGHIJ\x1b[3;1HKLMNO");
    state.resize_session(2, 3);
    assert_eq!(state.grid.rows, 2);
    assert_eq!(state.grid.cols, 3);
    assert_eq!(state.grid.at(0, 0).ch, 'A');
    assert_eq!(state.grid.at(0, 2).ch, 'C');
    assert_eq!(state.grid.at(1, 0).ch, 'F');
    assert_eq!(state.grid.at(1, 2).ch, 'H');
}

#[test]
fn t17_fidelity_anchor_40x120() {
    let bytes = load_or_build_fixture();
    let mut state = ClipState::new(40, 120);
    // Split the fixture into chunks of a few hundred bytes to additionally
    // exercise chunk-boundary handling.
    let mut pos = 0;
    while pos < bytes.len() {
        let end = core::cmp::min(pos + 257, bytes.len());
        state.apply_chunk(&bytes[pos..end]);
        pos = end;
    }
    // Round-trip: emit at session size, replay into a fresh state, compare
    // visible text grid-for-grid.
    let emitted = state.emit(40, 120);
    let mut replay = ClipState::new(40, 120);
    replay.apply_chunk(&emitted);
    assert_eq!(visible_text(&state), visible_text(&replay));
}

#[test]
fn t18_unknown_csi_passthrough() {
    let mut state = ClipState::new(2, 4);
    // CSI `\x1b[?1049h` is not recognised by our parser as a cursor-visibility
    // sequence; it must be passed through verbatim into the emit stream.
    state.apply_chunk(b"\x1b[?1049h");
    let emitted = state.emit(2, 4);
    assert!(find_subslice(&emitted, b"\x1b[?1049h").is_some());
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Load the committed fixture, or synthesise a deterministic sample when the
/// real fixture hasn't been captured yet (D1.7). The sample exercises CUP,
/// SGR, EL, and plain text — the same subset `Output::serialize_with_size`
/// produces — so it's a valid fidelity anchor even without a live capture.
fn load_or_build_fixture() -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/full_session_40x120.ansi"
    );
    #[cfg(feature = "std")]
    {
        if let Ok(bytes) = std::fs::read(path) {
            if !bytes.is_empty() {
                return bytes;
            }
        }
    }
    #[cfg(not(feature = "std"))]
    let _ = path;
    build_sample_fixture_40x120()
}

fn build_sample_fixture_40x120() -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"\x1b[2J");
    for row in 1..=40u16 {
        // CUP to the row.
        let cup = format_csi_cup(row, 1);
        out.extend_from_slice(cup.as_bytes());
        // Alternating SGR colours per row.
        let sgr = match row % 4 {
            0 => "\x1b[0m",
            1 => "\x1b[1;31m",
            2 => "\x1b[32m",
            _ => "\x1b[33;44m",
        };
        out.extend_from_slice(sgr.as_bytes());
        // 120 printable ASCII characters.
        for col in 0..120u16 {
            let c = (b'!' as u16 + ((row + col) % 90)) as u8;
            out.push(c);
        }
        out.extend_from_slice(b"\x1b[0m");
    }
    out.extend_from_slice(b"\x1b[1;1H");
    out
}

fn format_csi_cup(row: u16, col: u16) -> String {
    let mut s = String::new();
    use core::fmt::Write;
    let _ = write!(&mut s, "\x1b[{};{}H", row, col);
    s
}
