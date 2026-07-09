//! Command-line surface for `vard`.
//!
//! Deliberately self-contained: no `use crate::…` imports, so `build.rs` can
//! include this file verbatim via `#[path = "src/cli.rs"]` and generate shell
//! completions and the manpage from the real `clap` definitions. Any
//! dependency added here must also resolve inside the build script.
//!
//! The surface is intentionally minimal — the daemon entry point and little
//! else. The remaining subcommands land in later tasks (VRD-15/16/17); this
//! skeleton exists so completion and manpage generation track real definitions
//! from day one.

use clap::{Parser, Subcommand};

/// Top-level `vard` command. `name`/`version`/`about` are the single source of
/// truth for the binary's identity, its `--version` string, and the manpage.
#[derive(Debug, Parser)]
#[command(name = "vard", version, about = env!("CARGO_PKG_DESCRIPTION"))]
pub struct Cli {
    /// The chosen subcommand, if any. Absent (a bare `vard`) prints help.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The `vard` subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the vard daemon in the foreground: watch every configured directory
    /// and snapshot it into version control until stopped.
    ///
    /// Holds the single-instance lock for the state directory, so only one
    /// daemon runs at a time. Reloads on SIGHUP or a change to the config file,
    /// and shuts down cleanly on SIGINT or SIGTERM.
    Run,
}
