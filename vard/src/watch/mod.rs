//! The `vard watch` command set: add, remove, list, set, pause, resume, sync.
//!
//! These are the first commands that *mutate* vard's configuration. They edit
//! `config.toml` in place through the comment-preserving
//! [`config_edit`] layer and commit each change atomically,
//! so the running daemon — which watches the file — reloads a clean, whole
//! config every time.
//!
//! # Identity (spec §12)
//!
//! A watch is keyed by its canonicalized path (symlinks resolved) and its
//! stable name. `add` stores the canonical path; the `<name|path>` selector on
//! `remove`/`pause`/`resume` resolves either (see [`select`]). Re-adding an
//! existing name at a new path relinks that watch — the moved-directory
//! recovery.
//!
//! # Output
//!
//! `list` renders through the global `--format`: human records on a TTY, JSON /
//! JSONL when piped. The mutating commands print a one-line confirmation in the
//! records form and a single result object in the machine forms, so a script can
//! consume `add`/`remove`/`pause`/`resume` as readily as `list`.

mod excludes;
// `pub(crate)` so the shared `<name|path>` identity/selector logic is reachable
// from future top-level commands (VRD-16/17) as `crate::watch::select`.
pub(crate) mod select;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use vard_core::{GitBackend, TriggerMode, VcsBackend, WatchSpec};

use crate::cli::{
    ColorWhen, OutputFormat, SyncArgs, WatchAddArgs, WatchCommand, WatchRemoveArgs, WatchSetArgs,
    WatchSyncArgs,
};
use crate::command::{
    CmdError, CmdResult, OutCtx, emit_action, emit_records, finish, finish_write,
};
use crate::config::{Config, ResolvedWatch, WatchConfig, expand_tilde};
use crate::config_edit::{self, ConfigLock, WatchEntry};
use crate::instance::{CliLock, InstanceLock};
use crate::journal::{self, Journal, OpLock};
use crate::output::record::{self, Record, RecordField};
use crate::paths::{self, HomeNotFound};

/// The filesystem locations the watch commands read and write, resolved once.
/// Injected in tests so nothing touches the real HOME.
struct WatchPaths {
    /// `config.toml`, mutated in place.
    config_file: PathBuf,
    /// Per-watch operation journals, drained on `remove` and dropped by
    /// `remove --purge`.
    journal_dir: PathBuf,
    /// The single-instance lock, taken while a no-daemon `remove` drains a
    /// watch's journal (the journal's single-writer invariant).
    lock_file: PathBuf,
}

impl WatchPaths {
    fn from_xdg() -> Result<WatchPaths, HomeNotFound> {
        Ok(WatchPaths {
            config_file: paths::config_file()?,
            journal_dir: paths::journal_dir()?,
            lock_file: paths::lock_file()?,
        })
    }
}

/// Production entry point for `vard watch <subcommand>`. Resolves paths and
/// output settings, dispatches, and maps the result to an exit code — all via
/// the shared [command-outcome layer](crate::command).
pub(crate) fn run(cmd: WatchCommand, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    let paths = match WatchPaths::from_xdg() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("vard: {err}");
            return ExitCode::from(2);
        }
    };

    let out = OutCtx::resolve(color, format);

    let result = match cmd {
        WatchCommand::Add(args) => cmd_add(&paths, &out, args),
        WatchCommand::Remove(args) => cmd_remove(&paths, &out, args),
        WatchCommand::List => cmd_list(&paths, &out),
        WatchCommand::Set(args) => cmd_set(&paths, &out, args),
        WatchCommand::Pause(args) => cmd_set_paused(&paths, &out, &args.target, true),
        WatchCommand::Resume(args) => cmd_set_paused(&paths, &out, &args.target, false),
        WatchCommand::Sync(args) => cmd_sync(&paths, &out, &args, color, format),
    };

    finish(result)
}

// --- add -------------------------------------------------------------------

/// How a `watch add` maps onto the existing config.
enum Registration {
    /// Append a new `[[watch]]`.
    Append,
    /// Relink an existing watch (matched by name) to the new path — the re-add
    /// / moved-directory path. The document is relocated by name at mutation
    /// time, so no index is carried across the two parses.
    Relink,
}

/// The repository decision resolved *before* the config lock is taken, so an
/// interactive `git init` prompt (a human wait) never spans the blocking flock
/// and wedges concurrent `vard watch` writers.
enum RepoPlan {
    /// The path is already a git repository; its backend is carried forward.
    Existing(GitBackend),
    /// The path is not a repository, but an init was authorized (`--init`, or an
    /// interactive "yes"). The init itself runs later, under the lock.
    Init,
}

