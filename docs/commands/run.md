---
title: run
description: Run the vard daemon in the foreground, watching and snapshotting every active configured directory.
---

# vard run

Run the vard daemon in the foreground until stopped. This is the process that does the actual watching: where [`snapshot`](snapshot.md) takes one snapshot on demand, `run` watches every active configured directory continuously and snapshots changes into version control on its own. It stays attached to the terminal and logs each event to stderr, so you run it under a process supervisor (systemd, launchd, a tmux pane) rather than as a one-shot command.

## Examples

```bash
vard run
# watch every active configured directory and snapshot until Ctrl-C

vard run &
# background it in a shell; logs still go to stderr

vard --color always run 2>vard.log
# force colored logs while redirecting stderr to a file
```

## What it does

On startup the daemon:

1. **Acquires the single-instance lock** for its state directory, so only one vard owns a directory tree at a time. A second daemon contending for the same state directory exits with status `2`.
2. **Loads the config file** into watch specs.
3. **Recovers stale version-control locks** left behind by a previous crash.
4. **Watches every active configured directory** and snapshots changes into version control. A directory whose repository cannot be opened is skipped with an error log rather than stopping the daemon — it is reported as `attention` (kind `unopenable`) by [`status`](status.md) and [`notify`](notify.md), and re-attempted on every reload, so repairing the repository plus a reload brings it back.

While running it:

