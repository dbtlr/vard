//! `vard doctor` — a read-only environment diagnosis (VRD-23).
//!
//! Doctor runs a small registry of independent checks and renders one row per
//! check (a per-watch check may emit one row per watch). It is a diagnostic:
//! it **never mutates anything** — it reads `/proc`, the config, the health
//! file, and the request dir, and reports what it finds. The request-dir check,
//! for instance, *flags* stale leftovers; it never deletes them.
//!
//! # Shape
//!
//! Each check is a `fn(&Ctx) -> Vec<CheckRow>` in [`CHECKS`]. A check gathers its
//! own inputs (shelling out to git, reading `/proc`, statting a directory) and
//! hands them to a pure `evaluate_*` function that decides the [`Status`]. The
//! evaluators take already-gathered values so they unit-test without touching a
//! real `/proc`, a real daemon, or a real XDG dir.
//!
//! # Exit code
//!
//! Every `ok`/`skipped` row contributes 0; any `warn` or `fail` contributes 1
//! (attention). Doctor being *unable to run at all* (an unresolvable state dir,
//! an invalid config) is the operational error 2, surfaced through the shared
//! [`CmdError`] layer. The per-row codes fold with [`CmdError::worse`].
//!
//! # Scope (this checkpoint)
//!
//! Local checks only: git presence/version, inotify limits vs watched-tree size
//! (Linux), health-file freshness, request-dir hygiene, and a per-watch secret
//! audit. The remote-auth probe and an `--offline` flag are a later checkpoint;
//! agent/keychain and linger checks are deferred to service-install (VRD-24).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, UNIX_EPOCH};

use anstyle::Style;

use crate::cli::{ColorWhen, OutputFormat};
use crate::command::{self, CmdError, CmdResult, OutCtx};
use crate::config::{Config, ResolvedWatch};
use crate::health::{self, HealthReport};
use crate::output::glyphs::{self, Glyph};
use crate::output::palette::Palette;
use crate::output::primitives::clean_line;
use crate::output::record::{self, Record, RecordField, format_duration};
use crate::paths;
use crate::request;

/// The check registry, run in order. Each returns one or more [`CheckRow`]s.
const CHECKS: &[fn(&Ctx) -> Vec<CheckRow>] =
    &[check_git, check_inotify, check_health, check_request_dir];

/// A single check's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Nothing wrong.
    Ok,
    /// A soft problem worth a human's attention (folds exit 1).
    Warn,
    /// A hard problem (folds exit 1).
    Fail,
    /// The check does not apply here (e.g. inotify on macOS). Exit 0.
    Skipped,
}

impl Status {
    /// The stable machine token stored in the `status` field.
    fn token(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Skipped => "skipped",
        }
    }

    /// This status's contribution to the process exit code: `ok`/`skipped` are
    /// healthy (0); `warn` and `fail` both need attention (1).
    fn exit_code(self) -> u8 {
        match self {
            Status::Ok | Status::Skipped => 0,
            Status::Warn | Status::Fail => 1,
        }
    }

    /// The glyph the human form prints for this status.
    fn glyph(self) -> Glyph {
        match self {
            Status::Ok => Glyph::Pass,
            Status::Warn => Glyph::Warn,
            Status::Fail => Glyph::Err,
            Status::Skipped => Glyph::Sep,
        }
    }
}

/// One rendered check result.
struct CheckRow {
    /// The check's stable name (e.g. `git`, `inotify`). A per-watch check names
    /// the watch alongside, in `detail`.
    name: String,
    /// The check's status.
    status: Status,
    /// A human-readable one-line explanation with any action guidance.
    detail: String,
}

/// Builds a [`CheckRow`].
fn row(name: &str, status: Status, detail: impl Into<String>) -> CheckRow {
    CheckRow {
        name: name.to_string(),
        status,
        detail: detail.into(),
    }
}