fn cmd_add(paths: &WatchPaths, out: &OutCtx, args: WatchAddArgs) -> CmdResult {
    // Canonicalize the path (which requires it to exist); this is the watch's
    // identity. A non-existent directory is a clear, early error.
    let canonical = std::fs::canonicalize(&args.path)
        .map_err(|e| CmdError::err(format!("{}: {e}", args.path.display())))?;
    if !canonical.is_dir() {
        return Err(CmdError::err(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }

    // The config is UTF-8 (TOML), so a non-UTF-8 path cannot be stored without
    // a lossy corruption that could never be matched again. Reject it honestly.
    let path_str = canonical
        .to_str()
        .ok_or_else(|| {
            CmdError::err(format!(
                "{} is not valid UTF-8; vard's config is UTF-8 and cannot store this path",
                canonical.display()
            ))
        })?
        .to_string();

    // Name: explicit, or the directory's own final component.
    let name = match &args.name {
        Some(n) => n.clone(),
        None => canonical
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                CmdError::err(format!(
                    "cannot derive a watch name from {}; pass --name",
                    canonical.display()
                ))
            })?,
    };

    // Validate everything vard-core owns (name charset, durations, trigger)
    // before touching the filesystem or config, so a bad flag fails cleanly.
    let interval = opt_duration(args.interval.as_deref())?;
    let quiesce = opt_duration(args.quiesce.as_deref())?;
    let sync_interval = opt_duration(args.sync_interval.as_deref())?;
    let trigger = args.trigger.map(|t| t.as_str());
    // The explicit `sync` pin the entry will carry (see [`add_sync_pin`]).
    let sync_pin = add_sync_pin(args.sync, args.no_sync);
    validate_watch(
        &name,
        &canonical,
        trigger,
        interval,
        quiesce,
        sync_interval,
        sync_pin,
        args.remote.as_deref(),
        args.branch.as_deref(),
    )?;

    // Resolve the repository decision — including any interactive `git init`
    // prompt — *before* acquiring the lock, so a human wait never blocks other
    // `vard watch` writers. The init itself is deferred until after the
    // under-lock conflict re-check.
    let repo_plan = plan_repo(&canonical, args.init)?;

    // Serialize the whole read→plan→mutate→write cycle against concurrent
    // `vard watch` writers, so lost updates and stale relocations cannot race.
    let config_lock = ConfigLock::acquire(&paths.config_file)?;

    // Decide append-vs-relink against the current config, and load the editable
    // document, *before* any git init or exclude side effects — so a rejected
    // add (a name/path conflict, or a config the editor cannot safely mutate)
    // leaves no git init or exclude block behind.
    let config = load_config(&paths.config_file)?;
    let registration = plan_registration(config.as_ref(), &name, &canonical)?;
    let relinked = matches!(registration, Registration::Relink);
    // Load the editable document plus the exact on-disk text: the config's
    // validity *before* this edit decides how strictly the result is judged (see
    // [`commit_document`]); a missing file is a valid empty baseline.
    let (mut doc, pre_edit) = match config_edit::load_document_with_text(&paths.config_file)? {
        Some((doc, text)) => (doc, Some(text)),
        None => (config_edit::new_document(), None),
    };

    // Realize the repository plan under the lock: perform any authorized init,
    // then seed vard's default excludes into the repo's resolved exclude file
    // (which is correct even for a worktree or submodule, where `.git` is a
    // file).
    let initialized = init_and_seed_excludes(&canonical, repo_plan, args.branch.as_deref())?;

    let entry = WatchEntry {
        name: name.clone(),
        path: path_str,
        branch: args.branch.clone(),
        remote: args.remote.clone(),
        trigger: trigger.map(str::to_string),
        interval: args.interval.clone(),
        quiesce: args.quiesce.clone(),
        sync_interval: args.sync_interval.clone(),
        sync: sync_pin,
    };

    match registration {
        Registration::Append => config_edit::append_watch(&mut doc, &entry),
        // Relink relocates by name inside this document; if the watch vanished
        // between planning and now, fall back to appending rather than panic.
        Registration::Relink => {
            if !config_edit::update_watch(&mut doc, &entry) {
                config_edit::append_watch(&mut doc, &entry);
            }
        }
    }
    // Validate the exact bytes to be written before committing them, so the CLI
    // can never take a valid config to invalid and wedge the daemon's reloads.
    let warning = config_edit::commit_document(&doc, &paths.config_file, pre_edit.as_deref())?;

    // The config write is durable now, and nothing below writes the config, so
    // release the writer lock BEFORE the `--sync` confirmation cycle — otherwise
    // a no-daemon confirmation would hold the config lock across a network
    // fetch/reconcile/push, blocking every other `vard watch` writer for its
    // duration. (The `vard watch sync` verb already scopes its lock this way.)
    drop(config_lock);

    let verb = if relinked { "relinked" } else { "added" };
    let human = format!("{verb} watch {name} → {}", canonical.display());
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", &name),
            RecordField::str("path", canonical.to_string_lossy()),
            RecordField::bool("initialized", initialized),
            RecordField::bool("relinked", relinked),
        ],
    };
    // `--sync` reports the add result and its confirmation cycle together (folded
    // into one document in the JSON form); the plain path emits the add result
    // now, surfaces any still-invalid warning, and — when syncing stays off —
    // prints the opt-in hint.
    if args.sync {
        return confirm_add_sync(paths, out, human, record, warning, &name);
    }

    emit_action(out, &human, &record)?;
    // A write that landed but left the config still-invalid carries an attention
    // warning; surface it now.
    if let Some(w) = warning {
        return Err(w);
    }

    // No `--sync`: when syncing resolves off for the resulting watch, print
    // exactly one hint pointing at the opt-in verb. Records form only — the
    // machine forms already carry the effective `sync` value via `watch list`.
    // An explicit `--no-sync` suppresses it (the user just chose local-only),
    // and the check reads the watch's EFFECTIVE sync after the write, so a
    // relink that preserved a `sync = true` pin — or a `defaults.sync = true` —
    // correctly suppresses it too rather than falsely claiming "syncing is off".
    if !args.no_sync
        && matches!(out.format, OutputFormat::Records)
        && resulting_watch_syncs(paths, &name) == Some(false)
    {
        let mut w = io::stdout().lock();
        let _ = writeln!(w, "syncing is off — enable with: vard watch sync {name}");
    }
    Ok(())
}

/// The explicit `sync` pin an `add` writes, from its `--sync` / `--no-sync`
/// flags: `--sync` pins `true`, `--no-sync` pins `false`, and neither leaves the
/// key unset (`None`) so the watch inherits `defaults.sync`. The two flags
/// conflict at the clap layer, so at most one is ever set here.
fn add_sync_pin(sync: bool, no_sync: bool) -> Option<bool> {
    if sync {
        Some(true)
    } else if no_sync {
        Some(false)
    } else {
        None
    }
}

/// The resulting watch's EFFECTIVE `sync` after an `add` write — its own
/// `sync` pin (an explicit `--no-sync`/`--sync`, or one preserved through a
/// relink) resolved over `defaults.sync` and the core default. `Some(false)`
/// gates the opt-in hint; `None` on any load/resolve failure suppresses the
/// hint rather than risk a misleading claim. Reads the post-write config, so a
/// relink that preserved a `sync = true` pin is seen as on.
fn resulting_watch_syncs(paths: &WatchPaths, name: &str) -> Option<bool> {
    let config = load_config(&paths.config_file).ok()??;
    let resolved = config.resolve_all().ok()?;
    resolved
        .iter()
        .find(|rw| rw.spec.name().eq_ignore_ascii_case(name))
        .map(|rw| rw.spec.sync())
}

/// Resolves the repository decision for `path` *without holding the config
/// lock*: an existing repo is carried forward, a missing one is authorized for
/// init only via `--init` or an interactive "yes", and a declined or
/// non-interactive miss is refused. Any human prompt happens here, before the
/// lock, so an unanswered prompt cannot wedge concurrent writers.
fn plan_repo(path: &Path, init_flag: bool) -> Result<RepoPlan, CmdError> {
    match GitBackend::detect(path) {
        Ok(Some(backend)) => Ok(RepoPlan::Existing(backend)),
        Ok(None) => {
            let approved = if init_flag {
                true
            } else if io::stdin().is_terminal() && io::stderr().is_terminal() {
                // Gate interactivity on the stream we actually prompt on
                // (stderr), so `2>/dev/null` yields a clean error, not a
                // silent, invisible hang waiting on input.
                prompt_init(path)?
            } else {
                return Err(CmdError::err(format!(
                    "{} is not a git repository; re-run with --init to initialize one, \
                     or run `git init` there first",
                    path.display()
                )));
            };
            if !approved {
                return Err(CmdError::attention(format!(
                    "{} is not a git repository; nothing was added",
                    path.display()
                )));
            }
            Ok(RepoPlan::Init)
        }
        Err(e) => Err(CmdError::err(format!("checking {}: {e}", path.display()))),
    }
}

