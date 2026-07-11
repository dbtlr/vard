---
title: sync
description: Sync a watch with its remote now — fetch, reconcile, and push.
---

# vard sync

Reconcile a watch with its remote now: fetch the remote, reconcile local and remote history, and push. It is the on-demand counterpart to the daemon's automatic syncing — where the [daemon](run.md) syncs right after each snapshot (and re-attempts a failed sync on an exponential backoff), `vard sync` is a single explicit request. There is no fixed-interval "cadence" sync yet; automatic syncs are driven by snapshots and by failure-backoff retries. With no selector every sync-enabled watch is synced; with a `<name|path>` only that one is.

The reconcile happens **out of tree**: local history is rebased onto the fetched remote inside a scratch worktree, never the working tree. A sync can never destroy uncommitted work: any uncommitted local work is committed by a pre-sync snapshot before it can be moved, and the step that makes the reconciled history live **refuses and retries rather than overwriting** if a local change (or a commit raced onto the branch) would be clobbered. The working tree only ever moves between fully-committed states.

Syncing must be enabled for the watch — its `sync` key (or `defaults.sync`) must be on, with a `branch` and `remote` configured — and its repository must actually have that remote. A watch without syncing enabled, or whose repository has no such remote, is reported and skipped; asking for one by name exits `1`.

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

A cycle **fetches first**, then — inside a single locked window — commits any uncommitted local work (a pre-sync snapshot, tagged with a `Vard-Host` trailer naming the machine), reconciles out of tree, and advances; it then pushes. The advance never overwrites uncommitted work or a commit raced onto the branch: it refuses and the cycle is re-attempted (the next cycle's pre-sync snapshot commits the new work and reconciles it properly). A reconcile that hits a conflict git cannot resolve **latches** the watch `conflicted` and stops automatic syncing for it until the conflict is resolved; the command reports it and exits `1`. A network or authentication failure is reported and exits `2`.

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

Possible `status` values: `pushed` (with the `commits` count), `pulled`, `synced` (both pushed and pulled), `up to date` (nothing to do), `conflict`, `failed`, `disabled` (syncing is off for the watch, or its repository has no configured remote — the `detail` says which), `did not run` (the request could not complete before the engine stopped), and `requested` (handed to a running daemon).

```bash
vard sync notes --format json
```

```json
[{"name":"notes","status":"up to date","detail":null,"commits":null,"ref":null}]
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Every named watch was synced or queued (including `up to date`). |
| `1` | Attention — a reconcile conflict latched a watch, or a named watch has syncing disabled (or its repository has no configured remote). |
| `2` | Operational error — a network or authentication failure, a sync that could not complete before the engine stopped, or a selector that resolves to no watch. |

## See also

- [`run`](run.md) — the daemon that syncs automatically.
- [`snapshot`](snapshot.md) — commit local state; a successful snapshot also triggers a sync.
- [`status`](status.md) — see whether a watch is `conflicted` or `sync-error`.
- [`config`](config.md) — the `sync`, `branch`, and `remote` keys that enable syncing.
- Run `vard sync --help` for the full reference.
