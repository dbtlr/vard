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

    /// Take a manual snapshot now: sweep the watched directory and commit its
    /// current state into version control.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Take a manual snapshot now.

Sweeps a watched directory and commits its current state into version control, \
the same operation the daemon performs automatically — just on demand. With no \
selector every configured watch is snapshotted; with a `<name|path>` only that \
one is. Paused watches are snapshotted too: a manual snapshot is an explicit \
request, so pausing (which only stops the daemon's automatic snapshots) does \
not block it.

If the vard daemon is already running it owns the repositories, so the snapshot \
is handed to it as a request and taken asynchronously; the command reports that \
the request was queued, not the commit result. With no daemon running the \
snapshot is taken in-process under the single-instance lock, and the new \
commit (or `no changes`) is reported per watch.

A repository that is not in a safe state — mid-merge, mid-rebase, on the wrong \
branch, or with a detached HEAD — is skipped with an explanation and the \
command exits 1 (attention), never committing into an in-progress operation. A \
running daemon defers its manual snapshots the same way, so finish the \
merge/rebase (or leave the wrong branch) and re-run.

A watch that is paused is not snapshotted by the daemon: when a daemon is \
running, requesting a snapshot of a paused watch exits 1 rather than silently \
queuing work the daemon will drop — resume it, or stop the daemon to snapshot \
in-process (an in-process manual snapshot of a paused watch is still allowed, \
as explicit intent).

`-m` prepends a message paragraph to the generated snapshot subject.")]
    Snapshot(SnapshotArgs),

    /// Show a watch's snapshot history, most recent first.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Show a watch's snapshot history, most recent first.

Read-only: reads the watch's version-control log directly and never takes a \
lock or mutates anything, so it is safe to run against a watch the daemon is \
actively snapshotting. Output follows the global `--format`: human-readable \
record blocks on a terminal (each snapshot's id, time, subject, and trigger), \
and JSON or JSONL when piped.

`--since` keeps only snapshots at or after a point in the past, given as a \
humane duration counted back from now — `2h`, `3d`, `1h30m`.")]
    Log(LogArgs),

    /// Show a raw unified diff for a watch: working tree against a snapshot.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Show a raw unified diff for a watch.

Read-only. With no reference the diff is the watched directory's working tree \
against its last snapshot (`HEAD`) — the uncommitted changes a snapshot would \
capture. Given a `<ref>` (a snapshot id, branch, tag, or any revision git \
understands), the diff runs from that reference to the current working tree, \
showing everything that changed since it.

The output is a raw unified diff and nothing else: on a terminal it is paged, \
and piped it passes through untouched so it feeds `patch`, `git apply`, or a \
file directly. Because a unified diff is inherently a text artifact, `diff` is \
text-only: an explicit `--format json` or `--format jsonl` is rejected. The \
piped default still yields plain diff text, so `vard diff notes > changes.patch` \
works as expected.")]
    Diff(DiffArgs),

    /// Restore a watch's tree (or one file) to a prior snapshot, protecting the
    /// current state first.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Restore a watch's working tree, or a single file within it, to a prior state.

Before touching the tree, vard ALWAYS takes a protective snapshot of the \
current state, so a restore can never destroy uncommitted work — the state you \
are about to overwrite is committed to history first and can be recovered from \
the log.

Choose the point to restore from with exactly one of:

  --ref <sha>   a snapshot id (or any revision git understands)
  --at <when>   the snapshot current as of a past time — a duration counted
                back from now (`2h`, `3d`), or an absolute UTC date/time
                `YYYY-MM-DDThh:mm` (the `T` needs no shell quoting). A bare
                `YYYY-MM-DD` means the END of that day (state as of that day);
                the space form `YYYY-MM-DD hh:mm` also works but must be quoted.
                Natural-language forms like `yesterday 3pm` are deliberately NOT
                supported and are rejected with this list.

`--file <subpath>` restores just that one path (relative to the watch root) \
instead of the whole tree. `--dry-run` previews the differences a restore \
would overwrite, via a diff, without changing anything (and without taking the \
protective snapshot, since nothing is modified). A whole-tree dry-run excludes \
files added after the chosen point, which a restore keeps rather than removes.