/// Realizes a [`RepoPlan`] under the config lock: performs a planned `git init`
/// (idempotent, so a repo appearing between plan and now is harmless), then
/// seeds vard's default excludes into the repo's resolved exclude file. Returns
/// whether an init happened.
fn init_and_seed_excludes(
    path: &Path,
    plan: RepoPlan,
    branch: Option<&str>,
) -> Result<bool, CmdError> {
    let (initialized, backend) = match plan {
        RepoPlan::Existing(backend) => (false, backend),
        RepoPlan::Init => {
            let backend = GitBackend::init(path, branch)
                .map_err(|e| CmdError::err(format!("git init {}: {e}", path.display())))?;
            (true, backend)
        }
    };
    let exclude_path = backend
        .info_exclude_path()
        .map_err(|e| CmdError::err(format!("resolving git excludes: {e}")))?;
    excludes::ensure(&exclude_path)
        .map_err(|e| CmdError::err(format!("writing git excludes: {e}")))?;
    Ok(initialized)
}

/// Asks the user, on a terminal, whether to initialize a repository. The prompt
/// goes to stderr so stdout stays clean for machine consumers; default is no.
fn prompt_init(path: &Path) -> Result<bool, CmdError> {
    eprint!(
        "{} is not a git repository. Initialize one? [y/N] ",
        path.display()
    );
    io::stderr()
        .flush()
        .map_err(|e| CmdError::err(format!("prompting: {e}")))?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| CmdError::err(format!("reading response: {e}")))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Validates the watch against vard-core's invariants by building a
/// [`WatchSpec`]. This surfaces a bad name, duration, or trigger with the same
/// message the daemon would give, before anything is written; the built spec is
/// returned so callers (and the regression test) can inspect the resolved
/// values.
///
/// `sync` is the explicit pin to be written (`None` leaves the key unset so the
/// watch inherits the default). It is applied to the builder *only when set*, so
/// validation mirrors the effective value being written: an absent flag no
/// longer forces `sync = true` onto the validated spec (the pre-fix
/// `sync(!no_sync)` did), which never matched the config the edit actually
/// produces.
#[allow(clippy::too_many_arguments)]
fn validate_watch(
    name: &str,
    path: &Path,
    trigger: Option<&str>,
    interval: Option<Duration>,
    quiesce: Option<Duration>,
    sync_interval: Option<Duration>,
    sync: Option<bool>,
    remote: Option<&str>,
    branch: Option<&str>,
) -> Result<WatchSpec, CmdError> {
    let mut builder = WatchSpec::builder(name, path);
    if let Some(t) = trigger {
        let mode = t
            .parse::<TriggerMode>()
            .map_err(|e| CmdError::err(e.to_string()))?;
        builder = builder.trigger(mode);
    }
    if let Some(iv) = interval {
        builder = builder.interval(iv);
    }
    if let Some(q) = quiesce {
        builder = builder.quiesce(q);
    }
    if let Some(si) = sync_interval {
        builder = builder.sync_interval(si);
    }
    if let Some(s) = sync {
        builder = builder.sync(s);
    }
    if let Some(r) = remote {
        builder = builder.remote(r);
    }
    if let Some(b) = branch {
        builder = builder.branch(b);
    }
    builder
        .build()
        .map_err(|e| CmdError::err(format!("invalid watch: {e}")))
}

/// Decides whether an add appends a new watch or relinks an existing one,
/// rejecting the conflicting cases. Path identity uses the shared spec-§12 rule
/// in [`select`], the same one the `<name|path>` selectors apply.
fn plan_registration(
    config: Option<&Config>,
    name: &str,
    canonical: &Path,
) -> Result<Registration, CmdError> {
    let Some(config) = config else {
        return Ok(Registration::Append);
    };
    let home = home_dir();
    let by_name = config
        .watches
        .iter()
        .position(|w| w.name.eq_ignore_ascii_case(name));
    let by_path = config.watches.iter().position(|w| {
        // `canonical` is already canonicalized, so it is its own canonical form.
        select::config_path_identifies(&w.path, home.as_deref(), canonical, Some(canonical))
    });

    match (by_name, by_path) {
        // Same watch (re-add same name + path, or relink same name to a new
        // path): update it in place.
        (Some(n), Some(p)) if n == p => Ok(Registration::Relink),
        (Some(_), None) => Ok(Registration::Relink),
        // The name exists on one watch and the path on another — relinking would
        // collide. Refuse rather than silently clobber.
        (Some(n), Some(p)) => Err(CmdError::err(format!(
            "name {:?} belongs to one watch and {} is already watched by {:?}; \
             remove one first",
            config.watches[n].name,
            canonical.display(),
            config.watches[p].name
        ))),
        // The path is watched under a different name.
        (None, Some(p)) => Err(CmdError::err(format!(
            "{} is already watched by {:?}; remove it first or re-add under that name",
            canonical.display(),
            config.watches[p].name
        ))),
        (None, None) => Ok(Registration::Append),
    }
}

// --- remove ----------------------------------------------------------------

/// How long a no-daemon `remove` waits out a *peer CLI* lock holder before
/// giving up on the synchronous drain. Short: the drain is a best-effort safety
/// net (a running daemon's reload, or the next daemon start's orphan sweep,
/// covers what this misses), so it must never make `remove` feel slow.
///
/// This is deliberately shorter than `snapshot`/`restore`'s `CLI_LOCK_BUDGET`
/// (10s): those *must* acquire the lock to do their job, so they wait longer
/// before conceding a peer is running; `remove`'s work (the config edit) is
/// already done by the time we get here, and only the optional drain wants the
/// lock, so a short wait is right — giving up just defers the drain to the
/// daemon rather than failing anything.
const REMOVE_DRAIN_BUDGET: Duration = Duration::from_secs(3);

