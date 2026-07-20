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

## v0.2.0 - 2026-07-20

### Added

- `vard self-update [--version X.Y.Z] [--dry-run]` — update the installed
  binary in place from GitHub Releases. Gated on the cargo-dist install
  receipt (a `cargo install`, Homebrew, or source build has no receipt and is
  pointed back at its own tooling); the download's sha256 is verified against
  the release manifest before anything is unpacked, and the swap itself is a
  single atomic rename, so a failed or mismatched download always leaves the
  running binary untouched. `--version` pins a specific release, including
  downgrades — that doubles as the rollback mechanism, since there is no
  separate revert command. When a service unit is installed, `self-update`
  restarts the daemon and polls its health file for the new version before
  reporting success; with no service installed it reports the swap and says
  so.
- `vard service install`, `start`, and `restart` now pre-flight the daemon's
  config before touching the unit — the same check `vard run` performs at
  startup — and refuse with exit `2` and direct advice (e.g. "add a watch
  first") when the config is missing, invalid, or defines no watches.
  `--dry-run` reports the would-refuse reason as a warning instead of exiting
  non-zero; an all-paused config is not a refusal.

### Fixed

- `vard service restart` (and `start`'s recovery path) now bring the macOS
  LaunchAgent up correctly from every state — running, stopped, not loaded,
  or crash-looping — instead of failing with `Bootstrap failed: 5:
  Input/output error` when the agent was throttled.
- Release builds for the musl Linux targets (x86_64, aarch64) carry the C
  compiler wiring cargo-dist's generated release workflow needs, fixing a
  build failure in the tree's C-compiled dependencies (`ring`, `lzma-sys`)
  that would have blocked those release artifacts.

## v0.1.0 - 2026-07-19

### Added

- Cargo workspace scaffold: `vard-core` (the embeddable engine) and `vard`
  (the binary), with XDG base-directory resolution for the config, state,
  data, and log paths.
- TOML config schema (`config.toml`): `[[watch]]` entries with `[defaults]`
  inheritance, trigger mode (`events`, `interval`, or `both`),
  quiesce/interval/sync-interval durations, excludes, and remote/branch
  settings, with strict validation, tilde expansion, and duplicate-name/path
  rejection.
- Filesystem watcher: per-watch quiescence windows debounce bursts of
  filesystem activity into a single snapshot signal, with gitignore-dialect
  excludes, an automatic `.git/` exclusion, and a polling fallback for
  filesystems (e.g. network mounts) where native watching isn't reliable.
- A per-watch interval scheduler for time-based snapshots independent of file
  activity, and a snapshot engine that runs one worker per watch, coalesces
  bursts into a single pass, and skips unsafe repository states (mid-merge,
  mid-rebase, detached HEAD).
- `vard run` — the foreground daemon: hot config reload (`SIGHUP` or an
  edited config file), a single-instance lock, and a crash-safe operation
  journal that recovers cleanly after a hard kill.
- A git version-control backend: snapshot, history, diff, and restore, plus
  fetch/reconcile/push for sync — locale-pinned, unicode-safe, and hardened
  against a user's own git configuration (signing, autostash, and hooks never
  leak into vard's own commits).
- Minimal `vard` command-line skeleton built on `clap`, grown into a full CLI
  with global `--format {records|json|jsonl}` and `--color {auto|always|never}`
  output primitives (TTY auto-detected), a two-tier help contract (`-h`
  scannable, `--help` a full reference with examples), and shell completions
  (bash, zsh, fish, nushell) plus a manpage covering every command, generated
  from the real CLI definitions at build time.
- `vard watch add/remove/list/pause/resume` — register, list, and pause or
  resume watched directories; `add` offers to `git init` an unversioned
  directory and seeds vard's own excludes into `.git/info/exclude`.
- `vard watch set` — edit an existing watch's trigger mode, interval, quiesce
  window, sync interval, remote, or branch without hand-editing the config
  file.
- `vard snapshot`, `vard history`, `vard diff`, and `vard restore` — trigger a
  manual snapshot, list snapshot history (with `--since` filtering), view a
  unified diff, and restore to a prior point in time. `restore` always takes
  a protective snapshot first, so a restore is never destructive.
- `vard sync` and opt-in remote syncing: a snapshot-first reconcile cycle
  that always commits local changes before touching the remote, rebases
  out-of-tree in a scratch worktree so the working directory is never left
  mid-conflict, and only ever advances the working tree through fully
  committed states. Syncing is off by default per watch; `vard watch sync`
  (and `watch add --sync`) is the explicit opt-in gesture, running one
  confirmation sync cycle and reporting its honest outcome. Auto-sync runs
  after every successful snapshot on a sync-enabled watch, plus an optional
  jittered pull-driven `sync_interval` cadence for passive machines.
- `vard status` — a read-only join of configured watches and daemon health,
  with per-watch state (`ok`/`paused`/`unknown`) and exit codes for
  scripting.
- `vard notify` — a near-instant health check (no daemon round-trip, no git)
  designed for shell prompts and hooks: silent and exit 0 when healthy, one
  sanitized line per problem otherwise.
- A daemon health file (`<state_dir>/health`) as the single source of truth
  `status`, `notify`, and `doctor` read from — a coalesced, versioned
  projection of engine state rather than shadow tracking, so it can't drift
  from what the daemon actually did.
- `vard config get/set/unset/edit/path` — read and edit scalar config values
  under a comment-preserving, lock-serialized writer; `get` and `path` print
  the bare value when piped, so they compose directly into shell scripts.
- `vard logs [-f] [-n N]` — read or follow the daemon's own rotating,
  retention-bounded logfile.
- `vard doctor` — read-only environment diagnosis: git version, inotify
  watch limits (Linux), health-file freshness, stale request files, a
  filename-based secret audit of already-tracked files, remote reachability
  for sync-enabled watches, and (once a service is installed) linger and
  service-context checks.
- Hooks: `[hooks]` and per-watch `[watch.hooks]` shell commands run on vard's
  bus events (snapshot, sync, restore, daemon lifecycle), with a coalescing
  loop guard (a hook that fires too often is delayed, never dropped), a
  timeout, and failures surfaced through `status`/`notify`.
- Secret quarantine: every snapshot pass scans newly added files (filename
  catalog, high-precision credential-token prefixes, an entropy heuristic)
  and withholds any match from the commit — never silently committed, never
  touched on disk. Clears itself automatically once a pass no longer finds
  anything.
- Structural safety invariants: a per-watch operation lock makes
  one-writer-per-watch true at all times (daemon worker, config-reload
  drain, and in-process CLI operations can never race each other), and
  daemon health distinguishes self-clearing trouble (a watch's own next
  successful pass proves a snapshot failure or panic is resolved) from
  latching trouble (a dead signal source, cleared only when the daemon
  rebuilds that watch). A repository that can't be opened is isolated to its
  own watch — the daemon keeps running and snapshotting every other watch,
  surfacing the broken one as an `attention` row instead of stopping.
- `vard service install/uninstall/start/stop/restart` — run vard as a login
  service: a macOS LaunchAgent or a Linux systemd user unit (with
  consent-gated `loginctl enable-linger`). `install` writes the unit,
  enables it, and verifies the daemon actually comes up before reporting
  success; `--dry-run` previews the unit and planned actions without
  touching anything.
- Release machinery via cargo-dist: a shell installer and tag-driven GitHub
  Releases for macOS and Linux (aarch64 and x86_64), bundling the generated
  completions and manpage. Release notes are drawn from this changelog.
- Continuous integration: formatting, clippy (`-D warnings`), the test suite,
  `cargo audit`, `cargo deny`, cross-target and dist-profile build checks, and
  install/completion/manpage smoke checks.
