# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it ships v1.0. Pre-1.0 versions may include breaking changes in minor
releases.

## [Unreleased]

Entries here have landed on `main` but have not yet been cut into a tagged
release. When a release is cut, this section is promoted to
`## v0.X.0 - YYYY-MM-DD` and a fresh `## [Unreleased]` header is added above it.

### Added

- `vard doctor` gains a `daemon-version` check and `vard status` surfaces the
  same skew on the daemon line: a running daemon whose version differs from the
  installed binary (or one too old to report a version) `warn`s with a
  `vard service restart` hint. Every install path except `vard self-update` —
  `cargo install`, `curl | sh`, a manual copy — replaces the binary without
  restarting the daemon, so a stale daemon could otherwise run unnoticed.

## v0.2.0 - 2026-07-21

### Added

- `vard self-update [--version X.Y.Z] [--dry-run]` — update the installed
  binary in place from GitHub Releases, gated on the cargo-dist install
  receipt. The download's sha256 is verified before an atomic swap, so a
  failed or mismatched download leaves the running binary untouched;
  `--version` pins a release (including downgrades) as the rollback. With a
  service installed, it restarts the daemon and verifies the new version
  came up before reporting success.
- `vard service install`, `start`, and `restart` pre-flight the daemon's
  config — the same check `vard run` performs at startup — and refuse with
  exit `2` and direct advice when the config is missing, invalid, or
  defines no watches; `--dry-run` reports the would-refuse reason as a
  warning instead.

### Fixed

- `vard service restart` (and `start`'s recovery path) now bring the macOS
  LaunchAgent up correctly from every state, including a crash-looping
  agent that previously failed with `Bootstrap failed: 5: Input/output
  error`.

## v0.1.0 - 2026-07-19

### Added

- Cargo workspace scaffold (`vard-core`, `vard`) with XDG paths, and a TOML
  config schema (`config.toml`): `[[watch]]` + `[defaults]` inheritance,
  trigger modes, durations, excludes, and remote/branch settings, with
  strict validation and tilde expansion.
- The watch engine: per-watch quiescence debounce (gitignore excludes, an
  automatic `.git/` exclusion, a polling fallback for unreliable
  filesystems) plus an optional fixed interval; one worker per watch
  coalesces bursts and skips unsafe repository states (mid-merge,
  mid-rebase, detached HEAD). A git backend handles snapshotting and
  history, hardened against a user's own git configuration. `vard run` is
  the foreground daemon: hot config reload, a single-instance lock, and a
  crash-safe operation journal.
- A full CLI built on `clap`: global `--format`/`--color` output primitives
  (TTY auto-detected), a two-tier help contract (`-h` scannable, `--help` a
  full reference), and generated shell completions plus a manpage for every
  command.
- `vard watch add/remove/list/pause/resume/set` — register, list, pause or
  resume, and edit watched directories (trigger, interval, quiesce, sync
  interval, remote, branch); `add` offers to `git init` and seeds vard's
  excludes into `.git/info/exclude`.
- `vard snapshot`, `vard history`, `vard diff`, and `vard restore` — manual
  snapshot, filtered history, unified diff, and restore to a prior point;
  `restore` always takes a protective snapshot first, so it's never
  destructive.
- `vard sync` and opt-in remote syncing (`vard watch sync` / `watch add
  --sync`, off by default): a snapshot-first reconcile that commits locally
  before touching the remote, rebases out-of-tree in a scratch worktree,
  and only advances the tree through fully committed states. Auto-sync
  runs after every snapshot on a sync-enabled watch, plus an optional
  jittered pull cadence.
- `vard status`, `vard notify`, `vard doctor`, and `vard logs [-f] [-n N]`
  — a read-only health join with exit codes, a near-instant shell-hook
  check, environment diagnosis (git version, inotify limits, stale
  requests, secret audit, remote reachability, service/linger checks), and
  rotating logfile access; `status`/`notify`/`doctor` are backed by a
  versioned daemon health file.
- `vard config get/set/unset/edit/path` — comment-preserving,
  lock-serialized scalar config edits; `get`/`path` print the bare value
  when piped.
- Hooks (`[hooks]` / per-watch `[watch.hooks]` shell commands on vard's bus
  events, coalescing loop guard) and secret quarantine (every snapshot
  scans new files for likely secrets and withholds matches from the
  commit, self-clearing once none are found). A repository that can't be
  opened is skipped and flagged, not fatal — the daemon keeps snapshotting
  every other watch.
- `vard service install/uninstall/start/stop/restart` — run vard as a login
  service (macOS LaunchAgent or Linux systemd user unit, with
  consent-gated `loginctl enable-linger`); `install` verifies the daemon
  comes up before reporting success, `--dry-run` touches nothing.
- Release machinery via cargo-dist: a shell installer and tag-driven GitHub
  Releases for macOS and Linux (aarch64 and x86_64), bundling the generated
  completions and manpage. Release notes are drawn from this changelog.
- Continuous integration: formatting, clippy (`-D warnings`), the test suite,
  `cargo audit`, `cargo deny`, cross-target and dist-profile build checks, and
  install/completion/manpage smoke checks.
