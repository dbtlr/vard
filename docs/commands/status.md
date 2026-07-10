---
title: status
description: Show the daemon's liveness and every watch's current state, read-only.
---

# vard status

Show whether the vard [daemon](run.md) is running and what state each watch is in. Read-only and safe to run any time: it probes the single-instance lock to learn whether a daemon is running, reads the small health file the daemon keeps, and reads the config's watch list — it never takes a lock, runs git, or mutates anything. Where [`notify`](notify.md) is the terse, always-on prompt hook that stays silent when all is well, `status` is the on-demand review that lists **every** watch, healthy and paused ones included.

## Examples

```bash
vard status
# the daemon's state plus every watch's state

vard status notes
# narrow to one watch (daemon-level trouble still folds in)

vard status --format json
# a stable JSON array for a status-bar program
```

## Targets

| Argument | Effect |
|---|---|
| `<NAME\|PATH>` | The watch to report, by name or by path. Omit to report every configured watch. With a selector the per-watch part reflects only that watch, but daemon-level trouble always folds in. |

## Output

A list surface (records/json/jsonl). The first line reports the daemon; each watch then reports one state.

```text
⚠ daemon: not running — start it with `vard run`
· notes: unknown
```

The **daemon** line is one of: running, not running, starting, stopping, or — when a running daemon's health file has gone stale — stale.

Each **watch** shows one state:

| State | Meaning |
|---|---|
| `ok` | Healthy. |
| `paused` | A pause you chose (which [`notify`](notify.md) stays silent about). |
| `unknown` | Nothing is monitoring it — the daemon is not running or still starting. |
| `blocked` | The repository is in an unsafe state and snapshots are blocked. |
| `snapshots-failing` | Snapshots are erroring. |
| `attention` | Needs a human. |
| `conflicted` | A merge/sync conflict. |
| `sync-error` | Syncing to the remote is erroring. |

A problem state also reports how long the watch has been in it.

```bash
vard status --format json
```

```json
[{"name":null,"state":"not-running","kind":null,"summary":"not running — start it with `vard run`","since":null,"elapsed_seconds":null,"daemon":true}]
```

In JSON, the daemon row carries a null watch `name` and a `daemon: true` flag; each configured watch is its own object.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The daemon is running and every reported watch is healthy. |
| `1` | Attention — the daemon is not running, starting, or stale, or a reported watch has a problem. |
| `2` | Operational error. |

## See also

- [`notify`](notify.md) — the terse, always-silent-when-healthy variant for a prompt.
- [`run`](run.md) — start the daemon this command probes for.
- [`watch`](watch.md) — pause/resume the watches this command reports on.
- Run `vard status --help` for the full reference.
