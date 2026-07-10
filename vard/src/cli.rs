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

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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

    /// Manage the set of watched directories: add, remove, list, pause, resume.
    //
    // The teaching prose lives in `long_about`. A bare `vard watch` (no
    // subcommand) prints this command's short help, mirroring a bare `vard`, so
    // the nested subcommand is `Option`al rather than required.
    #[command(disable_help_flag = true)]
    #[command(disable_help_subcommand = true)]
    #[command(long_about = "\
Manage the set of directories vard watches.

Each watch is one directory tracked as its own git repository. These commands \
edit the config file in place, preserving your comments and formatting; the \
running daemon reloads the change automatically, so edits take effect without a \
restart. A watch is keyed by its canonicalized path and its stable name — \
selectors accept either.

  add     register a directory (offering `git init` when it is not yet a repo)
  remove  unregister a watch (never touching the repository or its history)
  list    show every watch and its settings
  pause   stop snapshotting a watch without unregistering it
  resume  resume a paused watch")]
    Watch {
        /// The chosen watch subcommand. Absent (a bare `vard watch`) prints
        /// this command's short help.
        #[command(subcommand)]
        command: Option<WatchCommand>,
    },
}

/// The `vard watch` subcommands.
#[derive(Debug, Subcommand)]
pub enum WatchCommand {
    /// Register a directory as a watch, seeding its git excludes.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Register a directory as a watch.

The directory must be a git repository. If it is not, `vard watch add` offers to \
run `git init` for you: on a terminal it prompts; non-interactively it declines \
unless `--init` is passed. The watch is recorded by its canonicalized path (with \
symlinks resolved) plus a stable name — `--name`, or the directory's own name by \
default.

Adding also seeds the repository's private `.git/info/exclude` (never your \
tracked `.gitignore`) with vard's default excludes: dependency and build \
directories, OS cruft, and well-known secret shapes such as `.env`, `*.pem`, and \
`id_rsa*`. The write is idempotent — re-adding never duplicates lines and leaves \
your own exclude entries untouched.

Re-adding an existing name at a new path relinks that watch to the new location, \
keeping its metadata — the recovery path for a directory that moved.")]
    Add(WatchAddArgs),

    /// Unregister a watch, leaving its repository untouched.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Unregister a watch.

This removes the watch from the config file only. It never touches the \
repository, its working tree, or its history — the directory and every snapshot \
vard ever took remain exactly as they were. The watch may be named by its stable \
name or by its path.

By default vard's own metadata for the watch (its operation journal and other \
per-watch state) is left in place, so re-adding the same name later resumes \
cleanly. Pass `--purge` to drop that metadata as well.")]
    Remove(WatchRemoveArgs),

    /// List every watch and its settings.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
List every registered watch.

Output follows the global `--format`: human-readable records on a terminal, and \
JSON (or JSONL) when piped, so scripts get a stable machine contract. Each watch \
reports its name, path, branch and remote, trigger and interval, whether it \
syncs, and whether it is paused.")]
    List,

    /// Pause a watch without unregistering it.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Pause a watch.

A paused watch stays registered and keeps all of its metadata, but the daemon \
stops snapshotting it until it is resumed. The pause is recorded as `paused = \
true` in the config file, so it survives a daemon restart and the running daemon \
applies it on its next reload. The watch may be named by its stable name or by \
its path.")]
    Pause(WatchSelectArgs),

    /// Resume a paused watch.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Resume a paused watch.

Clears the watch's paused flag so the daemon resumes snapshotting it on its next \
reload. Resuming a watch that is not paused is a no-op. The watch may be named by \
its stable name or by its path.")]
    Resume(WatchSelectArgs),
}

/// Arguments to `vard watch add`.
#[derive(Debug, Args)]
pub struct WatchAddArgs {
    /// The directory to watch. Registered by its canonicalized path.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Stable name for the watch. Defaults to the directory's own name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Remote the watch pushes to and pulls from (default: origin).
    #[arg(long, value_name = "REMOTE")]
    pub remote: Option<String>,

    /// Branch the watch commits to (default: the repository's current branch).
    #[arg(long, value_name = "BRANCH")]
    pub branch: Option<String>,

    /// Which automatic triggers arm snapshots.
    #[arg(long, value_enum, value_name = "MODE")]
    pub trigger: Option<TriggerArg>,

    /// Interval between periodic snapshots, e.g. 15m or 1h30m.
    #[arg(long, value_name = "DURATION")]
    pub interval: Option<String>,

    /// How long file activity must settle before a snapshot, e.g. 10s.
    #[arg(long, value_name = "DURATION")]
    pub quiesce: Option<String>,

    /// Register the watch as local-only: never sync to a remote.
    #[arg(long = "no-sync")]
    pub no_sync: bool,

    /// If the directory is not a git repository, initialize one without
    /// prompting. The script-friendly escape hatch for non-interactive use.
    #[arg(long)]
    pub init: bool,
}

/// Arguments to `vard watch remove`.
#[derive(Debug, Args)]
pub struct WatchRemoveArgs {
    /// The watch to remove, by name or by path.
    #[arg(value_name = "NAME|PATH")]
    pub target: String,

    /// Also drop vard's own metadata for the watch (its journal and per-watch
    /// state). Never touches the repository.
    #[arg(long)]
    pub purge: bool,
}

/// Arguments to `vard watch pause` and `vard watch resume`.
#[derive(Debug, Args)]
pub struct WatchSelectArgs {
    /// The watch to act on, by name or by path.
    #[arg(value_name = "NAME|PATH")]
    pub target: String,
}

/// Which automatic snapshot triggers a watch arms. The CLI mirror of
/// `vard_core::TriggerMode`; kept here so `cli.rs` stays dependency-free for the
/// `build.rs` include (the conversion lives in [`crate::watch`]).
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerArg {
    /// Snapshot only in response to filesystem changes.
    Events,
    /// Snapshot only when the periodic interval elapses.
    Interval,
    /// Arm both change and interval triggers.
    Both,
}

impl TriggerArg {
    /// The canonical config/spelling of this mode (`events`, `interval`,
    /// `both`) — the string written to config and parsed by `vard_core`.
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerArg::Events => "events",
            TriggerArg::Interval => "interval",
            TriggerArg::Both => "both",
        }
    }
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
