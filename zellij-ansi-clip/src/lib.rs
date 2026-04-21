//! ANSI clipping / padding state machine for Zellij read-only relay viewers.
//!
//! The Zellij server emits a single untruncated render stream at the full session
//! viewport size. Each read-only viewer receives an identical copy of that stream.
//! This crate maintains an in-memory grid by replaying the stream, and `emit`s a
//! clipped (smaller viewer) or padded (larger viewer) ANSI byte sequence that
//! xterm.js (or a native terminal) can write verbatim to produce a faithful render
//! for arbitrary viewer dimensions.
//!
//! The parser recognises only the CSI subset produced by
//! `zellij_server::output::Output::serialize_with_size`. Anything unrecognised is
//! passed through verbatim as a safety valve.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod emit;
mod grid;
mod parser;

#[cfg(test)]
mod tests;

#[cfg(all(target_arch = "wasm32", feature = "wasm"))]
mod wasm;

use alloc::vec::Vec;

pub use parser::ClipError;

/// Opaque state tracked across `apply_chunk` calls. A `ClipState` holds the full
/// session grid, the current cursor, partial-CSI carry-over, and cursor
/// visibility. `emit` is a pure projection onto an arbitrary viewer size.
pub struct ClipState {
    grid: grid::Grid,
    cursor: grid::Cursor,
    sgr: grid::SgrAttrs,
    cursor_visible: bool,
    partial_csi: Vec<u8>,
    passthrough_tail: Vec<u8>,
}

impl ClipState {
    pub fn new(session_rows: u16, session_cols: u16) -> Self {
        Self {
            grid: grid::Grid::new(session_rows, session_cols),
            cursor: grid::Cursor { row: 0, col: 0 },
            sgr: grid::SgrAttrs::default(),
            cursor_visible: true,
            partial_csi: Vec::new(),
            passthrough_tail: Vec::new(),
        }
    }

    pub fn apply_chunk(&mut self, ansi: &[u8]) {
        parser::apply(self, ansi);
    }

    pub fn emit(&self, viewer_rows: u16, viewer_cols: u16) -> Vec<u8> {
        emit::emit(self, viewer_rows, viewer_cols)
    }

    pub fn resize_session(&mut self, rows: u16, cols: u16) {
        self.grid.resize(rows, cols);
        if self.cursor.row >= rows {
            self.cursor.row = rows.saturating_sub(1);
        }
        if self.cursor.col >= cols {
            self.cursor.col = cols.saturating_sub(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn session_rows(&self) -> u16 {
        self.grid.rows
    }
    #[cfg(test)]
    pub(crate) fn session_cols(&self) -> u16 {
        self.grid.cols
    }
}
