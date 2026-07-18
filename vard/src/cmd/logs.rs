//! `vard logs [-f] [-n N]` — read the daemon's rotated logfile set.
//!
//! The daemon (`vard run`) writes a daily-rolling logfile set under
//! `<state_dir>/logs` (`vard.log.YYYY-MM-DD`); this command reads it. No watch
//! argument: one daemon writes one log for every watch it supervises.
//!
//! Like [`diff`](super::diff), the output is a raw text artifact — paged on a TTY
//! when not following, passed through untouched when piped, and an explicit
//! `--format json`/`jsonl` is rejected. `-n` spans rotation boundaries (reading
//! the previous day's file when the newest holds fewer than N lines) and `-f`
//! follows the live file, switching to the next day's file on rotation.
//!
//! The filesystem reads are factored into small `*_in`/dir-taking helpers
//! ([`list_logfiles`], [`tail_lines`], [`poll_follow`]) so the cross-file
//! assembly and rotation-aware follow logic are unit-tested against a tempdir of
//! synthetic rotated files, matching the crate's injected-paths pattern.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use super::{CmdError, CmdResult, OutCtx, emit_raw_paged};
use crate::cli::{ColorWhen, LogsArgs, OutputFormat};
use crate::paths;

/// Whether `name` is one of the daemon's rotated logfiles: the shared
/// [`daemon::LOG_FILE_PREFIX`](crate::daemon::LOG_FILE_PREFIX) base followed by a
/// `.` — the dotted form the DAILY appender writes (`vard.log.YYYY-MM-DD`).
/// Deriving the dot here (rather than repeating a `"vard.log."` literal) keeps
/// the writer and reader on one source of truth, and requiring the dot means the
/// match never picks up an unrelated `vard.logsomething`.
fn is_logfile_name(name: &str) -> bool {
    name.strip_prefix(crate::daemon::LOG_FILE_PREFIX)
        .is_some_and(|rest| rest.starts_with('.'))
}

/// How often `--follow` polls the live logfile for appended bytes and rotation.
const FOLLOW_POLL: Duration = Duration::from_millis(400);

/// Entry point for `vard logs`.
pub(crate) fn run(args: LogsArgs, color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    super::finish(run_inner(args, color, format))
}

fn run_inner(args: LogsArgs, color: ColorWhen, format: Option<OutputFormat>) -> CmdResult {
    let out = OutCtx::resolve(color, format);

    // Like diff, logs is a raw text artifact; reject an explicit machine format.
    // The piped auto-default still resolves to plain log text.
    if matches!(
        out.raw_format,
        Some(OutputFormat::Json) | Some(OutputFormat::Jsonl)
    ) {
        return Err(CmdError::err(
            "logs emits the daemon's raw log text and is text-only; --format json/jsonl is not \
             supported — pipe it to a file or to `grep`/`less` instead",
        ));
    }

    let log_dir = paths::log_dir().map_err(|e| CmdError::err(e.to_string()))?;
    let files = list_logfiles(&log_dir)
        .map_err(|e| CmdError::err(format!("reading log directory {}: {e}", log_dir.display())))?;

    if files.is_empty() {
        // A clean "nothing to show yet" rather than a crash or silent success.
        return Err(CmdError::attention(format!(
            "no daemon logfile yet under {}; the vard daemon writes one only while running \
             (`vard run`), so this is expected if it has not run since file logging landed",
            log_dir.display()
        )));
    }

    let tail =
        tail_lines(&files, args.lines).map_err(|e| CmdError::err(format!("reading logs: {e}")))?;

    if args.follow {
        // `-f` streams straight to stdout — no pager — and runs until interrupted.
        follow(&log_dir, &files, &tail)
    } else {
        emit_raw_paged(&out, &tail, "vard logs")
    }
}

