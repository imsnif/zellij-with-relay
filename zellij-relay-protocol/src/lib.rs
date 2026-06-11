//! Protocol types shared between Zellij and the `zellij-relay` tunnel server.
//!
//! This crate intentionally depends only on `prost` and `serde` so that the
//! relay binary can link against it without pulling in the full
//! `zellij-utils` dependency tree.

pub mod generated {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/prost_relay/generated_relay_protocol.rs"
    ));
}

mod conversion;
pub mod crypto;

#[cfg(test)]
mod tests;

pub use conversion::{decode_control_frame, decode_terminal_frame, ControlMessage, TerminalMessage};

use std::ops::RangeInclusive;

/// Current relay tunnel protocol version. Bumped on breaking changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Inclusive range of protocol versions a relay built against this crate
/// accepts on `TunnelAuth`. Zellij clients speak a single
/// `PROTOCOL_VERSION`; the range exists so one relay build can support a
/// rolling window of Zellij versions.
pub const SUPPORTED_PROTOCOL_VERSIONS: RangeInclusive<u32> = 1..=1;
