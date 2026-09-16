use std::fmt;

use zellij_relay_protocol::TunnelErrorCode;
use zellij_utils::data::RelayFailureReason;

/// Typed relay handshake rejection carried out of the control/terminal tunnel
/// handshake so callers recover the `TunnelErrorCode` (via `anyhow` downcast)
/// instead of parsing the diagnostic message.
#[derive(Debug)]
pub struct RelayHandshakeError {
    pub code: TunnelErrorCode,
    pub message: String,
}

impl RelayHandshakeError {
    pub fn new(code: TunnelErrorCode, message: String) -> Self {
        RelayHandshakeError { code, message }
    }
}

impl fmt::Display for RelayHandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RelayHandshakeError {}

/// Map a wire `TunnelErrorCode` onto the plugin-facing `RelayFailureReason`.
pub fn failure_reason_for(code: &TunnelErrorCode) -> RelayFailureReason {
    match code {
        TunnelErrorCode::AuthRejected => RelayFailureReason::AuthRejected,
        TunnelErrorCode::ProtocolVersionUnsupported {
            supported_min,
            supported_max,
            offered_version,
        } => RelayFailureReason::ProtocolMismatch {
            supported_min: *supported_min,
            supported_max: *supported_max,
            offered_version: *offered_version,
        },
        // Terminal binding failure signals a linking bug/stale-binary or an
        // attack on the terminal socket, not a bad account credential — it must
        // NOT surface as the plugin's auth-rejected UX, so it maps to the
        // generic unreachable/retry path.
        TunnelErrorCode::Unspecified
        | TunnelErrorCode::MalformedFrame
        | TunnelErrorCode::UnexpectedFrame
        | TunnelErrorCode::MissingSlug
        | TunnelErrorCode::UnknownSlug
        | TunnelErrorCode::TunnelIdMismatch
        | TunnelErrorCode::TerminalBindingRejected => RelayFailureReason::Unreachable,
    }
}

/// Recover the typed `RelayFailureReason` from an `anyhow` error produced by the
/// handshake path, defaulting to `Unreachable` for untyped errors.
pub fn failure_reason_from_error(err: &anyhow::Error) -> RelayFailureReason {
    err.downcast_ref::<RelayHandshakeError>()
        .map(|e| failure_reason_for(&e.code))
        .unwrap_or(RelayFailureReason::Unreachable)
}