/// Everything the checks read, gathered once. The per-watch checks iterate
/// [`watches`](Ctx::watches); the rest read the resolved state-dir paths.
struct Ctx {
    /// Every configured watch, paused ones included (config order). Read by the
    /// Linux inotify check (and, in a later checkpoint, the per-watch secret
    /// audit on every platform); unused on a non-Linux build until then.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    watches: Vec<ResolvedWatch>,
    /// The single-instance lock file (for the daemon-liveness probe).
    lock_file: PathBuf,
    /// The daemon's health document.
    health_file: PathBuf,
    /// The request-file queue directory.
    request_dir: PathBuf,
    /// Unix seconds sampled once, so every age the checks report is consistent.
    now: u64,
}

impl Ctx {
    /// Resolves the XDG paths and the watch list. A missing config is an empty
    /// watch list; a present-but-invalid config is the operational error 2
    /// (doctor cannot enumerate what it would diagnose).
    fn gather() -> Result<Ctx, CmdError> {
        let lock_file = paths::lock_file().map_err(|e| CmdError::err(e.to_string()))?;
        let health_file = paths::health_file().map_err(|e| CmdError::err(e.to_string()))?;
        let request_dir = paths::request_dir().map_err(|e| CmdError::err(e.to_string()))?;
        let config_file = paths::config_file().map_err(|e| CmdError::err(e.to_string()))?;
        let config =
            Config::load_optional(&config_file).map_err(|e| CmdError::err(e.to_string()))?;
        let watches = match &config {
            Some(cfg) => cfg
                .resolve_all()
                .map_err(|e| CmdError::err(e.to_string()))?,
            None => Vec::new(),
        };
        Ok(Ctx {
            watches,
            lock_file,
            health_file,
            request_dir,
            now: health::now_secs(),
        })
    }
}

/// Entry point for `vard doctor`. Returns the folded attention exit code (0 all
/// clear, 1 something needs attention), or 2 when doctor could not run at all.
pub(crate) fn run(color: ColorWhen, format: Option<OutputFormat>) -> ExitCode {
    match run_inner(color, format) {
        Ok(code) => ExitCode::from(code),
        Err(err) => command::finish(Err(err)),
    }
}

fn run_inner(color: ColorWhen, format: Option<OutputFormat>) -> Result<u8, CmdError> {
    let out = OutCtx::resolve(color, format);
    let ctx = Ctx::gather()?;
    let rows: Vec<CheckRow> = CHECKS.iter().flat_map(|check| check(&ctx)).collect();
    let code = rows
        .iter()
        .fold(0u8, |acc, r| CmdError::worse(acc, r.status.exit_code()));
    render(&out, &rows)?;
    Ok(code)
}

// --- check 1: git ---------------------------------------------------------

/// The conservative minimum git version vard's backend needs.
///
/// The floor is set by the snapshot-log format string in
/// `vard-core/src/vcs/git.rs` (`GitBackend::log`):
/// `--format=…%(trailers:key=Vard-Trigger,valueonly)…`. Both the `key=` filter
/// and the `valueonly` modifier on `%(trailers)` were introduced in git 2.22.0
/// (2019). Below it, the trigger trailer cannot be read back out of history, so
/// `vard history` loses every snapshot's trigger. Every other git feature the
/// backend uses predates 2.22 — `:(exclude,literal)` pathspec magic (1.9),
/// `rev-parse --absolute-git-dir` (2.13), `worktree remove` (2.17) — so 2.22 is
/// the binding floor.
const MIN_GIT: (u32, u32, u32) = (2, 22, 0);

/// What probing `git --version` found.
enum GitProbe {
    /// `git` could not be spawned — not on PATH.
    Missing,
    /// `git` ran but its version string could not be parsed.
    Unparsed(String),
    /// `git` ran and reported a parseable version.
    Found {
        /// The parsed `(major, minor, patch)`.
        version: (u32, u32, u32),
    },
}