fn cmd_remove(paths: &WatchPaths, out: &OutCtx, args: WatchRemoveArgs) -> CmdResult {
    let _lock = ConfigLock::acquire(&paths.config_file)?;
    let config = require_config(paths, "remove")?;
    let index =
        select::select_watch(&config, &args.target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();
    let raw_path = config.watches[index].path.clone();
    let path_display = raw_path.display().to_string();
    // The repo's identity path, for keying and draining its journal. Expanded
    // exactly the way `Config::resolve_all` expands a watch path (tilde against
    // HOME, textual fallback when HOME is unset), so the CLI keys the same
    // journal the daemon does; the journal helpers canonicalize from here (a
    // single-path rule — see `journal::identity_path`). This is deliberately
    // *not* `select`'s pairwise identity rule, which we already used above to
    // find the row: selection matches a typed selector against many config
    // rows, whereas here we resolve one known row's own path.
    let repo_path = expand_tilde(&raw_path, home_dir().as_deref()).unwrap_or(raw_path);

    let (mut doc, pre_edit) = config_edit::load_document_with_text(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while removing".to_string()))?;
    if !config_edit::remove_watch(&mut doc, &name) {
        return Err(CmdError::err(format!(
            "watch {name:?} vanished from the config before it could be removed"
        )));
    }
    let warning = config_edit::commit_document(&doc, &paths.config_file, Some(&pre_edit))?;

    // Drain the watch: settle any in-flight operation and clean a stale git
    // lock we left, so a removed watch never wedges on a lock only its journal
    // could prove ours. A running daemon's reload drains it instead (we skip);
    // with no daemon we run recovery here, under the instance lock. The return
    // value is whether *this* CLI actually drained — which gates whether
    // `--purge` may delete the journal. The repository is never touched.
    let drained = drain_removed_watch(paths, &repo_path, &name);
    let purged = if args.purge {
        purge_metadata(paths, &repo_path, drained)?
    } else {
        false
    };

    let human = if !args.purge {
        format!("removed watch {name}")
    } else if purged {
        format!("removed watch {name} and purged its metadata")
    } else {
        // A daemon or peer CLI holds the lock and the journal still records an
        // open operation: we did not delete it. Say so honestly — the daemon's
        // reload-drain or the next start's orphan sweep will settle it.
        format!(
            "removed watch {name}; its operation journal is retained until the daemon settles \
             the open operation"
        )
    };
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", &name),
            RecordField::str("path", path_display),
            RecordField::bool("purged", purged),
        ],
    };
    emit_action(out, &human, &record)?;
    warning.map_or(Ok(()), Err)
}

/// Drains a just-removed watch's journal when no daemon is running: takes the
/// instance lock (the journal's single-writer invariant) and runs stale-lock
/// recovery against the repo, so a lock left by a crashed in-process operation
/// is proven ours and cleaned rather than wedging a repo the config no longer
/// mentions.
///
/// Best-effort by design, and it deliberately does nothing when it is not the
/// journal's writer:
///
/// * A **daemon** holds the lock ⇒ the reload triggered by the config write
///   drains the watch as the engine drops it; double-draining here would race
///   the daemon's own journal writes, so we skip.
/// * A **peer CLI** holds the lock past the short budget ⇒ another in-process
///   operation is mid-flight; we could not have crashed, and forcing our way in
///   would violate the single-writer invariant. Skip; the next daemon start's
///   sweep covers any residue.
///
/// A recovery hiccup is never fatal — recovery folds every outcome into a
/// report, and this is a cleanup step, not the removal itself.
///
/// Returns `true` only when *this* CLI acquired the lock **and** recovery reached
/// a settled outcome that authorizes deleting the journal
/// ([`RecoveryReport::settled`](crate::journal::RecoveryReport::settled): the
/// record was compacted clean, a stale lock removed, no lock present, or the lock
/// proven foreign). A daemon/peer holder — or a recovery that left the record
/// live (a still-alive holder, a too-fresh lock, a corrupt/failed outcome) —
/// returns `false`: `--purge` must then not delete a journal that could still
/// record an open operation, and falls back to the
/// [`is_clean`](Journal::is_clean) guard instead.
fn drain_removed_watch(paths: &WatchPaths, repo_path: &Path, name: &str) -> bool {
    match InstanceLock::acquire_for_cli(&paths.lock_file, REMOVE_DRAIN_BUDGET) {
        Ok(CliLock::Acquired(lock)) => {
            let journal = Journal::for_repo_in_dir(&paths.journal_dir, repo_path);
            // Route through the shared logger so the CLI drain reports at the
            // same per-variant levels as the daemon's recover sites. Only a
            // *settled* outcome authorizes deleting the recovery evidence.
            let settled = journal
                .recover_and_log(repo_path, name, "watch-remove")
                .settled();
            drop(lock);
            settled
        }
        // A daemon drains via its reload; a peer CLI means we are not the
        // writer. Either way, leave the journal untouched — we did not drain.
        Ok(CliLock::DaemonHeld) | Ok(CliLock::BusyPeerCli) => false,
        // Could not even attempt the lock (an I/O error resolving it): the drain
        // is best-effort, so a removal must not fail on it.
        Err(_) => false,
    }
}

/// Drops vard's per-watch metadata (its operation journal and sibling op-lock
/// file, keyed by repo path) for a removed watch, returning whether it actually
/// deleted the journal.
///
/// It deletes the journal only when doing so cannot destroy live recovery
/// evidence: either this CLI already `drained` it (took the op lock and ran
/// recovery), or the journal is provably clean (no dangling `begin`). If a
/// daemon or peer CLI holds the lock *and* the journal still records an open
/// operation, the file is left for the daemon's reload-drain or the next
/// start's orphan sweep — the journal now encodes its own repo path, so that
/// sweep can still recover it. Absent metadata is not an error — purge is
/// idempotent.
fn purge_metadata(paths: &WatchPaths, repo_path: &Path, drained: bool) -> Result<bool, CmdError> {
    let journal = Journal::for_repo_in_dir(&paths.journal_dir, repo_path);
    if !drained && !journal.is_clean() {
        return Ok(false);
    }
    // Prove the op lock free before unlinking anything. Unlinking a `.lock` file a
    // live holder still has open would let the next acquirer create a *fresh
    // inode* at the same path and flock that instead — silently breaking the
    // single-writer invariant. So try-acquire the sibling first: a `WOULDBLOCK`
    // means a holder is mid-operation, so leave the metadata for the daemon's
    // reload-drain or the next start's sweep. Holding the lock, both unlinks are
    // safe (mirrors `reconcile_orphan`'s held-lock delete; unlinking a file we
    // hold open is fine on Unix, and the guard releases on return).
    let _guard = match OpLock::try_acquire(journal.lock_path()) {
        Ok(Some(guard)) => guard,
        Ok(None) => return Ok(false),
        // Op-lock I/O trouble: best-effort, so retain rather than hard-fail the
        // removal (the journal now encodes its own repo path, so the sweep can
        // still recover it later).
        Err(e) => {
            eprintln!(
                "vard: purge: op lock for {}: {e}; metadata retained",
                repo_path.display()
            );
            return Ok(false);
        }
    };
    // Drop the sibling op-lock file too (best-effort): purged metadata must leave
    // nothing behind, and the orphan sweep only scans `.journal` files.
    let _ = std::fs::remove_file(journal.lock_path());
    match std::fs::remove_file(journal.path()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(CmdError::err(format!(
            "purging metadata for {}: {e}",
            repo_path.display()
        ))),
    }
}

