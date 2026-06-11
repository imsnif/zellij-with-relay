//! C ABI shim exposed to the browser through `WebAssembly.instantiate`.
//! Every pointer handed across the boundary is owned by the JS side until the
//! matching `clip_free` / `clip_free_state` is called.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem;

use crate::ClipState;

#[no_mangle]
pub extern "C" fn clip_alloc(len: usize) -> *mut u8 {
    let mut buf: Vec<u8> = Vec::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    mem::forget(buf);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn clip_free(ptr: *mut u8, len: usize) {
    let _ = Vec::from_raw_parts(ptr, 0, len);
}

#[no_mangle]
pub extern "C" fn clip_new(rows: u16, cols: u16) -> *mut ClipState {
    Box::into_raw(Box::new(ClipState::new(rows, cols)))
}

#[no_mangle]
pub unsafe extern "C" fn clip_free_state(ptr: *mut ClipState) {
    drop(Box::from_raw(ptr));
}

#[no_mangle]
pub unsafe extern "C" fn clip_apply(state: *mut ClipState, ansi: *const u8, len: usize) {
    let slice = core::slice::from_raw_parts(ansi, len);
    (*state).apply_chunk(slice);
}

#[no_mangle]
pub unsafe extern "C" fn clip_emit(
    state: *mut ClipState,
    rows: u16,
    cols: u16,
    out_len: *mut usize,
) -> *mut u8 {
    let out = (*state).emit(rows, cols);
    let len = out.len();
    let mut out = out.into_boxed_slice();
    let ptr = out.as_mut_ptr();
    mem::forget(out);
    *out_len = len;
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn clip_resize_session(state: *mut ClipState, rows: u16, cols: u16) {
    (*state).resize_session(rows, cols);
}
