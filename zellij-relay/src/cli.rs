//! Command-line interface for the `zellij-relay` binary.
//!
//! The default (subcommand-less) invocation is `Serve`, preserving Phase-1
//! behaviour. Phase-6 Session C adds token-management subcommands.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "zellij-relay",
    about = "Relay server for Zellij remote session sharing",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the relay HTTP/WS server (default).
    Serve,
    /// Mint a new tunnel-auth token. The plaintext value is printed to stdout
    /// exactly once — store it immediately.
    CreateToken {
        /// Optional human-readable label (must be unique).
        label: Option<String>,
    },
    /// Revoke a tunnel-auth token by label or by raw token value.
    RevokeToken { label_or_token: String },
    /// List stored tunnel-auth tokens (metadata only; hashes are never printed).
    ListTokens,
}

impl Cli {
    pub fn resolved_command(self) -> Command {
        self.command.unwrap_or(Command::Serve)
    }
}