fn check_git(_ctx: &Ctx) -> Vec<CheckRow> {
    vec![evaluate_git(&probe_git())]
}

/// Runs `git --version` and classifies the result. A spawn failure means git is
/// not on PATH; otherwise the first `git version X.Y.Z` token is parsed.
fn probe_git() -> GitProbe {
    match Command::new("git").arg("--version").output() {
        Err(_) => GitProbe::Missing,
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            match parse_git_version(text.trim()) {
                Some(version) => GitProbe::Found { version },
                None => GitProbe::Unparsed(text.trim().to_string()),
            }
        }
    }
}

/// Parses `git version X.Y.Z…` into `(major, minor, patch)`. Trailing vendor
/// suffixes (`2.39.3 (Apple Git-146)`) are ignored; a missing minor/patch reads
/// as 0.
fn parse_git_version(text: &str) -> Option<(u32, u32, u32)> {
    let rest = text.strip_prefix("git version ")?;
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut fields = token.split('.').filter(|s| !s.is_empty());
    let major = fields.next()?.parse().ok()?;
    let minor = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Renders `a.b.c`.
fn fmt_ver(v: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

fn evaluate_git(probe: &GitProbe) -> CheckRow {
    match probe {
        GitProbe::Missing => row(
            "git",
            Status::Fail,
            "the git executable could not be found on PATH; vard cannot snapshot without it — \
             install git",
        ),
        GitProbe::Unparsed(text) => row(
            "git",
            Status::Warn,
            format!(
                "git is present but its version could not be parsed from {text:?}; vard needs \
                 at least {}",
                fmt_ver(MIN_GIT)
            ),
        ),
        GitProbe::Found { version } if *version < MIN_GIT => row(
            "git",
            Status::Warn,
            format!(
                "git {} is older than the {} vard needs — the snapshot-log trigger trailer \
                 (`%(trailers:key=…,valueonly)`) requires git 2.22+; upgrade git",
                fmt_ver(*version),
                fmt_ver(MIN_GIT)
            ),
        ),
        GitProbe::Found { version } => row(
            "git",
            Status::Ok,
            format!(
                "git {} ({} or newer required)",
                fmt_ver(*version),
                fmt_ver(MIN_GIT)
            ),
        ),
    }
}

// --- check 2: inotify (Linux) ---------------------------------------------

/// Warn when the watched trees would consume this fraction (or more) of a limit.
const INOTIFY_WARN_FRACTION: f64 = 0.8;

/// The percent form of [`INOTIFY_WARN_FRACTION`], for the detail text.
const INOTIFY_WARN_PCT: u64 = 80;

#[cfg(not(target_os = "linux"))]
fn check_inotify(_ctx: &Ctx) -> Vec<CheckRow> {
    vec![row(
        "inotify",
        Status::Skipped,
        "not applicable on this platform — vard uses FSEvents here, which has no per-user \
         watch-descriptor limit to exhaust",
    )]
}

#[cfg(target_os = "linux")]
fn check_inotify(ctx: &Ctx) -> Vec<CheckRow> {
    let max_watches = read_proc_u64("/proc/sys/fs/inotify/max_user_watches");
    let max_instances = read_proc_u64("/proc/sys/fs/inotify/max_user_instances");
    let (max_watches, max_instances) = match (max_watches, max_instances) {
        (Some(w), Some(i)) => (w, i),
        _ => {
            return vec![row(
                "inotify",
                Status::Warn,
                "could not read /proc/sys/fs/inotify/{max_user_watches,max_user_instances}; \
                 cannot check the kernel limits against the watched trees",
            )];
        }
    };
    // The notify backend watches every directory in a tree recursively, so each
    // directory costs one watch descriptor; each watch root is one inotify
    // instance.
    let total_dirs: u64 = ctx.watches.iter().map(|w| count_dirs(w.spec.path())).sum();
    vec![evaluate_inotify(
        max_watches,
        max_instances,
        total_dirs,
        ctx.watches.len(),
    )]
}

/// Reads a single-integer `/proc` file, or `None` when it is absent/unreadable.
#[cfg(target_os = "linux")]
fn read_proc_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Counts the directories in the tree rooted at `root` (root included), which is
/// one inotify watch descriptor each under the notify backend's recursive watch.
/// Symlinked directories are not descended (their `DirEntry` file type is a
/// symlink, not a dir), so a symlink cycle cannot make this loop forever, and an
/// unreadable subtree is simply not counted rather than erroring.
#[cfg(target_os = "linux")]
fn count_dirs(root: &Path) -> u64 {
    let mut count = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            _ => continue,
        }
        count += 1;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(entry.path());
                }
            }
        }
    }
    count
}

