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
//! # Scope
//!
//! Local checks: git presence/version, inotify limits vs watched-tree size
//! (Linux), health-file freshness, request-dir hygiene, a per-watch secret
//! audit, systemd linger (Linux), and service-context agent/keychain
//! reachability. One network check: a per-watch **remote-auth** probe (a
//! read-only `git ls-remote`), which `--offline` renders `skipped`. The linger
//! and service-agent checks are local probes too and are never gated on
//! `--offline`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, UNIX_EPOCH};

use anstyle::Style;
use vard_core::{SecretMatch, SecretScanner, VcsBackend, VcsError};

use crate::atomic;
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
use crate::service::launchd::LaunchctlPrint;

/// The check registry, run in order. Each returns one or more [`CheckRow`]s.
const CHECKS: &[fn(&Ctx) -> Vec<CheckRow>] = &[
    check_git,
    check_inotify,
    check_health,
    check_request_dir,
    check_secret_audit,
    check_remote_auth,
    check_linger,
    check_service_agent,
];

/// Wall-clock bound on each per-watch remote-auth probe, so a dead VPN or a
/// prompt-wanting remote cannot hang doctor. Each watch is probed independently
/// under this same bound (see [`check_remote_auth`]).
const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Wall-clock bound on doctor's own service-context probes (`loginctl`,
/// `systemctl --user show-environment`, `launchctl print`) — local
/// login-session queries that should answer in well under a second, bounded
/// generously against a wedged session. Shorter than [`REMOTE_PROBE_TIMEOUT`]
/// since these never touch the network.
const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The check's stable name (e.g. `git`, `inotify`).
    name: String,
    /// The watch this row is about, for a per-watch check (secret-audit,
    /// remote-auth); `None` for a global row. Carried as its own field so a
    /// machine consumer reads it directly rather than parsing it out of
    /// [`detail`](Self::detail) prose.
    watch: Option<String>,
    /// The check's status.
    status: Status,
    /// A human-readable one-line explanation with any action guidance. A
    /// per-watch row still names its watch here for the human form.
    detail: String,
}

/// Builds a global [`CheckRow`] (no associated watch).
fn row(name: &str, status: Status, detail: impl Into<String>) -> CheckRow {
    CheckRow {
        name: name.to_string(),
        watch: None,
        status,
        detail: detail.into(),
    }
}

/// Builds a per-watch [`CheckRow`], tagging it with the `watch` it is about.
fn watch_row(name: &str, watch: &str, status: Status, detail: impl Into<String>) -> CheckRow {
    CheckRow {
        name: name.to_string(),
        watch: Some(watch.to_string()),
        status,
        detail: detail.into(),
    }
}

/// Everything the checks read, gathered once. The per-watch checks iterate
/// [`watches`](Ctx::watches); the rest read the resolved state-dir paths.
struct Ctx {
    /// Every configured watch, paused ones included (config order). Read by the
    /// per-watch secret audit (every platform) and the Linux inotify check.
    watches: Vec<ResolvedWatch>,
    /// The single-instance lock file (for the daemon-liveness probe).
    lock_file: PathBuf,
    /// The daemon's health document.
    health_file: PathBuf,
    /// The request-file queue directory.
    request_dir: PathBuf,
    /// Unix seconds sampled once, so every age the checks report is consistent.
    now: u64,
    /// Whether network checks are skipped (`--offline`). When set, the
    /// remote-auth probe emits `skipped` rows instead of contacting any remote.
    offline: bool,
}

impl Ctx {
    /// Resolves the XDG paths and the watch list. A missing config is an empty
    /// watch list; a present-but-invalid config is the operational error 2
    /// (doctor cannot enumerate what it would diagnose).
    fn gather(offline: bool) -> Result<Ctx, CmdError> {
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
            offline,
        })
    }
}

/// Entry point for `vard doctor`. Returns the folded attention exit code (0 all
/// clear, 1 something needs attention), or 2 when doctor could not run at all.
pub(crate) fn run(color: ColorWhen, format: Option<OutputFormat>, offline: bool) -> ExitCode {
    match run_inner(color, format, offline) {
        Ok(code) => ExitCode::from(code),
        Err(err) => command::finish(Err(err)),
    }
}

fn run_inner(
    color: ColorWhen,
    format: Option<OutputFormat>,
    offline: bool,
) -> Result<u8, CmdError> {
    let out = OutCtx::resolve(color, format);
    let ctx = Ctx::gather(offline)?;
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
    let scan = scan_request_dir(&ctx.request_dir, ctx.now);
    vec![evaluate_request_dir(&scan)]
}

/// The kinds of stale entry the request-dir check flags — all older than
/// [`request::STALE_AFTER`] and all flag-only (doctor never deletes) — plus any
/// I/O failures reading the queue, so a directory we could not inspect never
/// masquerades as a clean one.
#[derive(Default)]
struct RequestDirScan {
    /// Temp names matching the atomic writer's scheme
    /// ([`atomic::is_temp_name`]) that an interrupted write stranded — a crashed
    /// writer's leftovers, safe to delete. Sorted.
    crashed: Vec<String>,
    /// Settled `*.toml` requests still sitting in the queue past the staleness
    /// window — requests piling up unconsumed (typically no daemon is running;
    /// the daemon would discard them as stale anyway). Sorted.
    stale_settled: Vec<String>,
    /// Stale entries vard did not write and cannot vouch for — neither a settled
    /// request nor a recognizable atomic-write temp. Flagged for investigation
    /// rather than labeled safe to delete. Sorted.
    unrecognized: Vec<String>,
    /// I/O failures encountered while scanning (an unreadable dir short of
    /// `NotFound`, or a per-entry stat that failed). Non-empty turns the row into
    /// a `warn` so a read failure is never silently reported `ok`.
    errors: Vec<String>,
}

