//! Command-line surface for `vard`.
//!
//! Deliberately self-contained: no `use crate::…` imports, so `build.rs` can
//! include this file verbatim via `#[path = "src/cli.rs"]` and generate shell
//! completions and the manpage from the real `clap` definitions. Any
//! dependency added here must also resolve inside the build script.
//!
//! The surface is intentionally minimal — the top-level command only.
//! Subcommands land in later tasks (VRD-15/16/17); this skeleton exists so
//! completion and manpage generation track real definitions from day one.

use clap::Parser;

/// Top-level `vard` command. `name`/`version`/`about` are the single source of
/// truth for the binary's identity, its `--version` string, and the manpage.
#[derive(Debug, Parser)]
#[command(name = "vard", version, about = env!("CARGO_PKG_DESCRIPTION"))]
pub struct Cli {}
