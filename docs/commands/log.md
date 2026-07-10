---
title: log
description: Show a watch's snapshot history, most recent first.
---

# vard log

Show a watch's snapshot history, most recent first. Read-only: it reads the watch's version-control log directly and never takes a lock or mutates anything, so it is safe to run against a watch the [daemon](run.md) is actively snapshotting. Where [`diff`](diff.md) shows *what* changed between two points, `log` shows *when* each snapshot happened and why.

## Examples

```bash
vard log notes
# every snapshot of "notes", newest first

vard log notes --since 2h
# only snapshots from the last two hours

vard log notes --since 3d
# only snapshots from the last three days

vard log notes --format jsonl
# one JSON object per snapshot, for a pipeline
```

## Targets and options

| Argument / flag | Effect |
|---|---|
| `<NAME\|PATH>` | The watch whose history to show, by name or by path. Required. |
| `--since <DURATION>` | Keep only snapshots at or after this far in the past, as a humane duration counted back from now — `2h`, `3d`, `1h30m`. |

## Output

A list surface (records/json/jsonl). Each snapshot reports its id, time, subject, and trigger — the trigger names why the snapshot was taken (`manual`, `pre-restore`, an automatic trigger, or `—` for a snapshot vard did not author, such as your own `init` commit).

```text
2 snapshots
────────────────────────────────────────────────────────────
  id       67a3776f99e54957fbe59ca359c05b4c83c11f21
  time     2026-07-10T17:47:19Z
  subject  snapshot: 1 changed, 1 added (a.md, b.md)
  trigger  manual
────────────────────────────────────────────────────────────
  id       876f656c71f0aade54788ec3ba450ea1bd6cd482
  time     2026-07-10T17:47:07Z
  subject  init
  trigger  —
```

```bash
vard log notes --format json
```

```json
[{"id":"67a3776f99e54957fbe59ca359c05b4c83c11f21","time":"2026-07-10T17:47:19Z","subject":"snapshot: 1 changed, 1 added (a.md, b.md)","trigger":"manual"},{"id":"876f656c71f0aade54788ec3ba450ea1bd6cd482","time":"2026-07-10T17:47:07Z","subject":"init","trigger":null}]
```

An `id` from this log is exactly what [`diff`](diff.md) and [`restore --ref`](restore.md) accept as a reference.

## See also

- [`diff`](diff.md) — the actual changes between a snapshot and the working tree.
- [`restore`](restore.md) — roll a watch back to one of these snapshots.
- [`snapshot`](snapshot.md) — add to this history on demand.
- Run `vard log --help` for the full reference.