/// Scans the request dir, sorting stale entries into the [`RequestDirScan`]
/// buckets. A settled `*.toml` ([`request::is_settled_request_name`]) older than
/// [`request::STALE_AFTER`] is a piling-up unconsumed request; a stale name
/// matching the atomic writer's temp scheme ([`atomic::is_temp_name`]) is a
/// crashed writer's leftover; any other stale name is unrecognized — vard did
/// not write it. A *fresh* entry is a writer or daemon mid-flight and is not
/// flagged. A missing dir (`NotFound`) yields an empty, clean scan; any other
/// read failure — the dir itself, an entry, or a stat — is recorded so the check
/// `warn`s instead of falsely reporting `ok`.
fn scan_request_dir(dir: &Path, now: u64) -> RequestDirScan {
    let mut scan = RequestDirScan::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing queue dir is the ordinary "no requests yet" state.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return scan,
        Err(e) => {
            scan.errors
                .push(format!("could not read the request dir: {e}"));
            return scan;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                scan.errors
                    .push(format!("could not read a request-dir entry: {e}"));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // A stat failure must not silently drop the entry — fold it into the warn.
        let age = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| now.saturating_sub(d.as_secs())),
            Err(e) => {
                scan.errors
                    .push(format!("could not stat request-dir entry {name:?}: {e}"));
                continue;
            }
        };
        if age.is_none_or(|age| age <= request::STALE_AFTER.as_secs()) {
            continue;
        }
        if request::is_settled_request_name(&name) {
            scan.stale_settled.push(name);
        } else if atomic::is_temp_name(&name) {
            scan.crashed.push(name);
        } else {
            scan.unrecognized.push(name);
        }
    }
    scan.crashed.sort();
    scan.stale_settled.sort();
    scan.unrecognized.sort();
    scan
}

/// Decides the request-dir check from a [`RequestDirScan`], so it tests with
/// injected values. Any populated bucket, or any recorded I/O error, `warn`s
/// (still flag-only); a clean scan is `ok`. The conditions carry distinct
/// wording: crashed-writer temps are safe to delete, piled-up settled requests
/// point at a daemon that is not consuming them, and unrecognized entries — which
/// vard did not write — are flagged for investigation, never called safe to
/// remove.
fn evaluate_request_dir(scan: &RequestDirScan) -> CheckRow {
    if scan.crashed.is_empty()
        && scan.stale_settled.is_empty()
        && scan.unrecognized.is_empty()
        && scan.errors.is_empty()
    {
        return row(
            "request-dir",
            Status::Ok,
            "no stale leftovers in the request queue",
        );
    }
    let mut parts = Vec::new();
    if !scan.crashed.is_empty() {
        parts.push(format!(
            "{} stale file(s) left by a crashed writer, safe to delete: {}",
            scan.crashed.len(),
            scan.crashed.join(", ")
        ));
    }
    if !scan.unrecognized.is_empty() {
        parts.push(format!(
            "{} unrecognized file(s) in the request dir — vard did not write this; investigate \
             before deleting: {}",
            scan.unrecognized.len(),
            scan.unrecognized.join(", ")
        ));
    }
    if !scan.stale_settled.is_empty() {
        parts.push(format!(
            "{} settled request(s) piling up unconsumed past the staleness window — no daemon is \
             consuming them (a running daemon would discard them as stale): {}",
            scan.stale_settled.len(),
            scan.stale_settled.join(", ")
        ));
    }
    for err in &scan.errors {
        parts.push(err.clone());
    }
    row("request-dir", Status::Warn, parts.join("; "))
}

// --- check 5: per-watch secret audit --------------------------------------

/// How many example paths a `fail` detail lists before eliding the rest.
const SECRET_EXAMPLE_CAP: usize = 5;

/// Audits every configured watch's already-tracked filenames for secret shapes
/// (VRD-22) — one row per watch. This is the complement to snapshot quarantine:
/// quarantine keeps *newly-added* secrets out of history, but a secret committed
/// before scanning was on (or force-added) is already tracked, and only this
/// audit catches it. With no watches configured there is nothing to audit.
fn check_secret_audit(ctx: &Ctx) -> Vec<CheckRow> {
    if ctx.watches.is_empty() {
        return vec![row(
            "secret-audit",
            Status::Ok,
            "no watches configured to audit",
        )];
    }
    ctx.watches.iter().map(audit_watch).collect()
}

/// Audits one watch. Opens the repository the vetted way the per-watch CLI
/// commands do ([`vard_core::open_git_backend`]), lists its tracked files, and
/// runs the **filename-only** audit built from this watch's own
/// `secret_scan`/`secret_patterns`. A disabled scanner is `skipped`; an
/// unopenable repository (or an unlistable tree, or a bad extra pattern) is a
/// `warn` that names the watch and never blocks the other watches' rows.
fn audit_watch(rw: &ResolvedWatch) -> CheckRow {
    let spec = &rw.spec;
    let name = spec.name();

    // A watch with scanning off is not audited — a deliberate opt-out, not a
    // problem. Reported `skipped`, mirroring the daemon's per-watch policy.
    if !spec.secret_scan() {
        return watch_row(
            "secret-audit",
            name,
            Status::Skipped,
            format!("watch {name:?}: secret scanning is disabled (secret_scan = false)"),
        );
    }

    // Compile the same scanner the daemon/CLI build per watch. A bad extra
    // pattern warns (it would also fail the daemon), not crashes.
    let scanner = match SecretScanner::compile(spec.secret_scan(), spec.secret_patterns()) {
        Ok(scanner) => scanner,
        Err(e) => {
            return watch_row(
                "secret-audit",
                name,
                Status::Warn,
                format!("watch {name:?}: {e}"),
            );
        }
    };

    // Open only the vetted way, matching every other per-watch command. An
    // unopenable repository is this watch's own warn — never a crash, never a
    // block on the rest.
    let backend = match vard_core::open_git_backend(spec) {
        Ok(backend) => backend,
        Err(e) => {
            return watch_row(
                "secret-audit",
                name,
                Status::Warn,
                format!(
                    "watch {name:?}: repository could not be opened ({e}); skipped — fix it and \
                     re-run"
                ),
            );
        }
    };

    let tracked = match backend.tracked_files() {
        Ok(tracked) => tracked,
        Err(e) => {
            return watch_row(
                "secret-audit",
                name,
                Status::Warn,
                format!("watch {name:?}: could not list tracked files ({e})"),
            );
        }
    };

    evaluate_secret_audit(name, &scanner.audit_tracked(&tracked))
}

