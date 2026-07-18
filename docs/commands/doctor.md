---
title: doctor
description: Diagnose the local vard environment read-only — git, inotify limits, health-file freshness, request-queue hygiene, a per-watch secret audit, and a per-watch remote-auth probe.
---

# vard doctor

Diagnose the local vard environment. `doctor` runs a set of checks and prints one row per check. It is strictly **read-only and never mutates anything**: it reads `/proc`, the config, the health file, the request queue, and each watch's repository, and reports what it finds — it does not clean, restore, or write. The request-dir check, for example, *flags* stale leftovers; it does not delete them. Its one network check — the remote-auth probe — is a read-only `git ls-remote` that lists a remote's refs and discards them.

## Examples

```bash
vard doctor
# one row per check, with a glyph and a one-line explanation

vard doctor --offline
# skip the network checks (remote-auth); run the local checks only

vard doctor --format json
# a stable JSON array for a script or a setup wizard
```

## Checks

Every check but `remote-auth` is local; `remote-auth` is the one that touches the network, and `--offline` skips it.

| Check | What it verifies |
|---|---|
| `git` | The `git` executable is on `PATH` and new enough. vard's snapshot log format reads the trigger trailer with `%(trailers:key=…,valueonly)`, which needs **git 2.22+**; an older git `warn`s and a missing git `fail`s. |
| `inotify` | **Linux only.** The kernel's inotify limits (`max_user_watches`, `max_user_instances`) against how many directories the configured watches would register (the notify backend watches every directory in a tree recursively, one watch descriptor each). It `warn`s once the totals reach 80% of either limit. On macOS this is `skipped` — vard uses FSEvents there, which has no such per-user limit. |
| `health-file` | Whether the daemon's health file is fresh. A running daemon whose file has gone stale (past the staleness window) `warn`s — it may be wedged or unable to write. A daemon that is **not** running is a legitimate state, reported `ok` with a note (start one with [`vard run`](run.md)). |
| `request-dir` | Stale entries in the request queue, older than the request staleness window. Three distinct cases, all `warn`, all flag-only (doctor never deletes): **crashed-writer leftovers** — files matching vard's own atomic-write temp scheme (`.<name>.tmp-<pid>`) that an interrupted write stranded, safe to delete; **settled requests piling up unconsumed**, which mean no daemon is draining the queue (a running daemon would discard them as stale anyway); and **unrecognized files** — anything else stale that vard did not write, flagged with "investigate before deleting" rather than called safe to remove. A queue directory that cannot be read (short of simply not existing) also `warn`s, naming the I/O error, so a read failure is never reported `ok`. |
| `secret-audit` | Per configured watch, whether any already-tracked file has a secret-shaped **name**. See [the secret audit](#the-secret-audit) below. |
| `remote-auth` | Per sync-enabled watch, whether the configured remote is reachable and authenticated. See [the remote-auth probe](#the-remote-auth-probe) below. |

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

## The remote-auth probe

For every **sync-enabled** watch, `doctor` probes that the watch's configured remote is reachable and that you are authenticated to it — the equivalent of `git ls-remote` against the remote. It is **read-only**: it lists the remote's refs and discards them, writing to neither repository. `GIT_TERMINAL_PROMPT=0` and a wall-clock timeout are set so a dead VPN or a remote that wants to prompt for credentials **fails fast** instead of hanging `doctor`. One row per watch, each probed independently — one bad remote never blocks another watch's row.

Per-watch outcomes:

| Outcome | Row |
|---|---|
| The remote answered and authentication succeeded | `ok`. |
| The remote is unreachable, refused authentication, or the probe timed out | `fail` — with git's reason (its first stderr line, not a dump), sanitized so any credential embedded in a remote URL (`scheme://user:token@host`) is redacted to `scheme://***@host` before it is shown. |
| The watch does not sync (`sync = false`), or syncs but has no remote defined in its repository | `skipped` — with the reason. |
| `--offline` was passed | `skipped` — "offline mode", without touching the network. |
| The repository cannot be opened | `warn` naming the watch — never a crash, and never a block on the other watches' rows (consistent with the secret audit). |

Pass `--offline` to skip this probe entirely (a dead network, or a deliberately local-only run); the remaining local checks still run and the row reads `skipped`.

## Output

A list surface (records/json/jsonl). On a terminal each check is a glyph line in the visual register of [`status`](status.md); piped, it is a stable JSON/JSONL array.

```text
✓ git: ok — git 2.55.0 (2.22.0 or newer required)
· inotify: skipped — not applicable on this platform — vard uses FSEvents here, which has no per-user watch-descriptor limit to exhaust
✓ health-file: ok — no daemon is running — a legitimate state; start one with `vard run` to watch your directories
✓ request-dir: ok — no stale leftovers in the request queue
✓ secret-audit: ok — watch "notes": no tracked file has a secret-shaped name
✓ remote-auth: ok — watch "notes": remote "origin" is reachable and authenticated
```

Each row carries a stable machine shape: `check`, `status`, and `detail`, plus a `watch` field on **per-watch** rows (`secret-audit`, `remote-auth`). A global row omits `watch` entirely, so a machine consumer reads the watch name from the field rather than parsing it out of the `detail` prose.

```bash
vard doctor --format json
```

```json
[
  {"check":"git","status":"ok","detail":"git 2.55.0 (2.22.0 or newer required)"},
  {"check":"remote-auth","status":"ok","watch":"notes","detail":"watch \"notes\": remote \"origin\" is reachable and authenticated"}
]
```

| Status | Meaning |
|---|---|
| `ok` | The check passed. |
| `warn` | A soft problem worth a look (an old git, tight inotify headroom, a stale health file, a stranded request leftover, a repository that could not be opened for a per-watch check). |
| `fail` | A hard problem (git is missing, or a sync-enabled watch's remote is unreachable or refused authentication). |
| `skipped` | The check does not apply here (inotify on macOS; a non-syncing watch, or remote-auth under `--offline`). |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Every check is `ok` or `skipped`. |
| `1` | At least one check `warn`ed or `fail`ed — something needs attention. |
| `2` | doctor itself could not run (an unresolvable state directory, or an invalid config it could not read the watch list from). |

## Not yet covered

`doctor` grows over time. Agent/keychain and service-linger checks are deferred to the service-install command (VRD-24).

## See also

- [`status`](status.md) — the daemon liveness and per-watch state this shares a visual register with.
- [`run`](run.md) — start the daemon whose health file this checks.
- Run `vard doctor --help` for the full reference.
