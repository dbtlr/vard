---
title: sync
description: Sync a watch with its remote now — fetch, reconcile, and push.
---

# vard sync

Reconcile a watch with its remote now: fetch the remote, reconcile local and remote history, and push. It is the on-demand counterpart to the daemon's automatic syncing — where the [daemon](run.md) syncs automatically (right after each successful snapshot, on a per-watch **cadence**, and re-attempting a failed sync on an exponential backoff), `vard sync` is a single explicit request. The cadence runs every `sync_interval` (default `20m`), jittered ±10% each tick so a fleet of watches is unlikely to sync in lockstep; set `sync_interval = "0s"` to turn the cadence off for a watch (its push-driven and manual syncs still work). A cadence sync applies any remote changes (emitting `sync.pulled`) and, like the other automatic triggers, is suppressed while a watch is `conflicted` and waits out any sync-error backoff — a manual `vard sync` is what breaks a conflict. With no selector every sync-enabled watch is acted on and reported (non-paused ones are synced; paused or remote-less ones get informational rows — see below); with a `<name|path>` only that one is.

The reconcile happens **out of tree**: local history is rebased onto the fetched remote inside a scratch worktree, never the working tree. A sync can never destroy uncommitted work: any uncommitted local work is committed by a pre-sync snapshot before it can be moved, and the step that makes the reconciled history live **refuses and retries rather than overwriting** if a local change (or a commit raced onto the branch) would be clobbered. The working tree only ever moves between fully-committed states.

Syncing is **off by default**, so it must be enabled for the watch — the one-step way is [`vard watch sync <name>`](watch.md#sync), which writes `sync = true` and runs a first confirming cycle; you can also set the `sync` key (or `defaults.sync`) to `true` by hand. Either way, a `branch` and `remote` must be configured, and the repository must actually have that remote. The remote is checked **live**, when the cycle runs (a cheap, non-network config lookup), not once at startup: a remote added after the daemon started is picked up on the next sync with no restart, and a request on a remote-less watch is answered honestly rather than silently dropped.

> **Upgrading from a default-on build:** syncing used to be **on** by default; it is now off unless explicitly enabled (`DEFAULT_SYNC` flipped off). A watch that relied on the old default now needs a one-time opt-in — run `vard watch sync <name>` for it (or set `sync = true` in the config). Watches that already set `sync = true` explicitly are unaffected.

With **no selector**, every sync-enabled watch gets a row: non-paused ones are synced; a paused one appears as an informational `paused` row (it is not synced, exactly as the daemon does not supervise it — and does not by itself change the exit code); one whose repository lacks the configured remote appears as an informational `disabled` row whose `detail` names the missing remote. Neither informational row makes the command fail — if every sync-enabled watch is paused, the command reports the paused rows and exits `0`. Asking for a watch **by name** that has syncing disabled, is paused (a paused watch never syncs, with or without a daemon), or whose repository has no such remote is reported and exits `1`.

## Examples

```bash
vard sync
# sync every sync-enabled watch with its remote now

vard sync notes
# fetch, reconcile, and push just the "notes" watch

vard sync --format json
# machine-readable result rows for a script
```

## Targets and options

| Argument / flag | Effect |
|---|---|
| `<NAME\|PATH>` | The watch to sync, by name or by path. Omit to sync every sync-enabled watch. |

## How the sync runs

- **If the daemon is running**, it owns the repositories, so the sync is handed to it as a request and runs asynchronously. The command reports that the request was *queued*, not the cycle result.
- **With no daemon running**, one cycle runs in-process under the single-instance lock — the same engine cycle the daemon drives — and the result is reported per watch.

A cycle **fetches first**, then — inside a single locked window — commits any uncommitted local work (a pre-sync snapshot, tagged with a `Vard-Host` trailer naming the machine) and, when the fetch found remote commits to integrate, reconciles out of tree and advances; it then pushes. With nothing new remotely there is nothing to reconcile, so the cycle pushes directly — which is also what makes the **first push** of a branch the remote has never seen (a fresh repository against an empty remote) work: there is no upstream yet to reconcile against. The advance never overwrites uncommitted work, a commit raced onto the branch, or a local **gitignored** file a remote change would clobber: it refuses and the cycle is re-attempted (the next cycle's pre-sync snapshot commits the new work and reconciles it properly). That re-attempt is **bounded** — a path being rewritten continuously so the advance keeps refusing terminates as a failure rather than looping forever. A reconcile that hits a conflict git cannot resolve **latches** the watch `conflicted` and stops automatic syncing for it until the conflict is resolved; the command reports it and exits `1`. A network or authentication failure is reported and exits `2`.

## Output

A list surface (records/json/jsonl). One row per sync-enabled watch acted on, reporting what the cycle did.

```text
1 syncs
────────────────────────────────────────────────────────────
  name     notes
  status   pushed
  detail   —
  commits  1
  ref      —
```

Possible `status` values: `pushed` (with the `commits` count), `pulled`, `synced` (both pushed and pulled), `up to date` (nothing to do), `conflict`, `failed` (a broken step, or a repository that could not be opened at all — one broken repository never blocks the other watches from syncing), `disabled` (syncing is off for the watch, or its repository lacks the configured remote — the `detail` says which, naming the missing remote), `paused` (the watch is paused; resume it to sync), `did not run` (the request could not complete before the engine stopped, or another operation held the repository throughout), and `requested` (handed to a running daemon).

```bash
vard sync notes --format json
```

```json
[{"name":"notes","status":"up to date","detail":null,"commits":null,"ref":null}]
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Every acted-on watch was synced or queued (including `up to date`), or was an informational no-selector row (`paused`, or `disabled` for a missing remote). |
| `1` | Attention — a reconcile conflict latched a watch; a watch asked for **by name** has syncing disabled, is paused (a paused watch never syncs, with or without a daemon), or its repository lacks the configured remote (the message names it); or, with **no selector**, no watch has syncing enabled at all (`no sync-enabled watches configured`). |
| `2` | Operational error — a network or authentication failure, a repository that could not be opened, a sync that could not complete before the engine stopped (including a repository held by another operation throughout), or a selector that resolves to no watch. |

## See also

- [`watch sync`](watch.md#sync) — the one-step opt-in that turns syncing on for a watch and confirms it.
- [`run`](run.md) — the daemon that syncs automatically.
- [`snapshot`](snapshot.md) — commit local state; a successful snapshot also triggers a sync.
- [`status`](status.md) — see whether a watch is `conflicted` or `sync-error`.
- [`config`](config.md) — the `sync`, `branch`, and `remote` keys that enable syncing.
- Run `vard sync --help` for the full reference.
