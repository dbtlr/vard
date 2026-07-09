//! Command-line surface for `vard`.
//!
//! Deliberately self-contained: no `use crate::…` imports, so `build.rs` can
//! include this file verbatim via `#[path = "src/cli.rs"]` and generate shell
//! completions and the manpage from the real `clap` definitions. Any
//! dependency added here must also resolve inside the build script.
//!
//! clap remains the argument parser and the single source of arg metadata, but
//! it never emits help: `-h`/`--help` are intercepted before parsing and
//! rendered by the [`crate::help`] module (CLI Help Output v2). The `-h` /
//! `--help` flags below exist only so clap's parser, completions, and the
//! manpage know about them; the interceptor acts on them first.
//!
//! The surface is intentionally minimal — the daemon entry point and little
//! else. The remaining subcommands land in later tasks (VRD-15/16/17); this
//! skeleton exists so completion and manpage generation track real definitions
//! from day one.

use clap::{Parser, Subcommand, ValueEnum};

// Top-level `vard` command. `name`/`version`/`about` are the single source of
// truth for the binary's identity, its `--version` string, and the manpage.
//
// This uses a plain comment, not a `///` doc comment: clap derives `long_about`
// from a struct doc comment, which would leak these implementation notes into
// `vard --help`. The user-facing description comes from the `about` attribute.
//
// Help is disabled at the clap level (`disable_help_flag` /
// `disable_help_subcommand`) and re-implemented as the global `-h`/`--help`
// flags below, which the `crate::help` interceptor renders before clap ever
// parses. This keeps required positionals on future subcommands from erroring
// before help can be shown.
#[derive(Debug, Parser)]
#[command(name = "vard", version, about = env!("CARGO_PKG_DESCRIPTION"))]
#[command(disable_help_flag = true)]
#[command(disable_help_subcommand = true)]
pub struct Cli {
    /// When to colorize output. Auto enables color on a TTY and disables it
    /// when piped; `NO_COLOR` and `CLICOLOR_FORCE` always win.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        value_name = "WHEN",
        help_heading = "Global options",
        help = "Color output: auto, always, or never. Honors NO_COLOR / CLICOLOR_FORCE"
    )]
    pub color: ColorWhen,

    /// Output shape. Absent means auto-detect by destination: records on a
    /// TTY, JSON when piped. The read/list commands (VRD-15+) consume this;
    /// `vard run` produces no records output and ignores it.
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "FORMAT",
        help_heading = "Global options",
        help = "Output shape: records, json, or jsonl. Defaults to records on a TTY, json when piped"
    )]
    pub format: Option<OutputFormat>,

    /// Print short help. Intercepted before parsing (see module docs).
    #[arg(
        short = 'h',
        global = true,
        action = clap::ArgAction::SetTrue,
        help_heading = "Global options",
        help = "Print short help. Use --help for the full reference"
    )]
    pub help_short: bool,

    /// Print full help. Intercepted before parsing (see module docs).
    #[arg(
        long = "help",
        global = true,
        action = clap::ArgAction::SetTrue,
        help_heading = "Global options",
        help = "Print the full reference. Use -h for a short summary"
    )]
    pub help_long: bool,

    /// The chosen subcommand, if any. Absent (a bare `vard`) prints short help.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The `vard` subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the vard daemon in the foreground: watch every configured directory
    /// and snapshot it into version control until stopped.
    //
    // The lifecycle prose lives in `long_about` below — the single authoritative
    // source. A doc-comment paragraph here would be discarded by clap (an
    // explicit `long_about` wins), so only the first line (the `about`) is a
    // `///` comment.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Run the vard daemon in the foreground until stopped.

The daemon acquires the single-instance lock for its state directory (so only \
one vard owns a directory tree at a time), loads the file config into watch \
specs, recovers any stale version-control locks left by a previous crash, then \
watches every configured directory and snapshots changes into version control. \
A second daemon contending for the same state directory exits with status 2.

It stays attached to the terminal and logs each event to stderr. It reloads on \
SIGHUP or when the config file changes on disk, rebuilds a watch whose event \
source dies (with exponential backoff), and shuts down cleanly on SIGINT or \
SIGTERM.")]
    Run,
}

/// When to colorize output. Resolved against TTY detection plus the `NO_COLOR`
/// and `CLICOLOR_FORCE` environment variables by [`crate::output::palette`].
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorWhen {
    /// Force color on, regardless of destination (still yields to `NO_COLOR`).
    Always,
    /// Color on a TTY, off when piped (the default).
    Auto,
    /// Never colorize.
    Never,
}

/// The shape of a command's stdout. The settled replacement for a per-command
/// `--json` flag: one global `--format` covering the human and machine forms.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable record blocks (the TTY default).
    Records,
    /// A single JSON document (the piped default).
    Json,
    /// Newline-delimited JSON, one object per line.
    Jsonl,
}
