//! `vard restore <name|path> [--at | --ref] [--file] [--dry-run]` — restore a
//! watch's tree (or one file) to a prior snapshot.
//!
//! # The protective-snapshot invariant
//!
//! A real restore ALWAYS takes a protective snapshot of the current state
//! first (in-process, journaled, `Vard-Trigger: pre-restore`), so no restore
//! can ever destroy uncommitted work: whatever the restore is about to
//! overwrite is committed to history and recoverable from the log. `--dry-run`
//! takes no protective snapshot because it changes nothing.
//!
//! # Interaction with a running daemon (documented, not locked)
//!
//! Unlike `snapshot`, restore cannot use the single-instance lock as a guard:
//! when a daemon is running it *holds* that lock, so the CLI could never take
//! it. Restore therefore proceeds without it. Two honest consequences:
//!
//! * The daemon will observe the restored files change and snapshot the
//!   restored state shortly after — that is by design; the restore is just
//!   another edit as far as the daemon is concerned.
//! * The protective snapshot and the restore each shell out to git while the
//!   daemon may also be committing the same repo. git's own `index.lock`
//!   serializes them, so the worst case is a [`VcsError::LockContended`], which
//!   is surfaced as a retryable "attention" outcome — never data loss. The
//!   per-watch operation journal is keyed by watch name and is written by both
//!   the daemon and this command; because each brackets its own operation and
//!   compacts on completion, a concurrent write can at worst leave a transient
//!   record naming the live daemon (so a later recovery sees a live holder and
//!   declines to touch the lock — the conservative outcome). No ad-hoc locking
//!   is invented here; the residual race is documented rather than papered
//!   over.

use std::path::Path;
use std::process::ExitCode;
use std::time::SystemTime;

use vard_core::{
    LogFilter, RestoreTarget, SafeState, SnapshotRequest, Trigger, VcsBackend, VcsError, VcsRef,
};

use super::timefmt::{format_rfc3339_utc, parse_at};
use super::{
    CmdError, CmdPaths, CmdResult, OutCtx, emit_action, emit_raw_paged, journaled_snapshot,
    load_config, open_backend, select_one,
};
use crate::cli::{ColorWhen, OutputFormat, RestoreArgs};
use crate::output::record::{Record, RecordField};

/// Entry point for `vard restore`.
pub(crate) fn run(args: RestoreArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: RestoreArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let paths = CmdPaths::from_xdg().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);

    let config = load_config(&paths.config_file)?;
    let rw = select_one(&config, &args.target)?;
    let backend = open_backend(&rw.spec)?;
    let name = rw.spec.name();

    let rev = resolve_rev(&backend, &args, name)?;
    let file = args.file.clone();

    if args.dry_run {
        return dry_run(&out, &backend, &rev, file.as_deref(), name);
    }

    // Protective snapshot first — a real restore may never destroy the only
    // copy of uncommitted work. The repo must be safe to commit into to protect
    // it; if it is not, refuse the restore rather than restore unprotected.
    match backend
        .is_safe_state()
        .map_err(|e| CmdError::err(format!("checking {name:?}: {e}")))?
    {
        SafeState::Unsafe(reason) => {
            return Err(CmdError::attention(format!(
                "cannot restore {name:?}: repository is not in a safe state ({reason}); \
                 no protective snapshot was taken and nothing was changed"
            )));
        }
        SafeState::Safe => {}
    }

    let req = SnapshotRequest {
        trigger: Trigger::PreRestore,
        user_text: Some(format!("pre-restore snapshot before restoring to {rev}")),
        extra_trailers: Vec::new(),
    };
    let protective = match journaled_snapshot(&paths.journal_dir, name, &backend, &req) {
        Ok(outcome) => outcome,
        Err(VcsError::UnsafeState(reason)) => {
            return Err(CmdError::attention(format!(
                "cannot restore {name:?}: repository became unsafe ({reason}); nothing was changed"
            )));
        }
        Err(VcsError::LockContended { .. }) => {
            return Err(CmdError::attention(
                "another vard operation holds the git lock; retry later — nothing was changed",
            ));
        }
        Err(e) => {
            return Err(CmdError::err(format!(
                "protective snapshot failed for {name:?}: {e}; restore aborted"
            )));
        }
    };

    // Now overwrite the tree (or the one file) with the target revision.
    let target = RestoreTarget {
        rev: rev.clone(),
        path: file.clone(),
    };
    backend
        .restore(&target)
        .map_err(|e| map_restore_err(e, &rev, file.as_deref(), name))?;

    let protective_id = protective.map(|o| o.id.as_str().to_string());
    let scope = file
        .as_ref()
        .map(|f| format!("{} in ", f.display()))
        .unwrap_or_default();
    let human = match &protective_id {
        Some(id) => format!("restored {scope}{name} to {rev} (protective snapshot {id})"),
        None => format!(
            "restored {scope}{name} to {rev} (working tree was clean; no protective snapshot needed)"
        ),
    };
    let record = Record {
        header: None,
        fields: vec![
            RecordField::str("name", name),
            RecordField::str("status", "restored"),
            RecordField::str("restored_to", rev.as_str()),
            RecordField::opt(
                "file",
                file.as_ref().map(|f| f.to_string_lossy().to_string()),
            ),
            RecordField::opt("protective_snapshot", protective_id),
        ],
    };
    emit_action(&out, &human, &record)
}