/// Decides one watch's secret-audit row from the audit findings, so it tests
/// with injected [`SecretMatch`]es. No findings is `ok`; any finding is `fail`
/// with the count and up to [`SECRET_EXAMPLE_CAP`] example repo-relative paths.
fn evaluate_secret_audit(watch: &str, findings: &[SecretMatch]) -> CheckRow {
    if findings.is_empty() {
        return watch_row(
            "secret-audit",
            watch,
            Status::Ok,
            format!("watch {watch:?}: no tracked file has a secret-shaped name"),
        );
    }
    let examples: Vec<String> = findings
        .iter()
        .take(SECRET_EXAMPLE_CAP)
        .map(|m| m.path.display().to_string())
        .collect();
    let elided = findings.len().saturating_sub(examples.len());
    let more = if elided > 0 {
        format!(" (+{elided} more)")
    } else {
        String::new()
    };
    watch_row(
        "secret-audit",
        watch,
        Status::Fail,
        format!(
            "watch {watch:?}: {} tracked file(s) have a secret-shaped name, already committed — \
             quarantine only stops NEW secrets, so review these: {}{more}",
            findings.len(),
            examples.join(", ")
        ),
    )
}

// --- check 6: per-watch remote-auth probe ---------------------------------

/// Probes every configured watch's remote for reachability and authentication
/// (a read-only `git ls-remote`), one row per watch. Watches are probed
/// independently — one dead remote never blocks another watch's row. A watch
/// that does not sync, or syncs with no remote defined in its repository, is
/// `skipped` with the reason; with `--offline` every sync-enabled watch is
/// `skipped` "offline mode" without touching the network. A reachable,
/// authenticated remote is `ok`; an unreachable one, an auth failure, or a probe
/// timeout is `fail`; a repository that cannot be opened is `warn` (consistent
/// with the secret audit). With no watches configured there is nothing to probe.
fn check_remote_auth(ctx: &Ctx) -> Vec<CheckRow> {
    if ctx.watches.is_empty() {
        return vec![row(
            "remote-auth",
            Status::Skipped,
            "no watches configured to probe",
        )];
    }
    ctx.watches
        .iter()
        .map(|rw| probe_watch_remote(rw, ctx.offline))
        .collect()
}

/// Probes one watch's remote. Resolves the disposition (skip/warn) that needs no
/// network first, then runs the bounded [`VcsBackend::probe_remote`] and hands
/// its result to [`evaluate_remote_auth`].
fn probe_watch_remote(rw: &ResolvedWatch, offline: bool) -> CheckRow {
    let spec = &rw.spec;
    let name = spec.name();

    // A non-syncing watch has no remote to probe — a deliberate config, not a
    // problem.
    if !spec.sync() {
        return watch_row(
            "remote-auth",
            name,
            Status::Skipped,
            format!("watch {name:?}: sync is disabled (sync = false) — no remote to probe"),
        );
    }

    // Offline: skip the network for every sync-enabled watch and say so, without
    // opening the repository or contacting any remote.
    if offline {
        return watch_row(
            "remote-auth",
            name,
            Status::Skipped,
            format!("watch {name:?}: offline mode — network check skipped"),
        );
    }

    // Open the vetted way, matching the secret audit. An unopenable repository is
    // this watch's own warn — never a crash, never a block on the rest.
    let backend = match vard_core::open_git_backend(spec) {
        Ok(backend) => backend,
        Err(e) => {
            return watch_row(
                "remote-auth",
                name,
                Status::Warn,
                format!(
                    "watch {name:?}: repository could not be opened ({e}); skipped — fix it and \
                     re-run"
                ),
            );
        }
    };

    // A sync-enabled watch whose repository does not define the configured remote
    // is skipped with the reason — the same honest no-op the sync engine treats
    // it as (a cheap, non-network config lookup, so it runs even though the probe
    // below is the network step).
    match backend.has_remote() {
        Ok(true) => {}
        Ok(false) => {
            return watch_row(
                "remote-auth",
                name,
                Status::Skipped,
                format!(
                    "watch {name:?}: sync is on but remote {:?} is not defined in the repository",
                    spec.remote()
                ),
            );
        }
        Err(e) => {
            return watch_row(
                "remote-auth",
                name,
                Status::Warn,
                format!("watch {name:?}: could not read the remote config ({e})"),
            );
        }
    }

    evaluate_remote_auth(
        name,
        spec.remote(),
        backend.probe_remote(REMOTE_PROBE_TIMEOUT),
    )
}

/// Decides one watch's remote-auth row from the probe result, so the mapping is
/// unit-testable without a network. `Ok` is a reachable, authenticated remote; a
/// timeout or any command failure is a `fail`, the latter summarizing git's
/// stderr to its first meaningful line rather than dumping the whole thing.
fn evaluate_remote_auth(watch: &str, remote: &str, probe: Result<(), VcsError>) -> CheckRow {
    match probe {
        Ok(()) => watch_row(
            "remote-auth",
            watch,
            Status::Ok,
            format!("watch {watch:?}: remote {remote:?} is reachable and authenticated"),
        ),
        Err(VcsError::Timeout { elapsed, .. }) => watch_row(
            "remote-auth",
            watch,
            Status::Fail,
            format!(
                "watch {watch:?}: remote {remote:?} did not answer within {elapsed:.1?} — check \
                 the network or VPN"
            ),
        ),
        Err(VcsError::CommandFailed { stderr, .. }) => watch_row(
            "remote-auth",
            watch,
            Status::Fail,
            format!(
                "watch {watch:?}: remote {remote:?} is unreachable or refused authentication: {}",
                redact_userinfo(&first_line(&stderr))
            ),
        ),
        Err(e) => watch_row(
            "remote-auth",
            watch,
            Status::Fail,
            format!("watch {watch:?}: remote {remote:?} probe failed: {e}"),
        ),
    }
}

