//! `vard diff <name|path> [<ref>]` — show a raw unified diff for a watch.
//!
//! Read-only. Output is a raw unified diff and nothing else: paged on a TTY,
//! passed through untouched when piped so it feeds `patch` / `git apply` / a
//! file. Because a unified diff is inherently a text artifact, `diff` is
//! text-only — an *explicit* `--format json`/`jsonl` is rejected, while the
//! piped default (which resolves to JSON for other commands) still yields plain
//! diff text so `vard diff notes > changes.patch` works.

use std::process::ExitCode;

use vard_core::{LogFilter, VcsBackend, VcsError, VcsRef};

use super::{
    CmdError, CmdPaths, CmdResult, OutCtx, emit_raw_paged, load_config, open_backend, select_one,
};
use crate::cli::{ColorWhen, DiffArgs, OutputFormat};

/// Entry point for `vard diff`.
pub(crate) fn run(args: DiffArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: DiffArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let paths = CmdPaths::from_xdg().map_err(|e| CmdError::err(e.to_string()))?;
    let out = OutCtx::resolve(color, format);

    // Reject an explicit machine format; the piped auto-default is left as text.
    if matches!(
        out.raw_format,
        Some(OutputFormat::Json) | Some(OutputFormat::Jsonl)
    ) {
        return Err(CmdError::err(
            "diff emits a raw unified diff and is text-only; --format json/jsonl is not \
             supported — pipe the diff to a file or to `patch`/`git apply` instead",
        ));
    }

    let config = load_config(&paths.config_file)?;
    let rw = select_one(&config, &args.target)?;
    let backend = open_backend(&rw.spec)?;
    let name = rw.spec.name();

    let from = args.reference.clone().unwrap_or_else(|| "HEAD".to_string());

    // The default `HEAD` reference is undefined in a repo with no snapshots
    // yet; give a friendly note rather than surfacing git's raw error.
    if args.reference.is_none() {
        let recent = backend
            .log(&LogFilter {
                since: None,
                limit: Some(1),
            })
            .map_err(|e| CmdError::err(format!("reading log for {name:?}: {e}")))?;
        if recent.is_empty() {
            eprintln!("vard: no snapshots yet for {name:?}; nothing to diff against");
            return Ok(());
        }
    }

    let diff = backend
        .diff(&VcsRef::new(&from), None)
        .map_err(|e| map_diff_err(e, &from, name))?;

    if diff.is_empty() {
        if out.is_tty {
            eprintln!("vard: no differences between {from} and the working tree of {name:?}");
        }
        return Ok(());
    }

    emit_raw_paged(&out, diff.as_bytes(), "vard diff")
}

/// Maps a diff failure, giving an unknown reference a friendly message and
/// otherwise surfacing the backend error.
fn map_diff_err(e: VcsError, reference: &str, name: &str) -> CmdError {
    if let VcsError::CommandFailed { stderr, .. } = &e
        && (stderr.contains("unknown revision")
            || stderr.contains("bad revision")
            || stderr.contains("ambiguous argument"))
    {
        return CmdError::err(format!("no such reference {reference:?} in watch {name:?}"));
    }
    CmdError::err(format!("diffing {name:?}: {e}"))
}