/// Lists the daemon's logfiles under `dir`, oldest first. The filenames embed an
/// ISO date (`vard.log.YYYY-MM-DD`), so a lexical sort is chronological. A
/// missing directory yields an empty list (the daemon has not run yet).
fn list_logfiles(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if is_logfile_name(&entry.file_name().to_string_lossy()) {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// The newest logfile under `dir`, or `None` when there are none.
fn newest_logfile(dir: &Path) -> io::Result<Option<PathBuf>> {
    Ok(list_logfiles(dir)?.pop())
}

/// Returns the last `n` lines across `files` (ordered oldest→newest) as raw bytes
/// with a trailing newline per line. Spans rotation boundaries: it walks the
/// files newest-first, taking lines from the end of each, so if the newest file
/// holds fewer than `n` lines the previous file(s) fill the remainder.
fn tail_lines(files: &[PathBuf], n: usize) -> io::Result<Vec<u8>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    // `n` is raw user input; cap the pre-allocation so a huge `-n` cannot
    // reserve unbounded memory (or overflow capacity) up front — the deque
    // still grows to the real line count on demand.
    let mut lines: VecDeque<Vec<u8>> = VecDeque::with_capacity(n.min(4096));
    'outer: for path in files.iter().rev() {
        let content = std::fs::read(path)?;
        let mut segs: Vec<&[u8]> = content.split(|b| *b == b'\n').collect();
        // A trailing newline leaves an empty final segment; drop it so the last
        // real line is not shadowed by a phantom blank one.
        if segs.last().is_some_and(|s| s.is_empty()) {
            segs.pop();
        }
        for seg in segs.iter().rev() {
            lines.push_front(seg.to_vec());
            if lines.len() >= n {
                break 'outer;
            }
        }
    }
    let mut out = Vec::new();
    for line in &lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    Ok(out)
}

/// Where `--follow` is currently reading from: the followed file and the byte
/// offset already emitted.
struct Follow {
    path: PathBuf,
    offset: u64,
}

/// One follow step: emit any bytes appended to the current file since the last
/// poll, then — if the daemon has rotated to a newer file — switch to it and
/// drain it from the start so no early lines are lost. Returns the bytes to
/// write. Testable against a tempdir: append to a file and it returns the new
/// bytes; add a lexically-newer file and it switches and returns its content.
fn poll_follow(dir: &Path, state: &mut Follow) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    append_from_offset(&state.path, &mut state.offset, &mut out)?;
    if let Some(newest) = newest_logfile(dir)?
        && newest != state.path
    {
        state.path = newest;
        state.offset = 0;
        append_from_offset(&state.path, &mut state.offset, &mut out)?;
    }
    Ok(out)
}

/// Appends `path`'s bytes from `*offset` to EOF onto `out`, advancing `*offset`.
/// A file that has shrunk since the last read (truncated or replaced) is re-read
/// from the top. A not-yet-existing file is a no-op (the daemon may not have
/// created the new day's file yet).
fn append_from_offset(path: &Path, offset: &mut u64, out: &mut Vec<u8>) -> io::Result<()> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    if len < *offset {
        *offset = 0;
    }
    f.seek(SeekFrom::Start(*offset))?;
    let read = f.read_to_end(out)?;
    *offset += read as u64;
    Ok(())
}

/// Follows the live log: emit the initial tail, then poll for appended lines,
/// switching to the next day's file on rotation. Runs until interrupted or the
/// reader goes away (a broken pipe, e.g. `| head`, is a clean stop).
fn follow(dir: &Path, files: &[PathBuf], initial_tail: &[u8]) -> CmdResult {
    let mut stdout = io::stdout().lock();
    if !write_flush(&mut stdout, initial_tail)? {
        return Ok(());
    }

    // Follow from the current end of the newest file, so only lines written after
    // the tail are streamed (the tail already covered everything up to here).
    let newest = files.last().cloned().expect("files is non-empty");
    let offset = std::fs::metadata(&newest).map(|m| m.len()).unwrap_or(0);
    let mut state = Follow {
        path: newest,
        offset,
    };

    loop {
        std::thread::sleep(FOLLOW_POLL);
        let bytes = poll_follow(dir, &mut state)
            .map_err(|e| CmdError::err(format!("following logs: {e}")))?;
        if !bytes.is_empty() && !write_flush(&mut stdout, &bytes)? {
            return Ok(());
        }
    }
}

