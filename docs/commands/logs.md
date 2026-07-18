---
title: logs
description: Show the vard daemon's own log — the rolling logfile it writes while running.
---

# vard logs

Show the vard daemon's own log output. While the [daemon](run.md) runs it writes its log to a daily-rolling file set under the state directory (`~/.local/state/vard/logs/vard.log.YYYY-MM-DD`, honoring `$XDG_STATE_HOME`), in addition to the stderr it always logged to. `vard logs` reads that file set.

There is no watch argument: one daemon writes one log covering every watch it supervises. This is the daemon's *operational* log (startup, reloads, snapshot/sync activity, errors) — distinct from [`history`](history.md), which shows a single watch's snapshot commits.

## Examples

```bash
vard logs
# the last 50 lines of the daemon's log

vard logs -n 200
# the last 200 lines, reaching into the previous day's file if needed

vard logs -f
# follow the live log, printing new lines as the daemon writes them

vard logs | grep -i error
# pipe the raw log text to grep (or less, or a file)
```

## Options

| Flag | Effect |
|---|---|
| `-n <N>`, `--lines <N>` | Show the last N lines (default 50). Spans rotation boundaries: if the newest day's file holds fewer than N lines, the previous day's file is read to make up the difference. |
| `-f`, `--follow` | Follow the live log, printing new lines as the daemon writes them and switching to the next day's file when the log rotates. Runs until interrupted (Ctrl-C). Implies no pager. |

## Output

The output is the daemon's raw log text and nothing else. On a terminal it is paged (unless following, which streams straight through); piped, it passes through untouched so it feeds `grep`, `less`, or a file. Each line carries a timestamp, level, and target:

```text
2026-07-18T15:16:22.114923Z  INFO vard::daemon: daemon started event="daemon.started"
```

Because a logfile is inherently a text artifact, `logs` is text-only: an explicit `--format json` or `--format jsonl` is rejected (the same contract as [`diff`](diff.md)).

## Retention and rotation

The daemon rotates the logfile daily and keeps a bounded number of days on disk (the oldest are pruned automatically), so the log never grows without limit. `-n` and `-f` both understand the rotated set, so a request that reaches past a day boundary — or a follow that runs across midnight — is seamless.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The log was shown (or followed until interrupted). |
| `1` | No logfile exists yet — the daemon has not run since file logging landed, or has never run. `vard logs` reports this rather than printing nothing. |
| `2` | An operational error (for example, `--format json`/`jsonl`, which `logs` does not support). |

## See also

- [`run`](run.md) — the daemon that writes this log.
- [`status`](status.md) — the daemon's liveness and each watch's current state.
- [`history`](history.md) — a single watch's snapshot history (not the daemon's operational log).
- Run `vard logs --help` for the full reference.