- **Reloads** on `SIGHUP` or when the config file changes on disk — so [`watch add`](watch.md), [`config set`](config.md), and hand edits take effect without a restart.
- **Rebuilds a watch** whose event source dies, with exponential backoff.
- **Syncs a watch to its remote** automatically after each snapshot, when that watch has syncing enabled (see below).
- **Runs hooks** — your own shell commands — in reaction to snapshot, sync, and daemon events (see [Hooks](#hooks)).
- **Shuts down cleanly** on `SIGINT` (Ctrl-C) or `SIGTERM`.

## Automatic sync

Syncing is **off by default** — a watch syncs only when it opts in with `sync = true` (on the watch or via `defaults.sync = true`), a `branch` and `remote` configured, and a repository that actually defines that remote. The remote is checked **live** each cycle (a cheap, non-network config lookup), so a remote added after the daemon started is picked up on the next sync with no restart, and a remote-less watch is skipped as a logged no-op rather than storming failed fetches.

For such a watch, the daemon runs a sync cycle on three automatic triggers: right after every successful snapshot, on a per-watch **cadence**, and on the failure-backoff retry. The cadence fires every `sync_interval` (default `20m`), with each tick jittered ±10% to de-correlate watches armed together at daemon start and reduce thundering-herd risk; `sync_interval = "0s"` turns the cadence off for a watch (its push-driven and manual syncs are unaffected). A cadence sync applies any remote commits it finds — emitting a `sync.pulled` event when a pull actually changes the tree — and, like every automatic sync, is suppressed while the watch is `conflicted` and waits out any sync-error backoff. A cycle fetches the remote, then — inside a single locked window — commits any uncommitted local work with a pre-sync snapshot, reconciles local history onto the remote **out of tree** (never the working tree), and pushes. The working tree only ever moves between fully-committed states, and the step that makes the reconciled history live refuses rather than overwriting uncommitted work or a commit raced onto the branch.

Outcomes are visible through [`status`](status.md) and [`notify`](notify.md):

- A reconcile **conflict** git cannot resolve latches the watch `conflicted` and **stops** its automatic syncing until the conflict is resolved (local snapshotting continues).
- A network, authentication, or other step failure latches `sync-error` and is re-attempted on an **exponential backoff** (capped), clearing on the next successful cycle.

`vard sync` is the on-demand counterpart — the same engine cycle, run once explicitly. See [`sync`](sync.md) for the full reconcile semantics and per-watch reporting.

## Hooks

Hooks run your own shell commands when the daemon emits an event — a snapshot completed, a sync pulled remote changes, the daemon started. They are configured in the TOML config, not through a CLI flag, so you add them by hand-editing the config (`vard config edit`); the running daemon reloads them on its own.

### Configuring hooks

A hook maps an **event key** to a **shell command**. Watch-scoped events (those about one watch) go under that watch's `[watch.hooks]` table; daemon-level events go under the top-level `[hooks]` table.

```toml
[defaults]
hook_timeout = "60s"        # kill a hook that runs longer (default 60s)
hook_rate_limit = "5m"      # coalescing window per hook (default 5m)

# Daemon-level hooks: events with no watch.
[hooks]
daemon_started = "echo vard is up | logger -t vard"

[[watch]]
name = "dotfiles"
path = "~/.dotfiles"

# This watch's hooks: events carrying this watch.
[watch.hooks]
snapshot_completed = "stow ."          # re-apply dotfiles after every snapshot
sync_pulled = "stow ."                 # and after pulling remote changes
```

Event keys are the event's name with its first `.` written as `_` (so `snapshot.completed` is keyed `snapshot_completed`, `watch.state_changed` is keyed `watch_state_changed`). A misspelled or misplaced key is refused when the config loads, naming the bad key — it never silently does nothing.

**Watch events** (under `[watch.hooks]`): `snapshot_started`, `snapshot_completed`, `snapshot_failed`, `snapshot_skipped`, `snapshot_quarantined`, `sync_pushed`, `sync_pulled`, `sync_conflict`, `sync_resolved`, `sync_failed`, `sync_skipped`, `restore_completed`, `watch_state_changed`.

`snapshot_quarantined` fires when a snapshot pass withheld one or more newly-added files as likely secrets ([secret quarantine](snapshot.md#secret-quarantine)); its hook receives only a **count** (`VARD_QUARANTINED_COUNT`), never the file names or any secret bytes.

**Daemon events** (under `[hooks]`): `daemon_started`, `daemon_stopped`, `update_available`.

`daemon_started` fires when the daemon first starts **and again on every config reload or engine rebuild** (each rebuilds the engine, which re-emits it); it is rate-limited by the loop guard below, so a storm of reloads collapses to at most one hook run per cooldown window. `daemon_stopped` fires only on a real shutdown — a reload deliberately does not emit it for the engine it replaces.

`hook_timeout` and `hook_rate_limit` live in `[defaults]` and each watch may override them in its own `[[watch]]` table; absent everywhere they default to `60s` and `5m`. (Global `[hooks]` use the `[defaults]` values.)

### Execution semantics

Each hook runs as `$SHELL -c "<command>"` (falling back to `/bin/sh` when `$SHELL` is unset or empty), so shell syntax — pipes, `&&`, `$VARD_*` expansions — works as written. Hooks run:

- **Asynchronously.** A hook never blocks the daemon's event loop or delays a snapshot; it runs on a background task.
- **In the watched directory.** The working directory is the watch's path (for a `[hooks]` global hook, the daemon's own working directory).
- **With stdin closed.** Hooks receive no standard input; all context arrives through `VARD_*` environment variables (below).
- **Under a timeout.** A hook still running after `hook_timeout` has its **whole process group** terminated — `SIGTERM`, a short grace, then `SIGKILL` — so a shell's own children die with it and nothing is left running.

A hook's stdout and stderr are captured into the daemon's log (at `debug` on success, `warn` on a non-zero exit or timeout), not the terminal.

### Environment

Every hook is passed an enumerated set of `VARD_*` variables; an unset one is **absent**, never an empty string.

| Variable | Set for | Value |
|---|---|---|
| `VARD_EVENT` | every hook | the dotted event name, e.g. `snapshot.completed` |
| `VARD_SUPPRESSED` | every hook | how many same-key events this run coalesced (see the loop guard) — `0` when it ran immediately |
| `VARD_WATCH` | watch events | the watch's name |
| `VARD_PATH` | watch events | the watch's directory |
| `VARD_REF` | `snapshot.completed`, `sync.pushed`, `sync.pulled`, `restore.completed` | the resulting commit / ref |
| `VARD_PREV_REF` | `sync.pulled`, `restore.completed` | the ref before the change |
| `VARD_FILES_CHANGED` | `snapshot.completed` | number of files changed in the snapshot |
| `VARD_QUARANTINED_COUNT` | `snapshot.quarantined` | number of newly-added files withheld as likely secrets (count only — never the names) |
| `VARD_ERROR` | `snapshot.failed`, `sync.failed` | the failure reason |

There is no file list and no JSON payload: a hook that needs the exact set of changed files reads git history in the watched repo (`VARD_REF` and `VARD_PREV_REF` bound the range).

### The loop guard

A hook can dirty the tree, which snapshots, which fires the hook again. To keep that from running away while never dropping a real event, each hook key — `(watch, event, command)` — is **rate-limited by trailing-edge coalescing**:

- The **first** event runs the hook immediately.
- While it is running or inside its `hook_rate_limit` cooldown, further same-key events do not spawn new runs — they replace a single pending slot (latest wins) and bump a counter.
- When the window opens, the pending event runs **once**, with `VARD_SUPPRESSED` set to how many arrivals it stands in for.

Events accepted by the loop guard are **delayed**, never skipped: the latest coalesced event always runs. (If the event-bus subscriber falls more than the bus capacity behind before the limiter sees an event — rare, and logged — older events can be dropped at the bus itself, upstream of the loop guard.) A single event that arrives during the cooldown is delivered when the window opens with `VARD_SUPPRESSED=1`; an immediate run always sees `VARD_SUPPRESSED=0`. An idempotent apply script (stow, rsync) produces no change on its second pass, so the loop dies in one iteration; a genuinely non-idempotent hook cycles at most once per cooldown window, bounded and visible.

The loop-guard state — each key's cooldown window, its pending trailing slot, and its consecutive-failure streak — **survives a config reload or engine rebuild**: a coalesced event still waiting to run fires when its window opens even if a reload lands in between, and a failing hook keeps its failure count rather than resetting each reload. The one boundary is process shutdown: stopping the daemon drops any not-yet-fired pending event (a hook is delayed across reloads, never across a restart). A hook whose command is changed in the config is a new key, so a pending run queued for the old command is dropped rather than run under the new one.

The per-watch count of coalesced events is shown by [`status`](status.md) as telemetry; it never marks a watch unhealthy and [`notify`](notify.md) stays silent about it.

### Failing hooks

If a hook fails — a **non-zero exit or a timeout** — on **3 consecutive** runs of the same key, the daemon reports it as a `hook-failing` problem: the hook's watch shows `attention` in [`status`](status.md) and [`notify`](notify.md), with the event, command, failure count, and last error. It **clears** on the hook's next success — but that clear reaches `status`/`notify` only at the daemon's next health refresh (a heartbeat or any state-change write), not the instant the hook succeeds; there is nothing to acknowledge. A global `[hooks]` hook that fails this way is reported as a daemon-scoped hook rather than against any watch.

## Output

`run` produces a live stderr log, not records — it does not consume the global `--format`. Use [`status`](status.md) and [`notify`](notify.md) to observe what the running daemon is doing.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown after `SIGINT`/`SIGTERM`. |
| `2` | Could not start — most commonly another daemon already owns the state directory. |

## See also

- [`status`](status.md) — check whether a daemon is running and what state each watch is in.
- [`notify`](notify.md) — the always-on health line for a prompt.
- [`watch`](watch.md) — register the directories `run` will watch.
- [`sync`](sync.md) — the on-demand counterpart to the daemon's automatic syncing.
- Run `vard run --help` for the full, always-current reference.
