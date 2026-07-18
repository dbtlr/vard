---
title: doctor
description: Diagnose the local vard environment read-only — git, inotify limits, health-file freshness, and request-queue hygiene.
---

# vard doctor

Diagnose the local vard environment. `doctor` runs a set of local checks and prints one row per check. It is strictly **read-only and never mutates anything**: it reads `/proc`, the config, the health file, and the request queue, and reports what it finds — it does not clean, restore, or write. The request-dir check, for example, *flags* stale leftovers; it does not delete them.

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

## Output

A list surface (records/json/jsonl). On a terminal each check is a glyph line in the visual register of [`status`](status.md); piped, it is a stable JSON/JSONL array.

```text
✓ git: ok — git 2.55.0 (2.22.0 or newer required)
· inotify: skipped — not applicable on this platform — vard uses FSEvents here, which has no per-user watch-descriptor limit to exhaust
✓ health-file: ok — no daemon is running — a legitimate state; start one with `vard run` to watch your directories
✓ request-dir: ok — no stale leftovers in the request queue
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

`doctor` grows over time. Planned for later releases: a **per-watch secret audit** (flagging already-tracked files whose *name* is secret-shaped, the complement to snapshot quarantine), and a **remote-authentication probe** with an `--offline` flag to skip it. Agent/keychain and service-linger checks are deferred to the service-install command (VRD-24).

## See also

- [`status`](status.md) — the daemon liveness and per-watch state this shares a visual register with.
- [`run`](run.md) — start the daemon whose health file this checks.
- Run `vard doctor --help` for the full reference.
