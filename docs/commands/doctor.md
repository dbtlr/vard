---
title: doctor
description: Diagnose the local vard environment read-only — git, inotify limits, health-file freshness, request-queue hygiene, and a per-watch secret audit.
---

# vard doctor

Diagnose the local vard environment. `doctor` runs a set of local checks and prints one row per check. It is strictly **read-only and never mutates anything**: it reads `/proc`, the config, the health file, the request queue, and each watch's repository, and reports what it finds — it does not clean, restore, or write. The request-dir check, for example, *flags* stale leftovers; it does not delete them.

## Examples

```bash
vard doctor
# one row per check, with a glyph and a one-line explanation

vard doctor --format json
# a stable JSON array for a script or a setup wizard
```

## Checks

All checks in this release are local — none touch the network.

| Check | What it verifies |
|---|---|
| `git` | The `git` executable is on `PATH` and new enough. vard's snapshot log format reads the trigger trailer with `%(trailers:key=…,valueonly)`, which needs **git 2.22+**; an older git `warn`s and a missing git `fail`s. |
| `inotify` | **Linux only.** The kernel's inotify limits (`max_user_watches`, `max_user_instances`) against how many directories the configured watches would register (the notify backend watches every directory in a tree recursively, one watch descriptor each). It `warn`s once the totals reach 80% of either limit. On macOS this is `skipped` — vard uses FSEvents there, which has no such per-user limit. |
| `health-file` | Whether the daemon's health file is fresh. A running daemon whose file has gone stale (past the staleness window) `warn`s — it may be wedged or unable to write. A daemon that is **not** running is a legitimate state, reported `ok` with a note (start one with [`vard run`](run.md)). |
| `request-dir` | Stale leftovers a crashed request writer stranded in the queue — unsettled temp/dot names older than the request staleness window. It `warn`s with the file names and a note that they are safe to delete. doctor only flags them; deleting them is left to you. |
| `secret-audit` | Per configured watch, whether any already-tracked file has a secret-shaped **name**. See [the secret audit](#the-secret-audit) below. |

## The secret audit

`doctor` audits every configured watch for **already-tracked files whose name is secret-shaped** — `.env`, `id_rsa`, `*.pem`, plus that watch's own `secret_patterns` (the same catalog snapshot [secret quarantine](snapshot.md) uses). One row per watch.

This is the **complement to quarantine**. Quarantine keeps *newly-added* secrets out of history — but a secret committed before scanning was ever enabled, or force-added past the excludes, is already tracked, and quarantine can never reach back for it. The audit is the check that catches exactly that: a tracked secret-shaped name is a `fail`, with the count and up to five example repo-relative paths and a note that they are already committed.

The audit is **filename-only by contract**. It runs the filename layer over each tracked path; it deliberately does **not** scan the *contents* of tracked files. A tracked file with an innocent name (`notes.txt`) but secret-shaped content is **not** flagged — content-scanning committed history is a heavier, separately-scoped job. (Quarantine still scans the *content* of new files at snapshot time; that is a different pass.)

Per-watch outcomes:

| Outcome | Row |
|---|---|
| Tracked secret-shaped names found | `fail` — the count and example paths (already committed). |
| None found | `ok`. |
| `secret_scan = false` for the watch | `skipped` — a deliberate opt-out, not a problem. |
| Repository cannot be opened, its tree cannot be listed, or an extra pattern is invalid | `warn` naming the watch — never a crash, and never a block on the other watches' rows. |

Move or delete a flagged file (and, if it should never have been committed, purge it from history with a tool such as `git filter-repo`), or add its shape to the watch's excludes. Configure scanning per watch with `secret_scan` / `secret_patterns` (see [`config`](config.md)).

## Output

A list surface (records/json/jsonl). On a terminal each check is a glyph line in the visual register of [`status`](status.md); piped, it is a stable JSON/JSONL array.

```text
✓ git: ok — git 2.55.0 (2.22.0 or newer required)
· inotify: skipped — not applicable on this platform — vard uses FSEvents here, which has no per-user watch-descriptor limit to exhaust
✓ health-file: ok — no daemon is running — a legitimate state; start one with `vard run` to watch your directories
✓ request-dir: ok — no stale leftovers in the request queue
✓ secret-audit: ok — watch "notes": no tracked file has a secret-shaped name
```

Each row carries a stable machine shape:

```bash
vard doctor --format json
```

```json
[{"check":"git","status":"ok","detail":"git 2.55.0 (2.22.0 or newer required)"}]
```

| Status | Meaning |
|---|---|
| `ok` | The check passed. |
| `warn` | A soft problem worth a look (an old git, tight inotify headroom, a stale health file, a stranded request leftover). |
| `fail` | A hard problem (git is missing). |
| `skipped` | The check does not apply here (inotify on macOS). |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Every check is `ok` or `skipped`. |
| `1` | At least one check `warn`ed or `fail`ed — something needs attention. |
| `2` | doctor itself could not run (an unresolvable state directory, or an invalid config it could not read the watch list from). |

## Not yet covered

`doctor` grows over time. Planned for a later release: a **remote-authentication probe** with an `--offline` flag to skip it. Agent/keychain and service-linger checks are deferred to the service-install command (VRD-24).

## See also

- [`status`](status.md) — the daemon liveness and per-watch state this shares a visual register with.
- [`run`](run.md) — start the daemon whose health file this checks.
- Run `vard doctor --help` for the full reference.