/// Writes `buf` and flushes. Returns `Ok(true)` to keep going, `Ok(false)` when
/// the reader closed the pipe (a clean stop), and `Err` on any other write error.
fn write_flush(w: &mut impl Write, buf: &[u8]) -> Result<bool, CmdError> {
    match w.write_all(buf).and_then(|()| w.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(CmdError::err(format!("writing logs: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn list_logfiles_missing_dir_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let files = list_logfiles(&root.path().join("nope")).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn list_logfiles_orders_by_date_and_ignores_others() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        write(dir, "vard.log.2026-07-17", "a\n");
        write(dir, "vard.log.2026-07-18", "b\n");
        write(dir, "vard.log.2026-07-16", "c\n");
        // Non-logfiles must be skipped.
        write(dir, "notes.txt", "x\n");
        write(dir, "vard.log", "no-date\n"); // no trailing-dot date suffix

        let files = list_logfiles(dir).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "vard.log.2026-07-16",
                "vard.log.2026-07-17",
                "vard.log.2026-07-18",
            ]
        );
    }

    #[test]
    fn tail_within_a_single_file() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let f = write(dir, "vard.log.2026-07-18", "l1\nl2\nl3\nl4\nl5\n");
        let out = tail_lines(&[f], 3).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "l3\nl4\nl5\n");
    }

    #[test]
    fn tail_spans_rotation_boundary() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        // Oldest→newest ordering is what `list_logfiles` returns and what
        // `tail_lines` expects. The newest file has only 2 lines, so a request
        // for 4 must reach back into the previous day's file.
        let old = write(dir, "vard.log.2026-07-17", "a1\na2\na3\n");
        let new = write(dir, "vard.log.2026-07-18", "b1\nb2\n");
        let out = tail_lines(&[old, new], 4).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "a2\na3\nb1\nb2\n");
    }

    #[test]
    fn tail_huge_n_does_not_overallocate_or_panic() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let f = write(dir, "vard.log.2026-07-18", "l1\nl2\n");
        // A pathological -n must not pre-reserve `n` slots (capacity overflow
        // panics at usize::MAX; huge-but-valid values reserve absurd memory).
        let out = tail_lines(&[f], usize::MAX).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "l1\nl2\n");
    }

    #[test]
    fn tail_more_than_available_returns_all() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let f = write(dir, "vard.log.2026-07-18", "only\none\n");
        let out = tail_lines(&[f], 50).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "only\none\n");
    }

    #[test]
    fn tail_zero_lines_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let f = write(dir, "vard.log.2026-07-18", "x\ny\n");
        assert!(tail_lines(&[f], 0).unwrap().is_empty());
    }

    #[test]
    fn poll_follow_streams_appended_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let f = write(dir, "vard.log.2026-07-18", "first\n");
        let mut state = Follow {
            path: f.clone(),
            offset: fs::metadata(&f).unwrap().len(),
        };
        // No new bytes yet.
        assert!(poll_follow(dir, &mut state).unwrap().is_empty());
        // Append and confirm only the new bytes come back.
        fs::write(&f, "first\nsecond\n").unwrap();
        let out = poll_follow(dir, &mut state).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "second\n");
    }

    #[test]
    fn poll_follow_switches_to_the_new_file_on_rotation() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let day1 = write(dir, "vard.log.2026-07-18", "d1-a\n");
        let mut state = Follow {
            path: day1.clone(),
            offset: fs::metadata(&day1).unwrap().len(),
        };
        // Rotation: a lexically-newer file appears with fresh content.
        write(dir, "vard.log.2026-07-19", "d2-a\nd2-b\n");
        let out = poll_follow(dir, &mut state).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "d2-a\nd2-b\n");
        assert_eq!(state.path.file_name().unwrap(), "vard.log.2026-07-19");
        // A further poll with no changes returns nothing.
        assert!(poll_follow(dir, &mut state).unwrap().is_empty());
    }

    #[test]
    fn poll_follow_drains_old_tail_before_switching() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let day1 = write(dir, "vard.log.2026-07-18", "d1-a\n");
        let mut state = Follow {
            path: day1.clone(),
            offset: fs::metadata(&day1).unwrap().len(),
        };
        // The old file gets one more line AND a newer file appears before the
        // next poll: the straggler line must not be dropped by the switch.
        fs::write(&day1, "d1-a\nd1-b\n").unwrap();
        write(dir, "vard.log.2026-07-19", "d2-a\n");
        let out = poll_follow(dir, &mut state).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "d1-b\nd2-a\n");
    }
}