/// The first non-empty, trimmed line of `text`, so a multi-line git stderr is
/// summarized to its most meaningful line rather than dumped whole. An all-blank
/// string yields `"(no details)"`.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(no details)")
        .to_string()
}

/// Redacts credentials embedded in any URL-shaped token in `line`, so a remote
/// URL carrying userinfo (`https://user:token@host/...` or the token-only
/// `https://token@host/...`) never leaks the secret into doctor's output or JSON
/// when git echoes the URL back in its stderr. The *whole* userinfo is replaced
/// with `***` — the username is not preserved, since it can itself be the secret
/// — while the scheme, host, and the rest of the line are kept for diagnosis.
/// Text with no `scheme://user@host` shape passes through unchanged.
fn redact_userinfo(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("://") {
        out.push_str(&rest[..pos + 3]);
        let after = &rest[pos + 3..];
        // The authority runs to the first path/query/fragment/space boundary.
        let auth_end = after
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(after.len());
        let authority = &after[..auth_end];
        match authority.rfind('@') {
            // Everything before the last `@` is userinfo; drop it wholesale.
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[auth_end..];
    }
    out.push_str(rest);
    out
}

// --- check 7: systemd linger (Linux) --------------------------------------

/// What [`check_linger`] found about the systemd user-service unit and, when
/// one is installed, whether lingering is enabled for the user (so the unit
/// survives logout).
#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum LingerState {
    /// No `vard service` unit is installed — lingering is moot.
    NotInstalled,
    /// A unit is installed; here is what `loginctl` reported about lingering.
    Installed(LoginctlProbe),
}

/// The `loginctl show-user --property=Linger --value` outcome.
#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum LoginctlProbe {
    /// Lingering is enabled for the user.
    Enabled,
    /// Lingering is disabled for the user.
    Disabled,
    /// `loginctl` is missing, failed, or its output could not be parsed as
    /// `yes`/`no`. Carries a one-line detail (the command's own failure
    /// summary, or a note about the unparseable output).
    Unavailable(String),
}

/// Linux: the systemd user unit stops at logout unless lingering is enabled
/// (`vard service install`'s own consent flow — see
/// [`systemd::should_prompt`](crate::service::systemd::should_prompt)). This
/// check reports the *current* state so a service installed non-interactively
/// (or with `--no-linger`) is not silently stopping at every logout.
#[cfg(target_os = "linux")]
fn check_linger(_ctx: &Ctx) -> Vec<CheckRow> {
    vec![evaluate_linger(&probe_linger())]
}

#[cfg(not(target_os = "linux"))]
fn check_linger(_ctx: &Ctx) -> Vec<CheckRow> {
    vec![row(
        "linger",
        Status::Skipped,
        "not applicable on this platform — linger is a systemd concept for keeping a user \
         service alive past logout; launchd has no equivalent",
    )]
}

/// Gathers the linger state: the unit's install path is the service module's
/// own [`systemd::unit_path`](crate::service::systemd::unit_path) — the same
/// single source of truth `vard service` itself writes to — so this check
/// never guesses a path of its own.
#[cfg(target_os = "linux")]
fn probe_linger() -> LingerState {
    let unit_installed = crate::service::systemd::unit_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    if !unit_installed {
        return LingerState::NotInstalled;
    }
    let user = current_user();
    let out = crate::service::run_bounded(
        "loginctl",
        &["show-user", &user, "--property=Linger", "--value"],
        SERVICE_PROBE_TIMEOUT,
    );
    LingerState::Installed(classify_loginctl(&out))
}

/// The identity to pass `loginctl`/`systemctl`: `$USER` when set, else the
/// numeric UID from rustix (both tools accept a UID in place of a username).
#[cfg(target_os = "linux")]
fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| rustix::process::getuid().as_raw().to_string())
}

#[cfg(target_os = "linux")]
fn classify_loginctl(out: &crate::service::RunOutput) -> LoginctlProbe {
    if !out.success() {
        return LoginctlProbe::Unavailable(out.detail());
    }
    match out.stdout.trim() {
        "yes" => LoginctlProbe::Enabled,
        "no" => LoginctlProbe::Disabled,
        other => LoginctlProbe::Unavailable(format!("unexpected `loginctl` output: {other:?}")),
    }
}

/// Decides the linger row from the gathered [`LingerState`], so the decision
/// table tests without a real systemd user session.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evaluate_linger(state: &LingerState) -> CheckRow {
    match state {
        LingerState::NotInstalled => row(
            "linger",
            Status::Ok,
            "service not installed — linger not needed yet",
        ),
        LingerState::Installed(LoginctlProbe::Enabled) => row(
            "linger",
            Status::Ok,
            "lingering is enabled for this user — the service survives logout",
        ),
        LingerState::Installed(LoginctlProbe::Disabled) => row(
            "linger",
            Status::Warn,
            "lingering is disabled — user services stop at logout; run `loginctl enable-linger` \
             (or `vard service install` with `--linger`) to keep it running",
        ),
        LingerState::Installed(LoginctlProbe::Unavailable(detail)) => {
            row("linger", Status::Skipped, detail.clone())
        }
    }
}

// --- check 8: service-context agent/keychain reachability ------------------

/// Linux: the `systemd --user` environment the service context inherits from.
#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum SystemctlEnv {
    /// `systemctl --user show-environment` succeeded; whether it listed an
    /// `SSH_AUTH_SOCK=` line.
    Read { has_ssh_auth_sock: bool },
    /// `systemctl` is missing or the user bus is unreachable. Carries a
    /// one-line failure summary.
    Unavailable(String),
}

/// Whether `url` is an ssh-style git remote (`git@host:path`, `ssh://…`, or
/// anything else that is not plain HTTP(S)) — the shapes that need a running
/// ssh-agent (or an interactively-unlocked key) to authenticate, unlike an
/// HTTPS remote's credential-helper/keychain path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_ssh_remote(url: &str) -> bool {
    !(url.starts_with("https://") || url.starts_with("http://"))
}