/// Decides the inotify check from injected limits and tree sizes, so the
/// comparison logic is testable on every platform (macOS included). Warns when
/// the watched directories reach [`INOTIFY_WARN_FRACTION`] of
/// `max_user_watches`, or the watch count reaches that fraction of
/// `max_user_instances`. With no watches configured there is nothing to exhaust.
//
// Compiled on every platform (and unit-tested on macOS), but only *called* from
// the Linux `check_inotify`; elsewhere only the tests reach it, so a non-Linux
// release build would otherwise flag it dead.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evaluate_inotify(
    max_watches: u64,
    max_instances: u64,
    total_dirs: u64,
    num_watches: usize,
) -> CheckRow {
    let num_watches = num_watches as u64;
    if num_watches == 0 {
        return row(
            "inotify",
            Status::Ok,
            format!(
                "no directories are being watched, so the kernel inotify limits \
                 (max_user_watches={max_watches}, max_user_instances={max_instances}) are not a \
                 concern"
            ),
        );
    }
    let watch_threshold = (max_watches as f64 * INOTIFY_WARN_FRACTION) as u64;
    let instance_threshold = (max_instances as f64 * INOTIFY_WARN_FRACTION) as u64;
    let detail = format!(
        "{total_dirs} watched directories across {num_watches} watch(es) vs \
         max_user_watches={max_watches}, max_user_instances={max_instances} \
         (warns at {INOTIFY_WARN_PCT}% of either limit)"
    );
    if total_dirs >= watch_threshold || num_watches >= instance_threshold {
        row(
            "inotify",
            Status::Warn,
            format!(
                "{detail}; raise the limits before they exhaust — \
                 `sysctl fs.inotify.max_user_watches` / `max_user_instances`"
            ),
        )
    } else {
        row("inotify", Status::Ok, detail)
    }
}

// --- check 3: health-file freshness ---------------------------------------

fn check_health(ctx: &Ctx) -> Vec<CheckRow> {
    match health::collect(&ctx.lock_file, &ctx.health_file) {
        Ok(report) => vec![evaluate_health(&report, ctx.now)],
        // An unreadable / unsupported health document is a soft finding for a
        // diagnostic, not a reason to abort the whole run.
        Err(e) => vec![row(
            "health-file",
            Status::Warn,
            format!("could not read the daemon health file: {e}"),
        )],
    }
}

/// Renders `secs` as a compact humane duration (`2h`, `15m`).
fn fmt_dur(secs: u64) -> String {
    format_duration(Duration::from_secs(secs))
}

