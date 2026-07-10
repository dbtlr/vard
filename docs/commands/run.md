---
title: run
description: Run the vard daemon in the foreground, watching and snapshotting every configured directory.
---

# vard run

Run the vard daemon in the foreground until stopped. This is the process that does the actual watching: where [`snapshot`](snapshot.md) takes one snapshot on demand, `run` watches every configured directory continuously and snapshots changes into version control on its own. It stays attached to the terminal and logs each event to stderr, so you run it under a process supervisor (systemd, launchd, a tmux pane) rather than as a one-shot command.

## Examples

```bash
vard run
# watch every configured directory and snapshot until Ctrl-C

vard run &
# background it in a shell; logs still go to stderr

vard --color always run 2>vard.log
# force colored logs while redirecting stderr to a file
```

## What it does

On startup the daemon:

1. **Acquires the single-instance lock** for its state directory, so only one vard owns a directory tree at a time. A second daemon contending for the same state directory exits with status `2`.
2. **Loads the config file** into watch specs.
3. **Recovers stale version-control locks** left behind by a previous crash.
4. **Watches every configured directory** and snapshots changes into version control.

While running it:

- **Reloads** on `SIGHUP` or when the config file changes on disk — so [`watch add`](watch.md), [`config set`](config.md), and hand edits take effect without a restart.
- **Rebuilds a watch** whose event source dies, with exponential backoff.
- **Shuts down cleanly** on `SIGINT` (Ctrl-C) or `SIGTERM`.

## Output

`run` produces a live stderr log, not records — it does not consume the global `--format`. Use [`status`](status.md) and [`notify`](notify.md) to observe what the running daemon is doing.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown after `SIGINT`/`SIGTERM`. |
| `2` | Could not start — most commonly another daemon already owns the state directory. |

## See also

- [`status`](status.md) — check whether a daemon is running and what state each watch is in.
- [`notify`](notify.md) — the always-on health line for a prompt.
- [`watch`](watch.md) — register the directories `run` will watch.
- Run `vard run --help` for the full, always-current reference.
