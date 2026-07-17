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

A list surface (records/json/jsonl). The first line reports the daemon; each watch then reports its state. A watch holding more than one problem contributes one row per problem, and a failing global `[hooks]` hook — which belongs to no watch — adds its own daemon-scoped `[hooks]` row (see below).

```text
⚠ daemon: not running — start it with `vard run`
· notes: unknown
```

The **daemon** line is one of: running, not running, starting, stopping, or — when a running daemon's health file has gone stale — stale.

Each **watch** row shows one state (a watch with several problems contributes one such row per problem):

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

`snapshots-failing` (a backend call erroring) and an `attention` caused by a panicked backend call clear themselves the moment that watch proves itself healthy again — a snapshot commits, or the tree is found clean — without any action on your part; there is nothing to acknowledge. An `attention` caused by the watch's signal source dying is different: it clears only once the daemon finishes rebuilding it (automatic, but not instant — a watch may briefly still read `attention` right after the underlying problem is gone). While that rebuild is pending, the reported reason keeps the source-died detail even if a snapshot failure arises in the meantime (the event log still records the failure). The reverse edge exists too: right after a rebuild, a source that dies again may briefly read `ok` before it is re-flagged, because a rebuilt watch starts healthy and only probes repository safety up front. One `attention` cause does require you to act: a watch whose repository could not be **opened** (kind `unopenable`, below) stays flagged until the repository is repaired and the daemon reloads. The path-alias case below is reported as `attention` too, but it clears differently again: it is recomputed fresh from your config on every run, so it disappears once the duplicate config entry is fixed.

A watch whose path canonicalizes onto an earlier watch's — two config entries that resolve to the same repository, for example a path and a symlink to it — is reported as `attention` with the summary `path aliases watch '<other>'; not supervised`. The daemon supervises only the first of the pair, so the later one is flagged rather than shown as `ok`.

A watch whose repository **cannot be opened** (a corrupt or deleted `.git`, a directory that was never initialized) is likewise never shown as `ok`: the daemon skips it at engine build rather than letting one broken repository stop every healthy watch, and reports it as `attention` with kind `unopenable` and a summary naming the open error. It is not being snapshotted while flagged. Repair the repository, then reload the daemon (`SIGHUP`, or any config-file change) — the next engine build re-opens every watch from scratch and picks it back up.

A watch with a [hook](run.md#hooks) that has failed on **3 consecutive** runs (a non-zero exit or a timeout) is reported as `attention` with kind `hook-failing`, and the summary names the event, the command, the failure count, and the last error. Like `snapshots-failing`, it **clears itself** the moment that hook next succeeds — there is nothing to acknowledge. A failing **global** `[hooks]` hook has no watch to attach to, so it is reported on its own row labeled `[hooks]`; being daemon-scope (like the daemon line itself), that row shows and counts even when you pass a selector.

A single watch can hold more than one problem at once — for example a sync `conflicted` *and* a `hook-failing` hook. Both are shown, on their own rows for that watch, engine problem first and the hook problem after; neither hides the other, and each counts toward the exit code.

A watch that has **coalesced** hook events (the [loop guard](run.md#the-loop-guard) delayed some same-key hook runs) shows a trailing `(N hook events coalesced)` on its line. This is pure telemetry: it never changes the watch's state (a healthy watch stays `ok`), never counts toward the exit code, and is omitted entirely when the count is zero.

```bash
vard status --format json
```

```json
[{"name":null,"state":"not-running","kind":null,"summary":"not running — start it with `vard run`","since":null,"elapsed_seconds":null,"suppressed":null,"daemon":true}]
```

In JSON, the daemon row carries a null watch `name` and a `daemon: true` flag; each configured watch is its own object. A watch's coalesced-hook count is the `suppressed` field (null when zero).

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