/// Decides the health-file check from an already-collected [`HealthReport`], so
/// it tests without a live daemon. A running daemon with a fresh document is
/// `ok`; a stale document is `warn`; a not-running (or starting) daemon is an
/// `ok`-with-note — not running is a legitimate state, never a doctor failure.
fn evaluate_health(report: &HealthReport, now: u64) -> CheckRow {
    match report {
        HealthReport::Running {
            written_at,
            problems,
            ..
        } => {
            let age = now.saturating_sub(*written_at);
            if age > health::STALE_AFTER_SECS {
                row(
                    "health-file",
                    Status::Warn,
                    format!(
                        "the daemon is running but its health file is stale ({} old, past the {} \
                         staleness window) — it may be wedged or unable to write; check \
                         `vard status` and `vard logs`",
                        fmt_dur(age),
                        fmt_dur(health::STALE_AFTER_SECS)
                    ),
                )
            } else {
                let watches = if problems.is_empty() {
                    "no watches currently need attention".to_string()
                } else {
                    format!(
                        "{} watch(es) currently need attention — see `vard status`",
                        problems.len()
                    )
                };
                row(
                    "health-file",
                    Status::Ok,
                    format!(
                        "the daemon is running and its health file is fresh ({} old); {watches}",
                        fmt_dur(age)
                    ),
                )
            }
        }
        HealthReport::Starting => row(
            "health-file",
            Status::Ok,
            "the daemon is starting or stopping; its health file is not yet available (a \
             transient state, not a failure)",
        ),
        HealthReport::NotRunning { .. } => row(
            "health-file",
            Status::Ok,
            "no daemon is running — a legitimate state; start one with `vard run` to watch your \
             directories",
        ),
    }
}

// --- check 4: request-dir hygiene -----------------------------------------

fn check_request_dir(ctx: &Ctx) -> Vec<CheckRow> {
    let leftovers = scan_stale_leftovers(&ctx.request_dir, ctx.now);
    vec![evaluate_request_dir(&leftovers)]
}

/// Scans the request dir for crashed-writer leftovers: entries whose name is
/// **not** a settled request ([`request::is_settled_request_name`]) — a temp or
/// dotfile from an interrupted atomic write — and whose mtime is older than
/// [`request::STALE_AFTER`]. A settled `*.toml` is a real queued request the
/// daemon owns; a *fresh* unsettled file is a writer mid-flight. Neither is
/// flagged. A missing dir yields nothing. Returns the names, sorted.
fn scan_stale_leftovers(dir: &Path, now: u64) -> Vec<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut leftovers = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if request::is_settled_request_name(&name) {
            continue;
        }
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_secs()));
        if age.is_some_and(|age| age > request::STALE_AFTER.as_secs()) {
            leftovers.push(name);
        }
    }
    leftovers.sort();
    leftovers
}

/// Decides the request-dir check from the stale-leftover names, so it tests with
/// injected values.
fn evaluate_request_dir(leftovers: &[String]) -> CheckRow {
    if leftovers.is_empty() {
        row(
            "request-dir",
            Status::Ok,
            "no stale leftovers in the request queue",
        )
    } else {
        row(
            "request-dir",
            Status::Warn,
            format!(
                "{} stale file(s) in the request queue, left by a crashed writer; safe to \
                 delete: {}",
                leftovers.len(),
                leftovers.join(", ")
            ),
        )
    }
}

// --- rendering ------------------------------------------------------------

/// Renders the checks in the resolved format: glyph lines on a terminal (the
/// visual register of `vard status`), a stable JSON/JSONL array when piped.
fn render(out: &OutCtx, rows: &[CheckRow]) -> CmdResult {
    let mut w = io::stdout().lock();
    let res = match out.format {
        OutputFormat::Records => render_human(&mut w, rows, &out.palette),
        OutputFormat::Json => record::render_json(&mut w, &records(rows)),
        OutputFormat::Jsonl => record::render_jsonl(&mut w, &records(rows)),
    };
    command::finish_write(res)
}

/// The human form: one status-colored glyph line per check.
fn render_human(w: &mut dyn Write, rows: &[CheckRow], palette: &Palette) -> io::Result<()> {
    let ascii = glyphs::use_ascii();
    for row in rows {
        let style = match row.status {
            Status::Ok => &palette.success,
            Status::Warn => &palette.warning,
            Status::Fail => &palette.error,
            Status::Skipped => &palette.dim,
        };
        writeln!(
            w,
            "{} {}: {} — {}",
            paint(glyphs::render(row.status.glyph(), ascii), style),
            clean_line(&row.name),
            clean_line(row.status.token()),
            clean_line(&row.detail)
        )?;
    }
    Ok(())
}