/// Resolves the revision to restore from: `--ref` directly, or `--at`
/// composed from the log (the newest snapshot at or before the named time).
/// clap keeps the two mutually exclusive; neither given is an error.
fn resolve_rev(
    backend: &impl VcsBackend,
    args: &RestoreArgs,
    name: &str,
) -> Result<VcsRef, CmdError> {
    match (&args.reference, &args.at) {
        (Some(reference), _) => Ok(VcsRef::new(reference)),
        (None, Some(at)) => resolve_at(backend, at, name),
        (None, None) => Err(CmdError::err(
            "restore needs a target: pass --ref <sha> or --at <when>",
        )),
    }
}

/// Composes `--at` from the log: parse the time expression, then pick the most
/// recent snapshot committed at or before it (the state that was current then).
fn resolve_at(backend: &impl VcsBackend, at_expr: &str, name: &str) -> Result<VcsRef, CmdError> {
    let cutoff = parse_at(at_expr, SystemTime::now()).map_err(CmdError::err)?;
    let snapshots = backend
        .log(&LogFilter {
            since: None,
            limit: None,
        })
        .map_err(|e| CmdError::err(format!("reading log for {name:?}: {e}")))?;
    // The log is most-recent-first, so the first entry at or before the cutoff
    // is the snapshot that was current at that time.
    match snapshots.iter().find(|s| s.time <= cutoff) {
        Some(snapshot) => Ok(VcsRef::from(&snapshot.id)),
        None => Err(CmdError::err(format!(
            "no snapshot at or before {} for watch {name:?}",
            format_rfc3339_utc(cutoff)
        ))),
    }
}

/// `--dry-run`: preview the differences a restore would overwrite, via a diff
/// of the target revision against the current tree, without changing anything.
/// For a single `--file`, the whole-tree diff is filtered to that path's
/// sections so the preview matches the scoped restore.
fn dry_run(
    out: &OutCtx,
    backend: &impl VcsBackend,
    rev: &VcsRef,
    file: Option<&Path>,
    name: &str,
) -> CmdResult {
    let diff = backend
        .diff(rev, None)
        .map_err(|e| CmdError::err(format!("previewing restore of {name:?}: {e}")))?;
    let scoped = match file {
        Some(path) => filter_diff_by_path(&diff, path),
        None => diff,
    };

    if scoped.trim().is_empty() {
        let scope = file
            .map(|p| format!("{} in ", p.display()))
            .unwrap_or_default();
        eprintln!(
            "vard: dry-run: {scope}{name:?} already matches {rev}; a restore would change nothing"
        );
        return Ok(());
    }

    eprintln!(
        "vard: dry-run: the diff below is what a restore of {name:?} to {rev} would overwrite"
    );
    emit_raw_paged(out, scoped.as_bytes(), "vard restore --dry-run")
}

/// Keeps only the unified-diff sections whose `diff --git a/… b/…` header names
/// `path`. Best-effort (paths with spaces are git-quoted and will not match);
/// the whole-tree preview is unaffected.
fn filter_diff_by_path(diff: &str, path: &Path) -> String {
    let needle = path.to_string_lossy();
    let a = format!("a/{needle}");
    let b = format!("b/{needle}");
    let mut out = String::new();
    let mut keep = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            keep = rest.split_whitespace().any(|t| t == a || t == b);
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Maps a restore failure: a path absent at the target revision becomes a
/// friendly error naming the path and the reference; a contended git lock
/// becomes a retryable attention outcome; anything else is surfaced.
fn map_restore_err(e: VcsError, rev: &VcsRef, file: Option<&Path>, name: &str) -> CmdError {
    match &e {
        VcsError::CommandFailed { stderr, .. }
            if stderr.contains("did not match") || stderr.contains("pathspec") =>
        {
            let p = file
                .map(|f| f.display().to_string())
                .unwrap_or_else(|| ".".to_string());
            CmdError::err(format!("{p:?} does not exist at {rev} in watch {name:?}"))
        }
        VcsError::LockContended { .. } => CmdError::attention(format!(
            "another vard operation holds the git lock; retry later — the protective snapshot \
             of {name:?} was taken but the restore did not run"
        )),
        _ => CmdError::err(format!("restoring {name:?} to {rev}: {e}")),
    }
}