// --- list ------------------------------------------------------------------

fn cmd_list(paths: &WatchPaths, out: &OutCtx) -> CmdResult {
    let Some(config) = load_config(&paths.config_file)? else {
        return emit_list(out, &[]);
    };
    // `list` is the one read-only diagnostic: it must render even when the
    // config fails full validation (a duplicate name, a bad inherited default),
    // so a broken config is exactly what you can still inspect. On success it
    // shows effective values; on a validation failure it renders leniently from
    // the raw watches and exits 1 (attention) with a warning, never 2.
    match config.resolve_all() {
        Ok(watches) => {
            // Flag canonical journal-key aliasing with the shared first-wins rule
            // the daemon uses to skip such a watch: a later collider shares one
            // journal and is therefore not supervised, so `list` must not show it
            // as an ordinary watch.
            let aliases =
                journal::alias_winners(watches.iter().map(|w| (w.spec.name(), w.spec.path())));
            let records: Vec<Record> = watches
                .iter()
                .zip(&aliases)
                .map(|(rw, alias)| watch_record(rw, alias.as_deref()))
                .collect();
            emit_list(out, &records)
        }
        Err(e) => {
            let records: Vec<Record> = config.watches.iter().map(raw_watch_record).collect();
            emit_list(out, &records)?;
            Err(CmdError::attention(format!(
                "listed watches as written, but the config is not fully valid: {e}"
            )))
        }
    }
}

/// Builds the display record for one resolved watch (effective values plus its
/// paused flag). Name is a field, not a header, so the machine forms carry it.
/// `alias_of` is the name of an earlier watch this one canonically aliases, if
/// any — such a watch is not supervised (the daemon skips it), surfaced here as
/// an `aliases` marker so `list` never shows a silent duplicate as ordinary.
fn watch_record(rw: &ResolvedWatch, alias_of: Option<&str>) -> Record {
    let spec = &rw.spec;
    Record {
        header: None,
        fields: vec![
            RecordField::str("name", spec.name()),
            RecordField::str("path", spec.path().to_string_lossy()),
            RecordField::opt("branch", spec.branch()),
            RecordField::str("remote", spec.remote()),
            RecordField::str("trigger", spec.trigger().to_string()),
            RecordField::str("interval", record::format_duration(spec.interval())),
            RecordField::bool("sync", spec.sync()),
            RecordField::bool("paused", rw.paused).highlighted(rw.paused),
            RecordField::opt("aliases", alias_of.map(str::to_string))
                .highlighted(alias_of.is_some()),
        ],
    }
}

/// Builds a *lenient* display record straight from a raw `[[watch]]` table,
/// used only when full resolution fails so the diagnostic `list` still renders.
/// Unset optional fields show absent (`—` / `null`) rather than resolved
/// defaults — this is the "as written" view, not the effective one.
fn raw_watch_record(w: &WatchConfig) -> Record {
    Record {
        header: None,
        fields: vec![
            RecordField::str("name", &w.name),
            RecordField::str("path", w.path.to_string_lossy()),
            RecordField::opt("branch", w.branch.clone()),
            RecordField::opt("remote", w.remote.clone()),
            RecordField::opt("trigger", w.trigger.clone()),
            RecordField::opt("interval", w.interval.map(record::format_duration)),
            // Boolean-or-null, matching the resolved path's `sync` type — a
            // machine consumer's parse must not depend on config validity.
            RecordField::opt_bool("sync", w.sync),
            RecordField::bool("paused", w.paused).highlighted(w.paused),
            // Aliasing needs canonical paths this lenient view has not resolved, so
            // it is always absent here — the field is present only to keep the
            // record shape identical to the resolved view's.
            RecordField::opt("aliases", None::<String>),
        ],
    }
}

// --- pause / resume --------------------------------------------------------

fn cmd_set_paused(paths: &WatchPaths, out: &OutCtx, target: &str, paused: bool) -> CmdResult {
    let _lock = ConfigLock::acquire(&paths.config_file)?;
    let config = require_config(paths, if paused { "pause" } else { "resume" })?;
    let index = select::select_watch(&config, target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();
    let was = config.watches[index].paused;

    let (mut doc, pre_edit) = config_edit::load_document_with_text(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while updating".to_string()))?;
    if !config_edit::set_paused(&mut doc, &name, paused) {
        return Err(CmdError::err(format!(
            "watch {name:?} vanished from the config before it could be updated"
        )));
    }
    let warning = config_edit::commit_document(&doc, &paths.config_file, Some(&pre_edit))?;

    let human = if was == paused {
        format!(
            "watch {name} was already {}",
            if paused { "paused" } else { "active" }
        )
    } else if paused {
        format!("paused watch {name}")
    } else {
        format!("resumed watch {name}")
    };
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", &name),
            RecordField::bool("paused", paused),
        ],
    };
    emit_action(out, &human, &record)?;
    warning.map_or(Ok(()), Err)
}

// --- set -------------------------------------------------------------------

