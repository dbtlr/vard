//! The command-outcome layer shared by every `vard` subcommand — the top-level
//! history commands (`cmd/`) and the config-mutating watch commands (`watch/`).
//!
//! One [`CmdError`] (message + exit code, with the `err`/`attention` severities
//! and their `worse` fold), one [`OutCtx`] resolving color/format/terminal
//! once, and one set of emitters ([`emit_records`], [`emit_action`],
//! [`emit_raw_paged`]) that render through the resolved `--format`. Factoring
//! these here keeps the two command families' output contract identical and
//! removes the two near-duplicate copies that had drifted apart.

use std::io::{self, IsTerminal, Write};

use crate::cli::{ColorWhen, OutputFormat};
use crate::output::format;
use crate::output::pager::{should_page, spawn_pager_or_passthrough};
use crate::output::palette::{self, Palette};
use crate::output::record::{self, Record};

/// A command failure carrying the message to print and the process exit code
/// (2 for an error, 1 for "attention needed" such as an unsafe repository or a
/// declined `git init`).
#[derive(Debug)]
pub(crate) struct CmdError {
    message: String,
    code: u8,
}

impl CmdError {
    /// An error (exit code 2).
    pub(crate) fn err(message: impl Into<String>) -> Self {
        CmdError {
            message: message.into(),
            code: 2,
        }
    }

    /// An "attention needed" outcome (exit code 1): the command did not fail,
    /// but it also did not fully complete — e.g. a repository was not in a safe
    /// state, a git lock was contended, or the user declined an init.
    pub(crate) fn attention(message: impl Into<String>) -> Self {
        CmdError {
            message: message.into(),
            code: 1,
        }
    }

    /// The higher-severity of two exit codes (2 beats 1 beats 0), for
    /// aggregating per-watch outcomes.
    pub(crate) fn worse(a: u8, b: u8) -> u8 {
        a.max(b)
    }

    /// The error's human-readable message.
    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    /// The process exit code this error maps to (2 for an error, 1 for
    /// attention). Used by tests to assert the exit class; the binary reads the
    /// field directly through [`finish`].
    #[cfg(test)]
    pub(crate) fn code(&self) -> u8 {
        self.code
    }
}

pub(crate) type CmdResult = Result<(), CmdError>;

/// Resolved output settings shared by every command's emitter.
pub(crate) struct OutCtx {
    /// The effective format after resolving the global `--format` against the
    /// destination.
    pub(crate) format: OutputFormat,
    /// The raw `--format` flag, before destination resolution. `diff` needs it
    /// to tell an explicit `--format json` (rejected) from the piped default.
    pub(crate) raw_format: Option<OutputFormat>,
    pub(crate) palette: Palette,
    pub(crate) term_width: usize,
    pub(crate) term_height: usize,
    pub(crate) is_tty: bool,
}

impl OutCtx {
    /// Resolve for a standard command: records on a TTY, JSON when piped, absent
    /// an explicit `--format`.
    pub(crate) fn resolve(color: ColorWhen, format_flag: Option<OutputFormat>) -> OutCtx {
        Self::build(color, format_flag, format::resolve(format_flag, is_tty()))
    }

    /// Resolve for a single-value surface (`config get`, `config path`): the bare
    /// value regardless of destination, absent an explicit `--format`. See
    /// [`format::resolve_single_value`].
    pub(crate) fn resolve_single_value(
        color: ColorWhen,
        format_flag: Option<OutputFormat>,
    ) -> OutCtx {
        Self::build(
            color,
            format_flag,
            format::resolve_single_value(format_flag),
        )
    }

    /// Shared construction: the resolved `format` is decided by the caller; the
    /// rest (destination, palette, terminal geometry) is identical.
    fn build(color: ColorWhen, format_flag: Option<OutputFormat>, format: OutputFormat) -> OutCtx {
        let is_tty = io::stdout().is_terminal();
        let (term_width, term_height) = terminal_size::terminal_size()
            .map(|(w, h)| (w.0 as usize, h.0 as usize))
            .unwrap_or((80, 24));
        OutCtx {
            format,
            raw_format: format_flag,
            palette: palette::resolve_with_tty(color, is_tty),
            term_width,
            term_height,
            is_tty,
        }
    }
}

/// Whether stdout is a terminal, sampled once for format resolution.
fn is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Emits a list of records in the resolved format under a collective noun.
pub(crate) fn emit_records(out: &OutCtx, records: &[Record], noun: &str) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => {
            record::render_records(&mut w, &out.palette, records, noun, out.term_width)
        }
        OutputFormat::Json => record::render_json(&mut w, records),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, records),
    };
    finish_write(res)
}

/// Emits a single command result: a human line in the records form, or a
/// single JSON object in the machine forms.
pub(crate) fn emit_action(out: &OutCtx, human: &str, record: &Record) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => writeln!(w, "{human}"),
        OutputFormat::Json | OutputFormat::Jsonl => {
            record::write_json_object(&mut w, record).and_then(|()| w.write_all(b"\n"))
        }
    };
    finish_write(res)
}

/// Emits raw bytes to stdout, paging through the resolved pager when they
/// overflow a terminal, and passing them through untouched when piped. Used for
/// the raw unified diff, which bypasses record shaping entirely.
pub(crate) fn emit_raw_paged(out: &OutCtx, buf: &[u8], context: &str) -> CmdResult {
    let line_count = buf.iter().filter(|b| **b == b'\n').count();
    let res = if should_page(
        line_count,
        /* no_pager */ false,
        out.is_tty,
        out.term_height,
    ) {
        let mut stderr = io::stderr();
        let mut stdout = io::stdout().lock();
        spawn_pager_or_passthrough(buf, &mut stdout, &mut stderr, context)
    } else {
        io::stdout().lock().write_all(buf)
    };
    finish_write(res)
}

/// Folds a write result into a [`CmdResult`], treating a broken pipe (the
/// reader went away, e.g. `| head`) as success rather than an error.
pub(crate) fn finish_write(res: io::Result<()>) -> CmdResult {
    match res {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(CmdError::err(format!("writing output: {e}"))),
    }
}

/// Maps a [`CmdResult`] to a process exit code, printing any error to stderr.
pub(crate) fn finish(result: CmdResult) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("vard: {}", err.message);
            std::process::ExitCode::from(err.code)
        }
    }
}
