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
//! The chosen revision is validated (see [`VcsBackend::verify_ref`]) *before*
//! any protective snapshot, so a typo'd `--ref` fails cleanly with nothing
//! changed — and the dry-run and real paths reject bad input identically.
//!
//! # The journal single-writer invariant
//!
//! The per-watch operation journal has exactly one writer: whoever holds the
//! instance lock. The daemon holds it for its lifetime; an in-process CLI holds
//! it for the duration of its operation. Restore honors this by *trying* to
//! acquire the instance lock and branching on who holds it:
//!
//! * **We acquire it** (no daemon): we hold it across BOTH the protective
//!   snapshot and the checkout, journaling the pair as one
//!   `begin("restore")`→`complete` bracket, so a crash mid-restore leaves one
//!   recoverable record. This is the common, fully-protected case.
//! * **A daemon holds it**: the daemon is the journal's writer, so we must NOT
//!   write it. We proceed without journaling — git's own `index.lock`
//!   serializes our two git commands against the daemon's, so the worst case is
//!   a [`VcsError::LockContended`], surfaced as a retryable "attention"
//!   outcome, never data loss. The residual risk is narrow: if *this* process
//!   crashes between the protective snapshot's `git add`/`commit` and its
//!   completion, no journal record names the abandoned `index.lock`, so the
//!   next daemon start cannot prove it stale and clean it — a human (or the
//!   tracked doctor-tool follow-up) must remove it. This is the honest cost of
//!   not owning the lock; it is documented rather than papered over with ad-hoc
//!   locking.
//! * **A peer CLI holds it**: we wait a bounded spell and then report honestly
//!   that another command is running, rather than racing it.

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use vard_core::{
    LogFilter, RestoreTarget, SafeState, SnapshotOutcome, SnapshotRequest, Trigger, VcsBackend,
    VcsError, VcsRef,
};

use super::timefmt::{format_rfc3339_utc, parse_at};
use super::{
    CmdError, CmdPaths, CmdResult, OutCtx, emit_action, emit_raw_paged, load_config, open_backend,
    select_one,
};
use crate::cli::{ColorWhen, OutputFormat, RestoreArgs};
use crate::instance::{CliLock, InstanceLock};
use crate::output::record::{Record, RecordField};

/// How long a real restore waits out a *peer CLI* lock holder before reporting
/// that another command is running (matching `vard snapshot`).
const CLI_LOCK_BUDGET: Duration = Duration::from_secs(10);

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
    let repo_path = rw.spec.path();

    let rev = resolve_rev(&backend, &args, name)?;
    // Validate the revision BEFORE any protective snapshot, so a typo'd --ref
    // fails with nothing changed and the dry-run and real paths agree on bad
    // input. (`--at` always resolves to a real snapshot id, but verifying it too
    // costs nothing and keeps one code path.)
    if !backend
        .verify_ref(&rev)
        .map_err(|e| CmdError::err(format!("verifying {rev} in watch {name:?}: {e}")))?
    {
        return Err(CmdError::err(format!(
            "no such revision {rev} in watch {name:?}"
        )));
    }
    let file = args.file.clone();

    if args.dry_run {
        return dry_run(&out, &backend, &rev, file.as_deref(), name);
    }

    // A real restore journals only while holding the instance lock (the
    // single-writer invariant). Branch on who holds it.
    match InstanceLock::acquire_for_cli(&paths.lock_file, CLI_LOCK_BUDGET) {
        Ok(CliLock::Acquired(lock)) => {
            let result = real_restore(
                &paths,
                &out,
                &backend,
                repo_path,
                &rev,
                file.as_deref(),
                name,
                true,
            );
            // Hold the lock across the whole restore; drop it only now.
            drop(lock);
            result
        }
        // A daemon owns the repo; git's index.lock serializes us against it, so
        // restore WITHOUT journaling (we are not the journal's writer).
        Ok(CliLock::DaemonHeld) => real_restore(
            &paths,
            &out,
            &backend,
            repo_path,
            &rev,
            file.as_deref(),
            name,
            false,
        ),
        Ok(CliLock::BusyPeerCli) => Err(CmdError::err(
            "another vard command is running; retry in a moment",
        )),
        Err(e) => Err(CmdError::err(format!("acquiring instance lock: {e}"))),
    }
}

