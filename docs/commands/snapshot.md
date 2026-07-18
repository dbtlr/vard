---
title: snapshot
description: Take a manual snapshot now — sweep a watch and commit its current state.
---

# vard snapshot

Take a manual snapshot now: sweep a watched directory and commit its current state into version control — the same operation the [daemon](run.md) performs automatically, just on demand. Where [`run`](run.md) snapshots continuously in the background, `snapshot` is a single explicit request. With no selector every configured watch is snapshotted; with a `<name|path>` only that one is.

Paused watches are snapshotted too when you name one directly and no daemon is running: a manual snapshot is explicit intent, and pausing only stops the daemon's *automatic* snapshots.

## Examples

```bash
vard snapshot
# snapshot every configured watch

vard snapshot notes
# snapshot just the "notes" watch

vard snapshot notes -m "before the big reorg"
# prepend a message paragraph to the generated snapshot subject

vard snapshot --format json
# machine-readable result rows for a script
```

## Targets and options

| Argument / flag | Effect |
|---|---|
| `<NAME\|PATH>` | The watch to snapshot, by name or by path. Omit to snapshot every configured watch. |
| `-m`, `--message <MSG>` | A message paragraph prepended to the generated snapshot subject. |

## How the snapshot is taken

- **If the daemon is running**, it owns the repositories, so the snapshot is handed to it as a request and taken asynchronously. The command reports that the request was *queued*, not the commit result.
- **With no daemon running**, the snapshot is taken in-process under the single-instance lock, and the new commit (or `no changes`) is reported per watch.

A repository that is not in a safe state — mid-merge, mid-rebase, on the wrong branch, or with a detached HEAD — is skipped with an explanation and the command exits `1`, never committing into an in-progress operation. Finish the merge/rebase (or leave the wrong branch) and re-run.

Requesting a snapshot of a paused watch **while a daemon is running** exits `1` rather than silently queuing work the daemon will drop — resume it, or stop the daemon to snapshot in-process.

## Secret quarantine

An in-process snapshot (no daemon running) scans every **newly-added** file for likely secrets before committing and **withholds** any match from the commit — the same per-watch scanning the [daemon](run.md) does on its own passes. This catches a secret-shaped filename (`.env`, `id_rsa`, `*.pem`, plus the watch's own `secret_patterns`) or a file whose content carries a recognizable credential (an AWS/GitHub/… token, a PEM private key, a high-entropy secret). Modified files already tracked are never scanned — only new ones.

A withheld file **stays on disk, uncommitted**; nothing is deleted or moved. The command prints a warning to stderr naming each withheld path and why, and then **still exits `0`** — quarantine is a warning, not a failure:

```text
vard: notes: withheld 1 newly-added file as a likely secret — each stays on disk, uncommitted:
vard:   creds.txt — contains AWS access key
vard: to snapshot one anyway, move it out of the watch, or set `secret_scan = false` for this watch to turn scanning off
```

If the withheld file was the *only* change, the snapshot commits nothing and the watch's row reads `quarantined` (not `no changes`). If legitimate changes committed alongside a withheld secret, the row is `committed` and its `detail` notes the withhold.

To include a file the scanner flags, move it out of the watched directory, or turn scanning off for the watch by setting `secret_scan = false` (see [`config`](config.md#secret-scanning-secret_scan--secret_patterns)). `secret_patterns` adds your own secret filename shapes on top of the built-in catalog.

When a **daemon is running** it owns the scanning: the snapshot is handed to it and the daemon quarantines on its pass, surfacing any withhold through [`status`](status.md)/[`notify`](notify.md) as a `secret-quarantined` problem rather than on this command's stderr.

## Output

A list surface (records/json/jsonl). One row per watch acted on, reporting the commit status.

```text
1 snapshots
────────────────────────────────────────────────────────────
  name     notes
  status   committed
  detail   —
  id       67a3776f99e54957fbe59ca359c05b4c83c11f21
  subject  snapshot: 1 changed, 1 added (a.md, b.md)
```

A watch with nothing to commit reports `no changes` and a null `id`:

```text
  name     notes
  status   no changes
  id       —
```

```bash
vard snapshot notes --format json
```

```json
[{"name":"notes","status":"no changes","detail":null,"id":null,"subject":null}]
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Every named watch was snapshotted or queued (including `no changes`, and including a snapshot that only `quarantined` a secret — that is a warning, not a failure). |
| `1` | A watch was skipped — an unsafe repository state, a paused watch requested while the daemon is running, or its lock is held by another operation (retry). |
| `2` | Operational error (e.g. a selector that resolves to no watch). |

## See also

- [`run`](run.md) — the daemon that snapshots automatically.
- [`log`](log.md) — review the snapshots this command created.
- [`diff`](diff.md) — see what a snapshot would capture before taking it.
- Run `vard snapshot --help` for the full reference.