/// A validated `vard watch set` plan: the keys to set (with their new string
/// values) and the keys to unset. Sets are collected in a fixed canonical order
/// so the reported output is deterministic regardless of flag order. Built
/// *before* the config lock is taken, so a bad value or a usage error fails
/// without touching the file.
struct SetPlan {
    /// `(key, new value)` for each `--<key>` flag given, all TOML strings.
    sets: Vec<(&'static str, String)>,
    /// Each `--unset <key>`, deduped in first-seen order.
    unsets: Vec<&'static str>,
}

impl SetPlan {
    /// Validates and reconciles the flags into a plan, or returns a usage error
    /// (exit 2): a bad duration, no change requested, or the same key both set
    /// and unset.
    fn from_args(args: &WatchSetArgs) -> Result<SetPlan, CmdError> {
        // Sets, in canonical key order. Durations are parse-validated here (the
        // same parse `watch add` applies) so a bad value fails cleanly before the
        // lock; the raw spelling is what gets written, exactly as `add` writes it.
        let mut sets: Vec<(&'static str, String)> = Vec::new();
        if let Some(trigger) = args.trigger {
            sets.push(("trigger", trigger.as_str().to_string()));
        }
        if let Some(interval) = &args.interval {
            opt_duration(Some(interval))?;
            sets.push(("interval", interval.clone()));
        }
        if let Some(quiesce) = &args.quiesce {
            opt_duration(Some(quiesce))?;
            sets.push(("quiesce", quiesce.clone()));
        }
        if let Some(sync_interval) = &args.sync_interval {
            opt_duration(Some(sync_interval))?;
            sets.push(("sync_interval", sync_interval.clone()));
        }
        if let Some(remote) = &args.remote {
            sets.push(("remote", remote.clone()));
        }
        if let Some(branch) = &args.branch {
            sets.push(("branch", branch.clone()));
        }

        // Unsets, deduped (a repeated `--unset trigger` is harmless).
        let mut unsets: Vec<&'static str> = Vec::new();
        for key in &args.unset {
            let key = key.as_str();
            if !unsets.contains(&key) {
                unsets.push(key);
            }
        }

        if sets.is_empty() && unsets.is_empty() {
            return Err(CmdError::err(
                "nothing to set: pass at least one of --trigger, --interval, --quiesce, \
                 --sync-interval, --remote, --branch, or --unset <key>",
            ));
        }
        // Setting and unsetting the same key in one invocation is a usage error.
        if let Some((key, _)) = sets.iter().find(|(k, _)| unsets.contains(k)) {
            return Err(CmdError::err(format!(
                "cannot both set and --unset {key} in one command"
            )));
        }
        Ok(SetPlan { sets, unsets })
    }
}

/// `vard watch set <name|path>` — edit an existing watch's settings in place.
///
/// Applies each `--<key>` and `--unset <key>` to the watch's `[[watch]]` table
/// through the comment-preserving editor, under the config lock, and validates
/// the whole result before it lands (via [`config_edit::commit_document`]) — so a bad value
/// leaves the config untouched and the daemon's reloads are never wedged. Sync,
/// the paused flag, the path, and the name are not settable here (each has its
/// own verb); the settable keys mirror `watch add`'s vocabulary.
fn cmd_set(paths: &WatchPaths, out: &OutCtx, args: WatchSetArgs) -> CmdResult {
    let plan = SetPlan::from_args(&args)?;

    let _lock = ConfigLock::acquire(&paths.config_file)?;
    let config = require_config(paths, "set")?;
    let index =
        select::select_watch(&config, &args.target).map_err(|e| CmdError::err(e.to_string()))?;
    let name = config.watches[index].name.clone();

    let (mut doc, pre_edit) = config_edit::load_document_with_text(&paths.config_file)?
        .ok_or_else(|| CmdError::err("config file vanished while updating".to_string()))?;

    let vanished = || {
        CmdError::err(format!(
            "watch {name:?} vanished from the config before it could be updated"
        ))
    };

    // Apply the sets.
    for (key, val) in &plan.sets {
        if !config_edit::set_watch_field(&mut doc, &name, key, val) {
            return Err(vanished());
        }
    }
    // Apply the unsets. Removing a key the watch does not set is reported and
    // exits 2 (parity with `vard config unset`) — and, because it returns before
    // the commit below, the config is left untouched.
    for key in &plan.unsets {
        match config_edit::unset_watch_field(&mut doc, &name, key) {
            None => return Err(vanished()),
            Some(false) => {
                return Err(CmdError::err(format!(
                    "watch {name} does not set {key}; nothing to unset"
                )));
            }
            Some(true) => {}
        }
    }

    let warning = config_edit::commit_document(&doc, &paths.config_file, Some(&pre_edit))?;

    // Report the applied changes: a one-line summary in the records form, and a
    // single flat object in the machine forms — the watch name plus one field per
    // changed key (its new value for a set, `null` for an unset).
    let mut fields = vec![RecordField::str("name", &name)];
    let mut changes: Vec<String> = Vec::new();
    for (key, val) in &plan.sets {
        fields.push(RecordField::str(key, val.clone()));
        changes.push(format!("{key} = {val}"));
    }
    for key in &plan.unsets {
        fields.push(RecordField::opt(key, None::<String>));
        changes.push(format!("unset {key}"));
    }
    let human = format!("updated watch {name}: {}", changes.join(", "));
    let record = Record {
        header: None,
        fields,
    };
    emit_action(out, &human, &record)?;
    warning.map_or(Ok(()), Err)
}

// --- sync opt-in -----------------------------------------------------------

/// `vard watch sync <name|path> [--off]` — the syncing opt-in gesture.
///
/// Without `--off`: writes `sync = true` on the watch (comment-preserving) and
/// then runs one confirmation cycle through the exact `vard sync <name>`
/// dispatch — the first cycle IS the confirmation. With `--off`: writes an
/// explicit `sync = false` pin (which also overrides a `defaults.sync = true`)
/// and runs no cycle, reporting plainly like pause/resume. The config write
/// releases its lock before any cycle runs, since the cycle takes the separate
/// instance lock.
fn cmd_sync(
    paths: &WatchPaths,
    out: &OutCtx,
    args: &WatchSyncArgs,
    color: ColorWhen,
    format: Option<OutputFormat>,
) -> CmdResult {
    let name = {
        let _lock = ConfigLock::acquire(&paths.config_file)?;
        let op = if args.off {
            "disable syncing for"
        } else {
            "enable syncing for"
        };
        let config = require_config(paths, op)?;
        let index = select::select_watch(&config, &args.target)
            .map_err(|e| CmdError::err(e.to_string()))?;
        let name = config.watches[index].name.clone();

        let (mut doc, pre_edit) = config_edit::load_document_with_text(&paths.config_file)?
            .ok_or_else(|| CmdError::err("config file vanished while updating".to_string()))?;
        if !config_edit::set_sync(&mut doc, &name, !args.off) {
            return Err(CmdError::err(format!(
                "watch {name:?} vanished from the config before it could be updated"
            )));
        }
        // A write that lands but leaves the config still-invalid carries an
        // attention warning; surface it and run no confirmation cycle against a
        // config we could not fully validate.
        if let Some(warning) =
            config_edit::commit_document(&doc, &paths.config_file, Some(&pre_edit))?
        {
            return Err(warning);
        }
        name
    }; // the config lock is released here, before any cycle takes the instance lock

    if args.off {
        let human = format!("disabled syncing for watch {name}");
        let record = Record {
            header: None,
            fields: vec![
                RecordField::str("name", &name),
                RecordField::bool("sync", false),
            ],
        };
        return emit_action(out, &human, &record);
    }

    confirm_sync(paths, out, &name, color, format)
}

/// Runs the confirmation sync cycle for a just-enabled watch through the exact
/// `vard sync <name>` dispatch, then — in the records form only — points at how
/// to add a remote when the watch has none (vard never creates remotes). Returns
/// the cycle's own result, so `watch sync`'s (and `watch add --sync`'s) exit code
/// mirrors `vard sync`.
fn confirm_sync(
    paths: &WatchPaths,
    out: &OutCtx,
    name: &str,
    color: ColorWhen,
    format: Option<OutputFormat>,
) -> CmdResult {
    let result = crate::cmd::sync::run_inner(
        SyncArgs {
            target: Some(name.to_string()),
        },
        color,
        format,
    );
    print_missing_remote_hint(out, paths, name);
    result
}

/// The `watch add --sync` confirmation: runs the same cycle as [`confirm_sync`]
/// but through [`cmd::sync::collect`](crate::cmd::sync::collect), so the JSON
/// form folds the cycle's rows into the single add object (`"sync": [...]`)
/// rather than emitting a second top-level document. Records and JSONL emit the
/// add result and the cycle output back to back (both formats allow multiple
/// values), matching the standalone `vard sync` output.
fn confirm_add_sync(
    paths: &WatchPaths,
    out: &OutCtx,
    human: String,
    add_record: Record,
    warning: Option<CmdError>,
    name: &str,
) -> CmdResult {
    // A write that landed still-invalid must not run a confirmation cycle; emit
    // the add result plainly and surface the warning.
    if let Some(w) = warning {
        emit_action(out, &human, &add_record)?;
        return Err(w);
    }

    let (result, emit) = crate::cmd::sync::collect(&SyncArgs {
        target: Some(name.to_string()),
    });

    match out.format {
        OutputFormat::Json => {
            // One parseable document: the add object with the confirmation rows
            // nested under `sync`.
            let rows = emit.into_records();
            let mut w = io::stdout().lock();
            let write = record::write_json_object_with_records(&mut w, &add_record, "sync", &rows)
                .and_then(|()| w.write_all(b"\n"));
            finish_write(write)?;
        }
        OutputFormat::Jsonl => {
            emit_action(out, &human, &add_record)?;
            crate::cmd::sync::emit_sync(out, &emit)?;
        }
        OutputFormat::Records => {
            emit_action(out, &human, &add_record)?;
            crate::cmd::sync::emit_sync(out, &emit)?;
            print_missing_remote_hint(out, paths, name);
        }
    }
    result
}

/// In the records form, when `name`'s repository lacks its configured remote,
/// print a one-line pointer at how to add it — vard never creates remotes. Best
/// effort: a probe failure or a write error is silently dropped so it can never
/// mask the confirmation cycle's own result.
fn print_missing_remote_hint(out: &OutCtx, paths: &WatchPaths, name: &str) {
    if !matches!(out.format, OutputFormat::Records) {
        return;
    }
    if let Some(remote) = watch_missing_remote(paths, name) {
        let mut w = io::stdout().lock();
        let _ = writeln!(
            w,
            "  no {remote:?} remote in the repository yet — add one, then re-sync: \
             git remote add {remote} <url>"
        );
    }
}

/// The watch's configured remote name when its repository does NOT define that
/// remote, or `None` when it does (or the check could not run). Best-effort: any
/// load/resolve/open failure yields `None`, so guidance is offered only when the
/// remote is positively known missing — the confirmation cycle's own row is the
/// authoritative outcome regardless.
fn watch_missing_remote(paths: &WatchPaths, name: &str) -> Option<String> {
    let config = load_config(&paths.config_file).ok()??;
    let index = select::select_watch(&config, name).ok()?;
    let mut resolved = config.resolve_all().ok()?;
    let rw = resolved.swap_remove(index);
    match vard_core::open_git_backend(&rw.spec).ok()?.has_remote() {
        Ok(false) => Some(rw.spec.remote().to_string()),
        Ok(true) | Err(_) => None,
    }
}

// --- shared helpers --------------------------------------------------------

/// Loads and validates the config, or `None` when the file does not exist.
fn load_config(config_file: &Path) -> Result<Option<Config>, CmdError> {
    Config::load_optional(config_file).map_err(|e| CmdError::err(e.to_string()))
}

/// Like [`load_config`], but a missing file is an error naming the failed
/// operation — you cannot remove or pause a watch that was never added.
fn require_config(paths: &WatchPaths, op: &str) -> Result<Config, CmdError> {
    load_config(&paths.config_file)?.ok_or_else(|| {
        CmdError::err(format!(
            "no config file at {}; nothing to {op}",
            paths.config_file.display()
        ))
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn opt_duration(raw: Option<&str>) -> Result<Option<Duration>, CmdError> {
    // `vard_core::parse_duration`'s error already names the offending value, so
    // it is surfaced verbatim rather than re-wrapped.
    raw.map(|v| vard_core::parse_duration(v).map_err(|e| CmdError::err(e.to_string())))
        .transpose()
}

/// Emits the watch list in the resolved format, under the `watches` noun.
fn emit_list(out: &OutCtx, records: &[Record]) -> CmdResult {
    emit_records(out, records, "watches")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::LockRole;
    use crate::journal::test_support::{plant_crashed, retry_until};

    fn paths_in(dir: &Path) -> WatchPaths {
        WatchPaths {
            config_file: dir.join("config.toml"),
            journal_dir: dir.join("journal"),
            lock_file: dir.join("vard.lock"),
        }
    }

    /// Writes a clean (empty) path-keyed journal for `repo` and returns it.
    fn clean_journal(paths: &WatchPaths, repo: &Path) -> Journal {
        let journal = Journal::for_repo_in_dir(&paths.journal_dir, repo);
        std::fs::create_dir_all(journal.path().parent().unwrap()).unwrap();
        std::fs::write(journal.path(), b"").unwrap();
        journal
    }

    #[test]
    fn no_daemon_remove_drains_a_proven_stale_lock() {
        // With no daemon holding the lock, a `remove` drains the watch: recovery
        // proves the crash-leftover lock ours and cleans it, and compacts the
        // journal — the no-daemon half of drain-on-remove.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let (repo, lock) = plant_crashed(&paths.journal_dir, &dir.path().join("repo"));

        assert!(
            drain_removed_watch(&paths, &repo, "repo"),
            "with no daemon the CLI drains and reports it drained"
        );

        assert!(!lock.exists(), "a proven-stale lock must be drained");
        let journal = Journal::for_repo_in_dir(&paths.journal_dir, &repo);
        assert_eq!(
            std::fs::metadata(journal.path()).unwrap().len(),
            0,
            "the journal is compacted after the drain"
        );
    }

    #[test]
    fn remove_drain_defers_to_a_running_daemon() {
        // A daemon holds the instance lock, so the CLI must NOT double-drain —
        // the daemon's reload covers it. The lock is left for the daemon.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::create_dir_all(&paths.journal_dir).unwrap();
        let _daemon = InstanceLock::acquire_at(&paths.lock_file, LockRole::Daemon).unwrap();
        let (repo, lock) = plant_crashed(&paths.journal_dir, &dir.path().join("repo"));

        assert!(
            !drain_removed_watch(&paths, &repo, "repo"),
            "the CLI must report it did NOT drain when a daemon holds the lock"
        );

        assert!(
            lock.exists(),
            "with a daemon holding the lock the CLI must defer, not drain"
        );
    }

    #[test]
    fn purge_deletes_a_clean_journal_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let repo = dir.path().join("repo");
        let journal = clean_journal(&paths, &repo);

        // A clean journal is deletable even without a drain. (Retry rides out the
        // sibling-fork/exec race that can transiently hold the just-created op-lock
        // fd — the same artifact the flock tests document; purge correctly retains
        // on a *genuine* concurrent holder.)
        assert!(retry_until(|| matches!(
            purge_metadata(&paths, &repo, false),
            Ok(true)
        )));
        assert!(!journal.path().exists(), "purge deletes the journal file");
        // A second purge on an already-absent journal is a clean no-op.
        assert!(retry_until(|| matches!(
            purge_metadata(&paths, &repo, false),
            Ok(true)
        )));
    }

    #[test]
    fn purge_after_a_cli_drain_deletes_even_a_dangling_journal() {
        // Acquired + dangling: the CLI drained it, so purge deletes the file
        // (the drain already compacted it clean, and drained=true authorizes it).
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let (repo, _lock) = plant_crashed(&paths.journal_dir, &dir.path().join("repo"));

        let drained = drain_removed_watch(&paths, &repo, "repo");
        assert!(drained);
        // Retry rides out the fork/exec race on the just-released op-lock fd.
        assert!(retry_until(|| matches!(
            purge_metadata(&paths, &repo, drained),
            Ok(true)
        )));
        let journal = Journal::for_repo_in_dir(&paths.journal_dir, &repo);
        assert!(!journal.path().exists(), "a drained journal is purged");
    }

    #[test]
    fn purge_retains_a_dangling_journal_when_a_daemon_holds_the_lock() {
        // DaemonHeld + dangling: the CLI could not drain, and the journal still
        // records an open operation — purge must NOT delete the evidence.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        std::fs::create_dir_all(&paths.journal_dir).unwrap();
        let _daemon = InstanceLock::acquire_at(&paths.lock_file, LockRole::Daemon).unwrap();
        let (repo, _lock) = plant_crashed(&paths.journal_dir, &dir.path().join("repo"));

        let drained = drain_removed_watch(&paths, &repo, "repo");
        assert!(!drained, "a daemon holds the lock");
        assert!(
            matches!(purge_metadata(&paths, &repo, drained), Ok(false)),
            "a dangling journal is retained when the CLI could not drain it"
        );
        let journal = Journal::for_repo_in_dir(&paths.journal_dir, &repo);
        assert!(
            journal.path().exists(),
            "the evidence survives for the daemon"
        );
    }

    #[test]
    fn purge_deletes_a_clean_journal_even_when_a_daemon_holds_the_lock() {
        // DaemonHeld + clean: nothing is dangling, so deleting destroys no
        // evidence — purge proceeds.
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_in(dir.path());
        let _daemon = InstanceLock::acquire_at(&paths.lock_file, LockRole::Daemon).unwrap();
        let repo = dir.path().join("repo");
        let journal = clean_journal(&paths, &repo);

        let drained = drain_removed_watch(&paths, &repo, "repo");
        assert!(!drained);
        // Retry rides out the fork/exec race on the just-created op-lock fd.
        assert!(retry_until(|| matches!(
            purge_metadata(&paths, &repo, drained),
            Ok(true)
        )));
        assert!(!journal.path().exists(), "a clean journal is safe to purge");
    }

    #[test]
    fn validate_watch_mirrors_the_effective_sync_value() {
        // Regression for the `sync(!no_sync)` quirk: with no explicit pin the
        // validated spec must NOT be forced to `sync = true` — it mirrors what
        // the edit writes (an absent key, i.e. the core default), not a value the
        // config never carries. An explicit pin is reflected faithfully.
        let path = Path::new("/some/watch");
        let sync_of = |pin| match validate_watch("w", path, None, None, None, None, pin, None, None)
        {
            Ok(spec) => spec.sync(),
            Err(e) => panic!("validate_watch rejected a valid watch: {}", e.message()),
        };

        assert_eq!(
            sync_of(None),
            vard_core::DEFAULT_SYNC,
            "an absent pin must not force sync on"
        );
        assert!(
            sync_of(Some(true)),
            "an explicit sync=true pin validates on"
        );
        assert!(
            !sync_of(Some(false)),
            "an explicit sync=false pin validates off"
        );
    }

    #[test]
    fn list_marks_a_canonically_aliased_watch() {
        // Two watches whose paths canonicalize to one repo (a directory and a
        // symlink to it) share a journal key. The daemon supervises only the
        // first, so `list` must carry an `aliases` marker naming the winner on the
        // second and leave the first unmarked.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();

        let watches = [
            ResolvedWatch {
                spec: vard_core::WatchSpec::builder("first", &repo)
                    .build()
                    .unwrap(),
                paused: false,
            },
            ResolvedWatch {
                spec: vard_core::WatchSpec::builder("second", &link)
                    .build()
                    .unwrap(),
                paused: false,
            },
        ];
        let aliases =
            journal::alias_winners(watches.iter().map(|w| (w.spec.name(), w.spec.path())));

        let alias_cell = |rec: &Record| {
            rec.fields
                .iter()
                .find(|f| f.key == "aliases")
                .expect("aliases field present")
                .cell
                .clone()
        };
        let first_rec = watch_record(&watches[0], aliases[0].as_deref());
        let second_rec = watch_record(&watches[1], aliases[1].as_deref());

        assert!(
            matches!(alias_cell(&first_rec), record::Cell::Absent),
            "the first (winning) watch is not aliased"
        );
        assert!(
            matches!(alias_cell(&second_rec), record::Cell::Str(s) if s == "first"),
            "the second watch is marked as aliasing the first"
        );
    }
}
