---
title: restore
description: Restore a watch's tree (or one file) to a prior snapshot, protecting current state first.
---

# vard restore

Restore a watch's working tree, or a single file within it, to a prior state. Where [`diff`](diff.md) shows what a prior state differs from and [`log`](log.md) lists the points you can go back to, `restore` actually applies one — and it can never destroy uncommitted work, because **before touching the tree it always takes a protective snapshot of the current state first.** The state you are about to overwrite is committed to history and recoverable from the [log](log.md).

## Examples

```bash
vard restore notes --ref 67a3776f
# restore the whole tree to snapshot 67a3776f

vard restore notes --at 2h
# restore to the snapshot current two hours ago

vard restore notes --at 2026-07-01
# restore to the state as of the END of July 1 (UTC)

vard restore notes --ref HEAD --file a.md
# restore just a.md from the last snapshot

vard restore notes --ref 67a3776f --dry-run
# preview what a restore would overwrite, changing nothing
```

## Targets and options

| Argument / flag | Effect |
|---|---|
| `<NAME\|PATH>` | The watch to restore, by name or by path. Required. |
| `--ref <SHA>` | Restore from this revision — a snapshot id, or any revision git understands. Mutually exclusive with `--at`. |
| `--at <WHEN>` | Restore the snapshot current as of a past time. Mutually exclusive with `--ref`. See below. |
| `--file <SUBPATH>` | Restore only this path (relative to the watch root) instead of the whole tree. |
| `--dry-run` | Preview the differences a restore would overwrite, without changing the tree or taking the protective snapshot. |

Choose the point to restore from with **exactly one** of `--ref` or `--at`.

### `--at <WHEN>` forms

`--at` takes the snapshot current as of a past time:

- a **duration** counted back from now — `2h`, `3d`;
- an **absolute UTC date/time** `YYYY-MM-DDThh:mm` (the `T` needs no shell quoting);
- a **bare date** `YYYY-MM-DD`, which means the **end** of that day (state as of that day);
- the **space form** `YYYY-MM-DD hh:mm` also works but must be quoted.

Natural-language forms like `yesterday 3pm` are deliberately **not** supported and are rejected.

## Behavior

`--dry-run` previews the differences via a diff without changing anything (and without taking the protective snapshot, since nothing is modified). A whole-tree dry-run excludes files added after the chosen point, which a real restore keeps rather than removes.

```text
vard: dry-run: the diff below is what a restore of "notes" to HEAD would overwrite
diff --git a/a.md b/a.md
...
```

A real restore reports the protective snapshot it took first:

```text
restored notes to HEAD (protective snapshot 64268508b0b79c62716f868eb31a20c1843f5a7d)
```

If the [daemon](run.md) is running it keeps ownership of the repository; the restore still proceeds and the daemon snapshots the restored state afterward — by design. The restore takes the watch's per-watch operation lock across both the protective snapshot and the checkout, so it serializes against the daemon's own worker and records a recoverable journal entry **whether or not a daemon is running** — a crash mid-restore leaves a record the next daemon start (or a later `vard watch remove`) uses to prove any leftover git lock stale and clean it. If a daemon's worker is mid-commit on that same watch, the restore reports that another operation holds the lock and changes nothing; retry in a moment. Restoring a path that does not exist at the chosen reference reports a friendly error naming the path and the reference.

The recoverable journal record is guaranteed on every real restore **except** one case: if the operation lock itself cannot be taken (an op-lock or journal I/O failure) *while a daemon is running*, the restore fails closed — it cannot prove exclusion against the daemon's worker, so it changes nothing and reports that it could not take the watch's operation lock (retry). With no daemon running the CLI is the sole writer, so the same I/O trouble is non-fatal: the restore proceeds under git's own index lock and only the recovery record is missing.

One more deferral protects a prior crash's recovery evidence: if an earlier operation on the watch left a dangling journal record plus a git lock that is not yet provably stale — its owner is gone but the lock is still within the freshness window a live foreign process could have created it in — the restore, under a running daemon, declines rather than overwrite that record, and reports that a prior operation's lock is still being verified (retry in a few minutes). It clears on its own once the lock ages past the window and the next attempt proves it stale. With no daemon running the restore proceeds instead (the CLI is the sole vard writer); the still-present git lock then surfaces as git's own index-lock error, and the recovery record is preserved either way.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The restore (or dry-run) completed. |
| `1` | Nothing was changed and the watch needs attention — the repository is not in a safe state, another operation holds the watch's lock (retry), or, under a running daemon, the operation lock could not be taken. |
| `2` | Operational error — an unresolved reference or path, both `--ref` and `--at` given, or an unparseable `--at`. |

## See also

- [`log`](log.md) — find the snapshot id or time to restore to.
- [`diff`](diff.md) — the same view `--dry-run` prints, for any two points.
- [`snapshot`](snapshot.md) — the protective snapshot restore takes is an ordinary snapshot.
- Run `vard restore --help` for the full reference.