/// Performs a real restore: protective snapshot, then checkout. When `journaled`
/// (we hold the instance lock), both are bracketed as one `begin("restore")`→
/// `complete` operation; otherwise (a daemon holds the lock) they run
/// unjournaled — see the [module docs](self) for why.
#[allow(clippy::too_many_arguments)]
fn real_restore(
    paths: &CmdPaths,
    out: &OutCtx,
    backend: &impl VcsBackend,
    repo_path: &Path,
    rev: &VcsRef,
    file: Option<&Path>,
    name: &str,
    journaled: bool,
) -> CmdResult {
    // Protective snapshot first — a real restore may never destroy the only
    // copy of uncommitted work. The repo must be safe to commit into to protect
    // it; if it is not, refuse rather than restore unprotected. This check sits
    // OUTSIDE the journal bracket so a doomed restore writes no record.
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
    let target = RestoreTarget {
        rev: rev.clone(),
        path: file.map(Path::to_path_buf),
    };

    // The protective snapshot AND the checkout are one operation: the protective
    // snapshot uses the RAW backend call (not `journaled_snapshot`) so it opens
    // no nested bracket that would clobber the outer `restore` one.
    let flow = || -> Result<Option<SnapshotOutcome>, CmdError> {
        let protective = match backend.snapshot(&req) {
            Ok(outcome) => outcome,
            Err(VcsError::UnsafeState(reason)) => {
                return Err(CmdError::attention(format!(
                    "cannot restore {name:?}: repository became unsafe ({reason}); \
                     nothing was changed"
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
        backend
            .restore(&target)
            .map_err(|e| map_restore_err(e, rev, target.path.as_deref(), name))?;
        Ok(protective)
    };

    let protective = if journaled {
        super::journaled(&paths.journal_dir, repo_path, name, "restore", flow)
    } else {
        flow()
    }?;

    let protective_id = protective.map(|o| o.id.as_str().to_string());
    let scope = file
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
            RecordField::opt("file", file.map(|f| f.to_string_lossy().to_string())),
            RecordField::opt("protective_snapshot", protective_id),
        ],
    };
    emit_action(out, &human, &record)
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

/// Composes `--at` from the log: parse the time expression, then ask git
/// directly for the newest snapshot at or before it (`--until` + a limit of one)
/// — the state that was current then — rather than fetching the whole history.
fn resolve_at(backend: &impl VcsBackend, at_expr: &str, name: &str) -> Result<VcsRef, CmdError> {
    let cutoff = parse_at(at_expr, SystemTime::now()).map_err(CmdError::err)?;
    let snapshots = backend
        .log(&LogFilter {
            since: None,
            until: Some(cutoff),
            limit: Some(1),
        })
        .map_err(|e| CmdError::err(format!("reading log for {name:?}: {e}")))?;
    match snapshots.first() {
        Some(snapshot) => Ok(VcsRef::from(&snapshot.id)),
        None => Err(CmdError::err(format!(
            "no snapshot at or before {} for watch {name:?}",
            format_rfc3339_utc(cutoff)
        ))),
    }
}

/// `--dry-run`: preview the differences a restore would overwrite, without
/// changing anything. A `--file` preview is scoped at the git level to that one
/// literal path (so a `--file` restore and its preview agree); a whole-tree
/// preview excludes files added *after* the target revision, which the real
/// whole-tree checkout keeps rather than overwrites.
fn dry_run(
    out: &OutCtx,
    backend: &impl VcsBackend,
    rev: &VcsRef,
    file: Option<&Path>,
    name: &str,
) -> CmdResult {
    match file {
        Some(path) => dry_run_file(out, backend, rev, path, name),
        None => dry_run_tree(out, backend, rev, name),
    }
}

/// Whole-tree dry-run. Files added after `rev` are excluded from the preview and
/// summarized in a note, because the real whole-tree restore keeps them.
fn dry_run_tree(out: &OutCtx, backend: &impl VcsBackend, rev: &VcsRef, name: &str) -> CmdResult {
    let diff = backend
        .diff(rev, None, None)
        .map_err(|e| CmdError::err(format!("previewing restore of {name:?}: {e}")))?;
    let (overwritten, added) = exclude_pure_additions(&diff);

    if added > 0 {
        eprintln!(
            "vard: dry-run: {added} file(s) added since {rev} are kept by restore \
             (excluded from this preview)"
        );
    }
    if overwritten.trim().is_empty() {
        eprintln!("vard: dry-run: {name:?} already matches {rev}; a restore would change nothing");
        return Ok(());
    }

    eprintln!(
        "vard: dry-run: the diff below is what a restore of {name:?} to {rev} would overwrite"
    );
    emit_raw_paged(out, overwritten.as_bytes(), "vard restore --dry-run")
}

/// Single-file dry-run. Pre-checks the path exists at `rev` — exactly as the
/// real restore does — and emits the same friendly error when it does not, so
/// the two agree; otherwise shows the scoped diff.
fn dry_run_file(
    out: &OutCtx,
    backend: &impl VcsBackend,
    rev: &VcsRef,
    path: &Path,
    name: &str,
) -> CmdResult {
    if !backend
        .path_exists_at(rev, path)
        .map_err(|e| CmdError::err(format!("checking {name:?}: {e}")))?
    {
        return Err(CmdError::err(path_absent_msg(
            &path.display().to_string(),
            rev,
            name,
        )));
    }

    let diff = backend
        .diff(rev, None, Some(path))
        .map_err(|e| CmdError::err(format!("previewing restore of {name:?}: {e}")))?;
    if diff.trim().is_empty() {
        eprintln!(
            "vard: dry-run: {} in {name:?} already matches {rev}; a restore would change nothing",
            path.display()
        );
        return Ok(());
    }

    eprintln!(
        "vard: dry-run: the diff below is what a restore of {} in {name:?} to {rev} would overwrite",
        path.display()
    );
    emit_raw_paged(out, diff.as_bytes(), "vard restore --dry-run")
}

/// Splits a unified diff into per-file sections and drops the pure additions —
/// files present in the work tree but absent at the target revision, which a
/// whole-tree restore leaves untouched. Returns the retained diff and the count
/// of excluded additions. A pure addition is a section carrying git's `new file
/// mode` line.
fn exclude_pure_additions(diff: &str) -> (String, usize) {
    let mut kept = String::new();
    let mut added = 0usize;
    for section in split_git_sections(diff) {
        if section.lines().any(|l| l.starts_with("new file mode")) {
            added += 1;
        } else {
            kept.push_str(&section);
        }
    }
    (kept, added)
}

/// Splits a unified diff into sections, each beginning at a `diff --git` header.
fn split_git_sections(diff: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
}

/// The friendly "path absent at revision" message, shared by the real restore's
/// error mapping and the dry-run pre-check so the two are byte-identical.
fn path_absent_msg(path_display: &str, rev: &VcsRef, name: &str) -> String {
    format!("{path_display:?} does not exist at {rev} in watch {name:?}")
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
            CmdError::err(path_absent_msg(&p, rev, name))
        }
        VcsError::LockContended { .. } => CmdError::attention(format!(
            "another vard operation holds the git lock; retry later — the protective snapshot \
             of {name:?} was taken but the restore did not run"
        )),
        _ => CmdError::err(format!("restoring {name:?} to {rev}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_pure_additions_drops_new_files_and_counts_them() {
        let diff = "\
diff --git a/kept.txt b/kept.txt
index 111..222 100644
--- a/kept.txt
+++ b/kept.txt
@@ -1 +1 @@
-old
+new
diff --git a/added.txt b/added.txt
new file mode 100644
index 000..333
--- /dev/null
+++ b/added.txt
@@ -0,0 +1 @@
+brand new
";
        let (kept, added) = exclude_pure_additions(diff);
        assert_eq!(added, 1, "one pure addition excluded");
        assert!(kept.contains("kept.txt"), "the modification is retained");
        assert!(!kept.contains("added.txt"), "the addition is excluded");
    }

    #[test]
    fn exclude_pure_additions_keeps_modifications_and_deletions() {
        // A file present at rev but deleted in the work tree (`+++ /dev/null`)
        // is NOT a pure addition — restoring it back IS an overwrite, so it must
        // stay in the preview.
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 444..000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-was here
";
        let (kept, added) = exclude_pure_additions(diff);
        assert_eq!(added, 0);
        assert!(kept.contains("gone.txt"));
    }

    #[test]
    fn path_absent_msg_is_stable() {
        let rev = VcsRef::new("abc123");
        assert_eq!(
            path_absent_msg("with space.txt", &rev, "notes"),
            "\"with space.txt\" does not exist at abc123 in watch \"notes\""
        );
    }
}