/// The machine records: one per check, a stable `{check, status, detail}` shape.
fn records(rows: &[CheckRow]) -> Vec<Record> {
    rows.iter()
        .map(|r| Record {
            header: None,
            fields: vec![
                RecordField::str("check", &r.name),
                RecordField::str("status", r.status.token()),
                RecordField::str("detail", &r.detail),
            ],
        })
        .collect()
}

/// Wraps `text` in a style's SGR codes (a no-op when color is off).
fn paint(text: &str, style: &Style) -> String {
    format!("{}{text}{}", style.render(), style.render_reset())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- git ----------------------------------------------------------------

    #[test]
    fn git_version_parses_plain_and_vendor_suffixed() {
        assert_eq!(parse_git_version("git version 2.55.0"), Some((2, 55, 0)));
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
        // A two-component version reads its patch as 0.
        assert_eq!(parse_git_version("git version 2.22"), Some((2, 22, 0)));
        assert_eq!(parse_git_version("totally not git"), None);
    }

    #[test]
    fn git_missing_is_a_fail() {
        let r = evaluate_git(&GitProbe::Missing);
        assert_eq!(r.status, Status::Fail);
        assert!(r.detail.contains("PATH"), "got: {}", r.detail);
    }

    #[test]
    fn git_below_the_floor_warns_and_names_the_feature() {
        let r = evaluate_git(&GitProbe::Found {
            version: (2, 20, 0),
        });
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("2.20.0") && r.detail.contains("2.22.0"));
        assert!(
            r.detail.contains("trailers"),
            "cites the floor: {}",
            r.detail
        );
    }

    #[test]
    fn git_at_or_above_the_floor_is_ok() {
        assert_eq!(
            evaluate_git(&GitProbe::Found { version: MIN_GIT }).status,
            Status::Ok
        );
        assert_eq!(
            evaluate_git(&GitProbe::Found {
                version: (2, 55, 0)
            })
            .status,
            Status::Ok
        );
    }

    #[test]
    fn git_unparsed_warns() {
        let r = evaluate_git(&GitProbe::Unparsed("weird".to_string()));
        assert_eq!(r.status, Status::Warn);
    }

    // --- inotify (comparison logic tested on every platform) ----------------

    #[test]
    fn inotify_comfortably_under_the_limits_is_ok() {
        // 100 dirs, 1 watch against generous limits: nowhere near 80%.
        let r = evaluate_inotify(8192, 128, 100, 1);
        assert_eq!(r.status, Status::Ok);
        assert!(
            r.detail.contains("100 watched directories"),
            "got: {}",
            r.detail
        );
    }

    #[test]
    fn inotify_dirs_approaching_max_user_watches_warns() {
        // 900 dirs vs a max of 1000 → 90%, past the 80% threshold.
        let r = evaluate_inotify(1000, 128, 900, 1);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("max_user_watches"), "got: {}", r.detail);
    }

    #[test]
    fn inotify_watch_count_approaching_max_instances_warns() {
        // Few dirs, but the watch count itself reaches 80% of the instance cap.
        let r = evaluate_inotify(100_000, 10, 50, 9);
        assert_eq!(r.status, Status::Warn);
    }

    #[test]
    fn inotify_with_no_watches_is_ok() {
        let r = evaluate_inotify(1000, 128, 0, 0);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("no directories"), "got: {}", r.detail);
    }

    // --- health -------------------------------------------------------------

    fn running(written_at: u64, problems: usize) -> HealthReport {
        HealthReport::Running {
            problems: (0..problems)
                .map(|i| crate::health::HealthProblem {
                    watch: format!("w{i}"),
                    state: "attention".to_string(),
                    kind: "attention".to_string(),
                    summary: "x".to_string(),
                    since: 0,
                })
                .collect(),
            suppressions: Vec::new(),
            written_at,
        }
    }

    #[test]
    fn health_running_and_fresh_is_ok() {
        let r = evaluate_health(&running(1000, 0), 1000 + 5);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("fresh"), "got: {}", r.detail);
    }

    #[test]
    fn health_running_but_stale_warns() {
        let r = evaluate_health(&running(1000, 0), 1000 + health::STALE_AFTER_SECS + 1);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("stale"), "got: {}", r.detail);
    }

    #[test]
    fn health_fresh_with_troubled_watches_stays_ok_but_notes_them() {
        // Freshness is about the FILE, not the watches: a fresh file with
        // troubled watches is still an ok freshness check (it notes the count).
        let r = evaluate_health(&running(1000, 2), 1000 + 5);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("2 watch"), "got: {}", r.detail);
    }

    #[test]
    fn health_not_running_is_ok_with_a_note() {
        let r = evaluate_health(&HealthReport::NotRunning { last_write: None }, 5);
        assert_eq!(r.status, Status::Ok, "not running is not a doctor failure");
        assert!(r.detail.contains("vard run"), "got: {}", r.detail);
    }

    #[test]
    fn health_starting_is_ok() {
        assert_eq!(
            evaluate_health(&HealthReport::Starting, 5).status,
            Status::Ok
        );
    }

    // --- request-dir --------------------------------------------------------

    #[test]
    fn request_dir_clean_is_ok() {
        assert_eq!(evaluate_request_dir(&[]).status, Status::Ok);
    }

    #[test]
    fn request_dir_with_leftovers_warns_and_names_them() {
        let r = evaluate_request_dir(&[".req-123.toml.tmp".to_string()]);
        assert_eq!(r.status, Status::Warn);
        assert!(
            r.detail.contains(".req-123.toml.tmp"),
            "names it: {}",
            r.detail
        );
        assert!(r.detail.contains("safe to delete"), "hints: {}", r.detail);
    }

    #[test]
    fn scan_flags_stale_unsettled_but_not_settled_or_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000_000u64;
        let stale = now - request::STALE_AFTER.as_secs() - 60;
        let fresh = now - 5;

        // A settled request (never flagged, however old).
        std::fs::write(dir.path().join("req.toml"), "x").unwrap();
        // A stale unsettled leftover (a crashed writer's temp name): flagged.
        let leftover = dir.path().join(".req-1.toml.tmp");
        std::fs::write(&leftover, "x").unwrap();
        // A fresh unsettled file (a writer mid-flight): not flagged.
        let inflight = dir.path().join(".req-2.toml.tmp");
        std::fs::write(&inflight, "x").unwrap();

        set_mtime(&dir.path().join("req.toml"), stale);
        set_mtime(&leftover, stale);
        set_mtime(&inflight, fresh);

        let found = scan_stale_leftovers(dir.path(), now);
        assert_eq!(
            found,
            vec![".req-1.toml.tmp".to_string()],
            "only the stale unsettled leftover is flagged"
        );
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_stale_leftovers(&dir.path().join("nope"), 1_000).is_empty());
    }

    /// Sets a file's mtime to `secs` past the epoch, via a `SystemTime` on the
    /// standard `set_times` API (no external crate).
    fn set_mtime(path: &Path, secs: u64) {
        let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let times = std::fs::FileTimes::new().set_modified(t);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(times).unwrap();
    }

    // --- status / exit-code folding -----------------------------------------

    #[test]
    fn exit_code_folds_worst_of_all_rows() {
        assert_eq!(Status::Ok.exit_code(), 0);
        assert_eq!(Status::Skipped.exit_code(), 0);
        assert_eq!(Status::Warn.exit_code(), 1);
        assert_eq!(Status::Fail.exit_code(), 1);
    }
}