If the daemon is running it keeps ownership of the repository; the restore \
still proceeds (git's own index lock serializes it against the daemon), and the \
daemon will snapshot the restored state afterward — that is by design. In that \
daemon-running case the restore is NOT journaled (only the lock holder journals), \
so if this command crashes mid-restore a leftover git lock may need clearing by \
hand — a tracked doctor-tool follow-up. Restoring a path that does not exist at \
the chosen reference reports a friendly error naming the path and the reference.")]
    Restore(RestoreArgs),

    /// Print one line per watch that needs attention, for a shell prompt or
    /// status bar. Silent and exit 0 when everything is healthy.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Print a short health summary, designed to be wired into a shell prompt, tmux \
status line, or starship module.

`notify` is built for speed above all else: it opens a small health file the \
daemon keeps up to date, reads a few bytes, and exits. It never talks to the \
daemon and never runs git, so it is safe to call on every shell prompt. When \
every watch is healthy it prints nothing and exits 0.

When something needs attention it prints one line per problem and exits 1 — a \
blocked (unsafe-repo), snapshots-failing, conflicted, sync-erroring, or \
attention-needing watch, each with how long it has been in that state. A watch \
you deliberately paused is not reported here (that is not a problem); `vard \
status` lists paused watches. If the daemon is not running that is itself one \
reported line (it replaces any stale per-watch entries), and while it is \
starting or stopping notify says so rather than reporting a false all-clear — \
so a prompt hook can tell \"all quiet\" from \"nothing is watching your files\".

Exit codes make it scriptable: 0 healthy, 1 problems (including a stopped, \
starting, or stale daemon), 2 an operational error. Output follows the global \
`--format`: human-readable lines by default, or a stable JSON/JSONL array of \
problem objects (an empty array when healthy) for a status-bar program to \
consume.

Wire it into a prompt by running it before each prompt and showing its output; \
because it exits non-zero on trouble, a prompt can also branch on the status \
without parsing the text. The warning glyph is colored only when color is \
enabled: `--color auto` (the default) disables color when its output is \
captured — which a prompt substitution always does — so pass `--color always` \
(or set CLICOLOR_FORCE=1) to keep the glyph colored in a prompt. Set VARD_ASCII \
(or use a non-UTF-8 locale) for an ASCII fallback glyph instead of the Unicode \
warning sign.")]
    Notify,

    /// Show the daemon's liveness and every watch's current state, read-only.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Show whether the vard daemon is running and what state each watch is in.

Read-only and safe to run any time: it probes the single-instance lock to learn \
whether a daemon is running, reads the small health file the daemon keeps, and \
reads the config's watch list — it never takes a lock, runs git, or mutates \
anything. With a `<name|path>` it narrows to one watch; with no selector it \
reports every configured watch.

The first line reports the daemon: running, not running, starting or stopping, \
or — when a running daemon's health file has gone stale — wedged. Each watch \
then shows one state: `ok`, `paused` (a pause you chose, which `notify` stays \
silent about), or a health-vocabulary problem — `blocked`, `snapshots-failing`, \
`attention`, `conflicted`, or `sync-error` — with how long it has been in it. \
Unlike `notify`, `status` lists healthy and paused watches too, so it is the \
on-demand review to `notify`'s always-on prompt hook.

Exit codes make it scriptable: 0 when the daemon is running and every reported \
watch is healthy, 1 when something needs attention (the daemon is not running, \
starting, or wedged, or a reported watch has a problem), 2 on an operational \
error. With a selector the per-watch part reflects only that watch, but \
daemon-level trouble always folds in. Output follows the global `--format`: \
human lines by default, or a stable JSON/JSONL array (the daemon carries a null \
watch name, each configured watch its own object) when piped.")]
    Status(StatusArgs),

    /// Read and edit vard's configuration: get, set, unset, edit, path.
    #[command(disable_help_flag = true)]
    #[command(disable_help_subcommand = true)]
    #[command(long_about = "\
Read and edit vard's TOML configuration file.

These commands address scalar keys in the `[daemon]`, `[defaults]`, `[ai]`, and \
`[update]` sections by their dotted names (`daemon.log_level`, \
`defaults.interval`, `ai.model`). Edits preserve your comments and formatting \
and are written atomically, so the running daemon — which watches the file — \
reloads a clean, whole config every time.

  get    print a key's value (exit 1 when the key is not set)
  set    set a key to a value, rejecting an edit that would break the config
  unset  remove a key
  edit   open the config in $EDITOR and validate the result
  path   print the config file's path

The set of watched directories is NOT edited here — a `watch.*` key is refused \
with a pointer to the `vard watch` verbs (`add`, `remove`, `pause`, `resume`), \
which understand watch identity. The top-level `version` is managed by vard and \
is not settable either. Every write is validated before it lands: an edit that \
would turn a valid config invalid is refused, so the CLI can never wedge the \
daemon's reloads (a config already invalid on disk may still be repaired).")]
    Config {
        /// The chosen config subcommand. Absent (a bare `vard config`) prints
        /// this command's short help.
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
}

/// Arguments to `vard snapshot`.
#[derive(Debug, Args)]
pub struct SnapshotArgs {
    /// The watch to snapshot, by name or by path. Omit to snapshot every
    /// configured watch.
    #[arg(value_name = "NAME|PATH")]
    pub target: Option<String>,

    /// A message paragraph prepended to the generated snapshot subject.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    pub message: Option<String>,
}

/// Arguments to `vard log`.
#[derive(Debug, Args)]
pub struct LogArgs {
    /// The watch whose history to show, by name or by path.
    #[arg(value_name = "NAME|PATH")]
    pub target: String,

    /// Keep only snapshots at or after this far in the past, e.g. 2h or 3d.
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,
}

/// Arguments to `vard diff`.
#[derive(Debug, Args)]
pub struct DiffArgs {
    /// The watch to diff, by name or by path.
    #[arg(value_name = "NAME|PATH")]
    pub target: String,

    /// The reference to diff from (a snapshot id, branch, or tag). Defaults to
    /// HEAD, the last snapshot.
    #[arg(value_name = "REF")]
    pub reference: Option<String>,
}

/// Arguments to `vard restore`.
#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// The watch to restore, by name or by path.
    #[arg(value_name = "NAME|PATH")]
    pub target: String,

    /// Restore from this revision (a snapshot id, branch, or tag).
    #[arg(long = "ref", value_name = "SHA", conflicts_with = "at")]
    pub reference: Option<String>,

    /// Restore the snapshot current as of a past time: a duration back from now
    /// (2h, 3d), or an absolute UTC date/time YYYY-MM-DDThh:mm (a bare
    /// YYYY-MM-DD means end of that day; the space form needs quoting).
    #[arg(long, value_name = "WHEN")]
    pub at: Option<String>,

    /// Restore only this path (relative to the watch root) instead of the whole
    /// tree.
    #[arg(long, value_name = "SUBPATH")]
    pub file: Option<PathBuf>,

    /// Preview the differences a restore would overwrite, without changing the
    /// tree or taking a protective snapshot.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments to `vard status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// The watch to report, by name or by path. Omit to report every watch.
    #[arg(value_name = "NAME|PATH")]
    pub target: Option<String>,
}