/// Reads the URL git has configured for a watch's remote (`git config --get
/// remote.<name>.url`) — the same read-only, non-network lookup
/// [`VcsBackend::has_remote`] performs internally, mirrored here directly
/// since the trait does not expose the URL itself, only its presence. `None`
/// for a non-syncing watch, an unopenable/unconfigured remote, or a failed
/// lookup — such a watch simply does not count toward "has an ssh remote".
#[cfg(target_os = "linux")]
fn watch_remote_url(rw: &ResolvedWatch) -> Option<String> {
    if !rw.spec.sync() {
        return None;
    }
    let key = format!("remote.{}.url", rw.spec.remote());
    let out = Command::new("git")
        .arg("-C")
        .arg(rw.spec.path())
        .args(["config", "--get", &key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Probes whether the service context (a systemd user manager, or a launchd
/// GUI session) can reach an ssh-agent / the keychain — the credential paths a
/// sync-enabled watch with an ssh remote needs, but which a login-session
/// service manager does not always inherit.
///
/// **Linux**: a systemd user unit stops at logout unless lingering is on
/// ([`check_linger`]), and even lingering does not guarantee `SSH_AUTH_SOCK`
/// is exported into the user manager's environment — an ssh-agent started by
/// a login shell lives outside it unless imported. This check probes for
/// exactly that gap.
///
/// **macOS**: the LaunchAgent runs inside the user's own GUI login session
/// (`gui/<uid>`), so the keychain and any ssh-agent socket are reachable by
/// construction — there is nothing to probe for reachability. Instead this
/// reports the service's actual loaded/running state, since that is the
/// thing that can actually be wrong here.
#[cfg(target_os = "linux")]
fn check_service_agent(ctx: &Ctx) -> Vec<CheckRow> {
    let has_ssh_remotes = ctx
        .watches
        .iter()
        .any(|rw| watch_remote_url(rw).is_some_and(|u| is_ssh_remote(&u)));
    let unit_installed = crate::service::systemd::unit_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    let env = probe_systemctl_env();
    vec![evaluate_service_agent_linux(
        has_ssh_remotes,
        unit_installed,
        &env,
    )]
}

#[cfg(target_os = "linux")]
fn probe_systemctl_env() -> SystemctlEnv {
    let out = crate::service::run_bounded(
        "systemctl",
        &["--user", "show-environment"],
        SERVICE_PROBE_TIMEOUT,
    );
    if !out.success() {
        return SystemctlEnv::Unavailable(out.detail());
    }
    let has_ssh_auth_sock = out
        .stdout
        .lines()
        .any(|l| l.trim_start().starts_with("SSH_AUTH_SOCK="));
    SystemctlEnv::Read { has_ssh_auth_sock }
}

/// Decides the Linux service-agent row, so the decision table tests without a
/// real systemd user session.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn evaluate_service_agent_linux(
    has_ssh_remotes: bool,
    unit_installed: bool,
    env: &SystemctlEnv,
) -> CheckRow {
    if !has_ssh_remotes {
        return row(
            "service-agent",
            Status::Ok,
            "no ssh remotes — agent not needed",
        );
    }
    if !unit_installed {
        return row(
            "service-agent",
            Status::Ok,
            "service not installed — nothing to probe",
        );
    }
    match env {
        SystemctlEnv::Unavailable(detail) => row("service-agent", Status::Skipped, detail.clone()),
        SystemctlEnv::Read {
            has_ssh_auth_sock: true,
        } => row(
            "service-agent",
            Status::Ok,
            "SSH_AUTH_SOCK is set in the systemd user manager environment — ssh-remote syncs can \
             reach your agent under the service",
        ),
        SystemctlEnv::Read {
            has_ssh_auth_sock: false,
        } => row(
            "service-agent",
            Status::Warn,
            "the service context has no SSH_AUTH_SOCK in its systemd user manager environment, so \
             ssh-remote sync auth may fail under the service — import it with `systemctl --user \
             import-environment SSH_AUTH_SOCK` from a login shell (or an environment.d entry) \
             after your agent starts",
        ),
    }
}

#[cfg(target_os = "macos")]
fn check_service_agent(_ctx: &Ctx) -> Vec<CheckRow> {
    let uid = rustix::process::getuid().as_raw();
    let plist_exists = crate::service::launchd::plist_path()
        .map(|p| p.exists())
        .unwrap_or(false);
    let target = crate::service::launchd::service_target(uid);
    let out = crate::service::run_bounded("launchctl", &["print", &target], SERVICE_PROBE_TIMEOUT);
    let print = crate::service::launchd::parse_launchctl_print(&out);
    vec![evaluate_service_agent_macos(plist_exists, &print)]
}

/// Decides the macOS service-agent row from the plist's presence and the
/// parsed `launchctl print` state (the parser lives in the launchd backend —
/// the single source of truth shared with the `start` verb, VRD-59), so it
/// tests without a real launchd.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn evaluate_service_agent_macos(plist_exists: bool, print: &LaunchctlPrint) -> CheckRow {
    if !plist_exists {
        return row(
            "service-agent",
            Status::Ok,
            "service not installed — nothing to probe",
        );
    }
    match print {
        LaunchctlPrint::Running => row(
            "service-agent",
            Status::Ok,
            "the LaunchAgent runs in your GUI login session, so the keychain and ssh-agent are \
             reachable there; the service is loaded and running",
        ),
        LaunchctlPrint::Exited { code } => row(
            "service-agent",
            Status::Warn,
            format!(
                "the service is crash-looping/exiting (last exit {code}) — run `vard run` in the \
                 foreground to see why"
            ),
        ),
        LaunchctlPrint::NotLoaded => row(
            "service-agent",
            Status::Warn,
            "the unit file is present but not loaded — run `vard service start`",
        ),
        LaunchctlPrint::Unparsed(detail) => row(
            "service-agent",
            Status::Skipped,
            format!("could not interpret `launchctl print` output: {detail}"),
        ),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_service_agent(_ctx: &Ctx) -> Vec<CheckRow> {
    vec![row(
        "service-agent",
        Status::Skipped,
        "not applicable on this platform — vard service is supported on macOS and Linux only",
    )]
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
/// A per-watch row additionally carries a `watch` field (between `status` and
/// `detail`); a global row omits it entirely, so a machine consumer reads the
/// watch name directly instead of parsing it out of `detail`.
fn records(rows: &[CheckRow]) -> Vec<Record> {
    rows.iter()
        .map(|r| {
            let mut fields = vec![
                RecordField::str("check", &r.name),
                RecordField::str("status", r.status.token()),
            ];
            if let Some(watch) = &r.watch {
                fields.push(RecordField::str("watch", watch));
            }
            fields.push(RecordField::str("detail", &r.detail));
            Record {
                header: None,
                fields,
            }
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

    fn scan(crashed: &[&str], stale_settled: &[&str], unrecognized: &[&str]) -> RequestDirScan {
        RequestDirScan {
            crashed: crashed.iter().map(|s| s.to_string()).collect(),
            stale_settled: stale_settled.iter().map(|s| s.to_string()).collect(),
            unrecognized: unrecognized.iter().map(|s| s.to_string()).collect(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn request_dir_clean_is_ok() {
        assert_eq!(
            evaluate_request_dir(&RequestDirScan::default()).status,
            Status::Ok
        );
    }

    #[test]
    fn request_dir_with_crashed_leftovers_warns_and_names_them() {
        let r = evaluate_request_dir(&scan(&[".req-123.toml.tmp-99"], &[], &[]));
        assert_eq!(r.status, Status::Warn);
        assert!(
            r.detail.contains(".req-123.toml.tmp-99"),
            "names it: {}",
            r.detail
        );
        assert!(r.detail.contains("safe to delete"), "hints: {}", r.detail);
    }

    #[test]
    fn request_dir_with_unrecognized_entry_warns_and_does_not_call_it_safe() {
        // A stale name vard never wrote (not a settled request, not a temp).
        let r = evaluate_request_dir(&scan(&[], &[], &["mystery.dat"]));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("mystery.dat"), "names it: {}", r.detail);
        assert!(
            r.detail.contains("vard did not write this")
                && r.detail.contains("investigate before deleting"),
            "honest unrecognized wording: {}",
            r.detail
        );
        // An unrecognized file is never labeled safe to delete.
        assert!(
            !r.detail.contains("safe to delete"),
            "must not call an unrecognized file safe to delete: {}",
            r.detail
        );
    }

    #[test]
    fn request_dir_with_stale_settled_warns_about_piling_up() {
        let r = evaluate_request_dir(&scan(&[], &["req-1.toml"], &[]));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("req-1.toml"), "names it: {}", r.detail);
        assert!(
            r.detail.contains("piling up") && r.detail.contains("no daemon"),
            "distinct piling-up wording: {}",
            r.detail
        );
        // The stale-settled wording must NOT borrow the crashed-writer phrasing.
        assert!(
            !r.detail.contains("crashed writer"),
            "distinct from the crashed case: {}",
            r.detail
        );
    }

    #[test]
    fn request_dir_with_both_kinds_warns_and_reports_each() {
        let r = evaluate_request_dir(&scan(&[".req-1.toml.tmp-7"], &["req-2.toml"], &[]));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("crashed writer"), "crashed: {}", r.detail);
        assert!(r.detail.contains("piling up"), "settled: {}", r.detail);
    }

    #[test]
    fn request_dir_scan_error_warns_and_is_never_a_silent_ok() {
        let mut s = RequestDirScan::default();
        s.errors
            .push("could not read the request dir: boom".to_string());
        let r = evaluate_request_dir(&s);
        assert_eq!(r.status, Status::Warn, "a read failure must not report ok");
        assert!(r.detail.contains("boom"), "names the error: {}", r.detail);
    }

    #[test]
    fn scan_sorts_stale_entries_into_the_three_buckets_and_ignores_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000_000u64;
        let stale = now - request::STALE_AFTER.as_secs() - 60;
        let fresh = now - 5;

        // A stale settled request: piling up unconsumed.
        std::fs::write(dir.path().join("req.toml"), "x").unwrap();
        // A stale crashed-writer leftover: the real atomic temp scheme
        // (`.{final}.tmp-{pid}`).
        let leftover = dir.path().join(".req-1.toml.tmp-4242");
        std::fs::write(&leftover, "x").unwrap();
        // A stale name vard never wrote: unrecognized, not "safe to delete".
        let stray = dir.path().join("mystery.dat");
        std::fs::write(&stray, "x").unwrap();
        // A fresh temp (a writer mid-flight): not flagged.
        let inflight = dir.path().join(".req-2.toml.tmp-4243");
        std::fs::write(&inflight, "x").unwrap();
        // A fresh settled request (the daemon just has not drained it yet): not
        // flagged.
        let queued = dir.path().join("req-fresh.toml");
        std::fs::write(&queued, "x").unwrap();

        set_mtime(&dir.path().join("req.toml"), stale);
        set_mtime(&leftover, stale);
        set_mtime(&stray, stale);
        set_mtime(&inflight, fresh);
        set_mtime(&queued, fresh);

        let found = scan_request_dir(dir.path(), now);
        assert_eq!(
            found.crashed,
            vec![".req-1.toml.tmp-4242".to_string()],
            "only the stale atomic-temp leftover is a crashed-writer entry"
        );
        assert_eq!(
            found.stale_settled,
            vec!["req.toml".to_string()],
            "only the stale settled request piles up"
        );
        assert_eq!(
            found.unrecognized,
            vec!["mystery.dat".to_string()],
            "the stale stray is unrecognized, not crashed"
        );
        assert!(found.errors.is_empty(), "a clean scan records no errors");
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let found = scan_request_dir(&dir.path().join("nope"), 1_000);
        assert!(
            found.crashed.is_empty()
                && found.stale_settled.is_empty()
                && found.unrecognized.is_empty()
                && found.errors.is_empty(),
            "a missing dir is an empty, clean, error-free scan"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_unreadable_dir_records_an_error_not_a_clean_ok() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Drop all permissions so read_dir fails with something other than
        // NotFound. Running as root defeats this, so skip-gate on that case.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        // Captured while the dir is unreadable; restore afterwards for cleanup.
        let found = scan_request_dir(dir.path(), 1_000);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        if found.errors.is_empty() {
            // The chmod did not actually block reads (running as root, or a
            // permissive FS) — nothing to assert.
            eprintln!("skipping: permissions did not block directory reads");
            return;
        }
        assert!(
            !found.errors.is_empty(),
            "an unreadable dir must record an error, not report a clean ok"
        );
        assert_eq!(
            evaluate_request_dir(&found).status,
            Status::Warn,
            "an unreadable request dir warns"
        );
    }

    /// Sets a file's mtime to `secs` past the epoch, via a `SystemTime` on the
    /// standard `set_times` API (no external crate).
    fn set_mtime(path: &Path, secs: u64) {
        let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let times = std::fs::FileTimes::new().set_modified(t);
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_times(times).unwrap();
    }

    // --- secret audit -------------------------------------------------------

    fn secret_named(path: &str) -> SecretMatch {
        SecretMatch {
            path: std::path::PathBuf::from(path),
            reason: vard_core::SecretReason::FilenamePattern {
                pattern: ".env".to_string(),
            },
        }
    }

    #[test]
    fn secret_audit_clean_watch_is_ok() {
        let r = evaluate_secret_audit("notes", &[]);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("notes"), "names the watch: {}", r.detail);
    }

    #[test]
    fn secret_audit_findings_fail_with_count_and_capped_examples() {
        let findings: Vec<SecretMatch> = (0..8)
            .map(|i| secret_named(&format!("dir/secret{i}.env")))
            .collect();
        let r = evaluate_secret_audit("vault", &findings);
        assert_eq!(r.status, Status::Fail);
        assert!(r.detail.contains("8 tracked"), "count: {}", r.detail);
        // At most SECRET_EXAMPLE_CAP paths are shown, and the rest are elided.
        assert!(r.detail.contains("dir/secret0.env"));
        assert!(
            !r.detail.contains("dir/secret5.env"),
            "past the cap must be elided: {}",
            r.detail
        );
        assert!(r.detail.contains("+3 more"), "elision note: {}", r.detail);
    }

    #[test]
    fn secret_audit_names_committed_provenance() {
        // The fail wording must make clear the file is ALREADY committed — that
        // is precisely what quarantine cannot catch.
        let r = evaluate_secret_audit("w", &[secret_named("id_rsa")]);
        assert!(r.detail.contains("already committed"), "got: {}", r.detail);
    }

    #[test]
    fn secret_audit_rows_carry_the_watch_field() {
        assert_eq!(
            evaluate_secret_audit("notes", &[]).watch.as_deref(),
            Some("notes")
        );
        assert_eq!(
            evaluate_secret_audit("vault", &[secret_named("id_rsa")])
                .watch
                .as_deref(),
            Some("vault")
        );
    }

    // --- remote-auth ---------------------------------------------------------

    #[test]
    fn remote_auth_reachable_is_ok() {
        let r = evaluate_remote_auth("notes", "origin", Ok(()));
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.watch.as_deref(), Some("notes"));
        assert!(
            r.detail.contains("reachable") && r.detail.contains("origin"),
            "got: {}",
            r.detail
        );
    }

    #[test]
    fn remote_auth_command_failure_fails_with_first_sanitized_stderr_line_only() {
        let probe = Err(VcsError::CommandFailed {
            op: "ls-remote".to_string(),
            status: Some(128),
            stderr: "fatal: repository 'x' does not exist\nfatal: Could not read from remote\n"
                .to_string(),
        });
        let r = evaluate_remote_auth("vault", "origin", probe);
        assert_eq!(r.status, Status::Fail);
        // Credential-free text passes through verbatim (the first line only).
        assert!(
            r.detail.contains("does not exist"),
            "summarizes the first line: {}",
            r.detail
        );
        // The second stderr line is elided — a summary, not a dump.
        assert!(
            !r.detail.contains("Could not read"),
            "must not dump the whole stderr: {}",
            r.detail
        );
    }

    #[test]
    fn remote_auth_command_failure_redacts_url_credentials() {
        // git echoes the remote URL back in its stderr; a URL with embedded
        // userinfo must never leak the secret into doctor's output or JSON.
        let secret = concat!("gh", "p_", "s3cr3tt0ken");
        let probe = Err(VcsError::CommandFailed {
            op: "ls-remote".to_string(),
            status: Some(128),
            stderr: format!(
                "fatal: unable to access 'https://alice:{secret}@github.com/acme/vault.git/': \
                 authentication failed\n"
            ),
        });
        let r = evaluate_remote_auth("vault", "origin", probe);
        assert_eq!(r.status, Status::Fail);
        // The credential (both the username and the token) is gone…
        assert!(
            !r.detail.contains(secret) && !r.detail.contains("alice"),
            "the credential must be redacted: {}",
            r.detail
        );
        // …the userinfo collapses to `***`, and the host survives for diagnosis.
        assert!(
            r.detail.contains("https://***@github.com") && r.detail.contains("authentication"),
            "host and reason must survive: {}",
            r.detail
        );
    }

    #[test]
    fn redact_userinfo_covers_token_only_and_leaves_plain_text() {
        // Token-only userinfo (`https://token@host`) is redacted too.
        assert_eq!(
            redact_userinfo("clone https://t0ken@example.com/x.git failed"),
            "clone https://***@example.com/x.git failed"
        );
        // A URL without userinfo is untouched.
        assert_eq!(
            redact_userinfo("fetch https://example.com/x.git timed out"),
            "fetch https://example.com/x.git timed out"
        );
        // Non-URL text passes through unchanged.
        assert_eq!(
            redact_userinfo("fatal: not a git repository"),
            "fatal: not a git repository"
        );
    }

    #[test]
    fn remote_auth_timeout_fails_and_names_the_bound() {
        let probe = Err(VcsError::Timeout {
            op: "ls-remote".to_string(),
            elapsed: Duration::from_secs(10),
        });
        let r = evaluate_remote_auth("vault", "origin", probe);
        assert_eq!(r.status, Status::Fail);
        assert!(
            r.detail.contains("did not answer"),
            "timeout wording: {}",
            r.detail
        );
    }

    #[test]
    fn first_line_summarizes_and_handles_blank() {
        assert_eq!(first_line("  fatal: nope\nmore\n"), "fatal: nope");
        assert_eq!(first_line("\n\n  \n"), "(no details)");
    }

    // --- records (machine shape) --------------------------------------------

    #[test]
    fn records_carry_watch_only_on_per_watch_rows() {
        let rows = vec![
            row("git", Status::Ok, "fine"),
            watch_row("remote-auth", "notes", Status::Ok, "reachable"),
        ];
        let recs = records(&rows);
        // The global git row has no `watch` field at all.
        assert!(
            recs[0].fields.iter().all(|f| f.key != "watch"),
            "global row must omit watch"
        );
        // The per-watch row carries it.
        assert!(
            recs[1].fields.iter().any(|f| f.key == "watch"),
            "per-watch row must carry watch"
        );
    }

    // --- status / exit-code folding -----------------------------------------

    #[test]
    fn exit_code_folds_worst_of_all_rows() {
        assert_eq!(Status::Ok.exit_code(), 0);
        assert_eq!(Status::Skipped.exit_code(), 0);
        assert_eq!(Status::Warn.exit_code(), 1);
        assert_eq!(Status::Fail.exit_code(), 1);
    }

    // --- linger ---------------------------------------------------------

    #[test]
    fn linger_not_installed_is_ok_and_flag_only() {
        let r = evaluate_linger(&LingerState::NotInstalled);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("not installed"), "got: {}", r.detail);
        // Flag-only: never advises running an install command from this row.
        assert!(
            !r.detail.contains('`'),
            "must not carry command advice: {}",
            r.detail
        );
    }

    #[test]
    fn linger_enabled_is_ok() {
        let r = evaluate_linger(&LingerState::Installed(LoginctlProbe::Enabled));
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("enabled"), "got: {}", r.detail);
    }

    #[test]
    fn linger_disabled_warns_with_advice() {
        let r = evaluate_linger(&LingerState::Installed(LoginctlProbe::Disabled));
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("enable-linger"), "got: {}", r.detail);
        assert!(r.detail.contains("--linger"), "got: {}", r.detail);
    }

    #[test]
    fn linger_loginctl_unavailable_is_skipped_with_detail() {
        let r = evaluate_linger(&LingerState::Installed(LoginctlProbe::Unavailable(
            "command not found".to_string(),
        )));
        assert_eq!(r.status, Status::Skipped);
        assert_eq!(r.detail, "command not found");
    }

    // --- ssh-remote classification ---------------------------------------

    #[test]
    fn ssh_remote_classification() {
        assert!(is_ssh_remote("git@github.com:acme/vault.git"));
        assert!(is_ssh_remote("ssh://git@example.com/acme/vault.git"));
        assert!(is_ssh_remote("/local/bare/repo.git"));
        assert!(!is_ssh_remote("https://github.com/acme/vault.git"));
        assert!(!is_ssh_remote("http://example.com/acme/vault.git"));
    }

    // --- service-agent (Linux) --------------------------------------------

    #[test]
    fn service_agent_linux_no_ssh_remotes_is_ok() {
        let r = evaluate_service_agent_linux(
            false,
            true,
            &SystemctlEnv::Read {
                has_ssh_auth_sock: false,
            },
        );
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("no ssh remotes"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_linux_not_installed_is_ok() {
        let r = evaluate_service_agent_linux(
            true,
            false,
            &SystemctlEnv::Read {
                has_ssh_auth_sock: false,
            },
        );
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("not installed"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_linux_systemctl_unavailable_is_skipped() {
        let r = evaluate_service_agent_linux(
            true,
            true,
            &SystemctlEnv::Unavailable("Failed to connect to bus".to_string()),
        );
        assert_eq!(r.status, Status::Skipped);
        assert_eq!(r.detail, "Failed to connect to bus");
    }

    #[test]
    fn service_agent_linux_socket_present_is_ok() {
        let r = evaluate_service_agent_linux(
            true,
            true,
            &SystemctlEnv::Read {
                has_ssh_auth_sock: true,
            },
        );
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("SSH_AUTH_SOCK"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_linux_socket_missing_warns_with_advice() {
        let r = evaluate_service_agent_linux(
            true,
            true,
            &SystemctlEnv::Read {
                has_ssh_auth_sock: false,
            },
        );
        assert_eq!(r.status, Status::Warn);
        assert!(
            r.detail.contains("import-environment SSH_AUTH_SOCK"),
            "got: {}",
            r.detail
        );
    }

    // The `launchctl print` parser now lives in the launchd backend
    // (`crate::service::launchd`), the single source of truth shared with the
    // `start` verb's state probe (VRD-59); its unit tests live there too.

    // --- service-agent (macOS) ----------------------------------------------

    #[test]
    fn service_agent_macos_plist_absent_is_ok() {
        let r = evaluate_service_agent_macos(false, &LaunchctlPrint::Running);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("not installed"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_macos_running_is_ok() {
        let r = evaluate_service_agent_macos(true, &LaunchctlPrint::Running);
        assert_eq!(r.status, Status::Ok);
        assert!(r.detail.contains("running"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_macos_exited_warns_and_names_the_code() {
        let r = evaluate_service_agent_macos(true, &LaunchctlPrint::Exited { code: 78 });
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("78"), "got: {}", r.detail);
        assert!(r.detail.contains("vard run"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_macos_not_loaded_warns_with_start_advice() {
        let r = evaluate_service_agent_macos(true, &LaunchctlPrint::NotLoaded);
        assert_eq!(r.status, Status::Warn);
        assert!(r.detail.contains("vard service start"), "got: {}", r.detail);
    }

    #[test]
    fn service_agent_macos_unparsed_is_skipped_with_detail() {
        let r = evaluate_service_agent_macos(
            true,
            &LaunchctlPrint::Unparsed("garbled output".to_string()),
        );
        assert_eq!(r.status, Status::Skipped);
        assert!(r.detail.contains("garbled output"), "got: {}", r.detail);
    }
}
