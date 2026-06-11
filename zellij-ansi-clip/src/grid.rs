//! Internal grid model. `Cell` packs character, wide-flag, and SGR attributes.
//! `Grid` is a flat row-major `Vec<Cell>`.
//!
//! Kept deliberately small so the wasm blob stays compact.

use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub row: u16,
    pub col: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WideFlag {
    Normal,
    Lead,
    Trail,
}

impl Default for WideFlag {
    fn default() -> Self {
        WideFlag::Normal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // styled underline variants are parsed-in (see emit.rs) but
                    // the current serializer does not emit them yet.
pub(crate) enum UnderlineStyle {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl Default for UnderlineStyle {
    fn default() -> Self {
        UnderlineStyle::None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    Default,
    Named(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Default for Color {
    fn default() -> Self {
        Color::Default
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct SgrAttrs {
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: UnderlineStyle,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strike: bool,
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cell {
    pub ch: char,
    pub wide: WideFlag,
    pub sgr: SgrAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            wide: WideFlag::Normal,
            sgr: SgrAttrs::default(),
        }
    }
}

pub(crate) struct Grid {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Cell>,
}

impl Grid {
    pub fn new(rows: u16, cols: u16) -> Self {
        let len = (rows as usize) * (cols as usize);
        Grid {
            rows,
            cols,
            cells: vec![Cell::default(); len],
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows == self.rows && cols == self.cols {
            return;
        }
        let new_len = (rows as usize) * (cols as usize);
        let mut new_cells = vec![Cell::default(); new_len];
        let copy_rows = core::cmp::min(rows, self.rows);
        let copy_cols = core::cmp::min(cols, self.cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                let src = (r as usize) * (self.cols as usize) + (c as usize);
                let dst = (r as usize) * (cols as usize) + (c as usize);
                new_cells[dst] = self.cells[src];
            }
        }
        self.rows = rows;
        self.cols = cols;
        self.cells = new_cells;
    }

    #[inline]
    pub fn at(&self, row: u16, col: u16) -> Cell {
        let idx = (row as usize) * (self.cols as usize) + (col as usize);
        self.cells[idx]
    }

    #[inline]
    pub fn set(&mut self, row: u16, col: u16, cell: Cell) {
        let idx = (row as usize) * (self.cols as usize) + (col as usize);
        self.cells[idx] = cell;
    }

    pub fn clear_all(&mut self) {
        for cell in self.cells.iter_mut() {
            *cell = Cell::default();
        }
    }

    pub fn clear_line_from(&mut self, row: u16, from_col: u16) {
        if row >= self.rows {
            return;
        }
        let start = (row as usize) * (self.cols as usize) + (from_col as usize);
        let end = (row as usize) * (self.cols as usize) + (self.cols as usize);
        for i in start..end.min(self.cells.len()) {
            self.cells[i] = Cell::default();
        }
    }

    pub fn clear_line(&mut self, row: u16) {
        if row >= self.rows {
            return;
        }
        let start = (row as usize) * (self.cols as usize);
        let end = start + (self.cols as usize);
        for i in start..end.min(self.cells.len()) {
            self.cells[i] = Cell::default();
        }
    }

    pub fn clear_lines_from(&mut self, from_row: u16) {
        if from_row >= self.rows {
            return;
        }
        let start = (from_row as usize) * (self.cols as usize);
        for i in start..self.cells.len() {
            self.cells[i] = Cell::default();
        }
    }
}