/// The `vard config` subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print a config key's value.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Print the value of a config key.

The key is a dotted name in the `[daemon]`, `[defaults]`, `[ai]`, or `[update]` \
section (`daemon.log_level`, `defaults.interval`). Only what the file actually \
sets is printed — an inherited default is not materialized here — so a key the \
config does not set prints nothing and exits 1, the way `git config` reports an \
unset key. In records/human output the bare value is printed for easy scripting; \
`--format json` wraps it as a `{key, value}` object.")]
    Get(ConfigGetArgs),

    /// Set a config key to a value.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Set a config key to a value.

The key is a dotted name in the `[daemon]`, `[defaults]`, `[ai]`, or `[update]` \
section. The value's type is inferred (`true`/`false` a boolean, a bare integer \
a number, otherwise a string) and then validated: the edit is applied to a \
comment-preserving copy of the file and the result must still parse as a valid \
config. An edit that would turn a valid config invalid is refused (exit 2) — for \
example a non-integer `daemon.log_retention_days`. A `watch.*` key is refused \
with a pointer to `vard watch`; `version` is not settable.")]
    Set(ConfigSetArgs),

    /// Remove a config key.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Remove a config key.

The key is a dotted name in the `[daemon]`, `[defaults]`, `[ai]`, or `[update]` \
section. Removing a key restores its inherited default. Removing a key that is \
not set is reported and exits 2. As with `set`, the result is validated before \
it lands and a `watch.*` key is refused with a pointer to `vard watch`.")]
    Unset(ConfigKeyArgs),

    /// Open the config file in $EDITOR and validate the result.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Open the config file in your editor and validate what you save.

The file is copied to a temporary file, `$EDITOR` (falling back to `$VISUAL`) is \
launched on it, and the result is validated before it replaces the config — \
written atomically under the config lock so the running daemon never sees a \
half-written file. If the edit would turn a valid config invalid, it is refused: \
the parse error and the temporary file's path are printed (so your work is not \
lost) and the command exits 2. The daemon reloads the change on its own; no \
signal is needed.")]
    Edit,

    /// Print the path to the config file.
    #[command(disable_help_flag = true)]
    #[command(long_about = "\
Print the path to vard's config file.

Resolves the same `$XDG_CONFIG_HOME/vard/config.toml` location the daemon and \
the other commands use, whether or not the file exists yet, so it can seed a \
script or an editor invocation.")]
    Path,
}

/// Arguments to `vard config get`.
#[derive(Debug, Args)]
pub struct ConfigGetArgs {
    /// The dotted config key to read, e.g. `daemon.log_level`.
    #[arg(value_name = "KEY")]
    pub key: String,
}

/// Arguments to `vard config set`.
#[derive(Debug, Args)]
pub struct ConfigSetArgs {
    /// The dotted config key to set, e.g. `defaults.interval`.
    #[arg(value_name = "KEY")]
    pub key: String,

    /// The value to set. Typed by inference: `true`/`false`, a bare integer, or
    /// otherwise a string.
    #[arg(value_name = "VALUE")]
    pub value: String,
}

/// Arguments to `vard config unset`.
#[derive(Debug, Args)]
pub struct ConfigKeyArgs {
    /// The dotted config key to remove, e.g. `ai.model`.
    #[arg(value_name = "KEY")]
    pub key: String,
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
