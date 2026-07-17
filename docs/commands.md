---
title: Command reference
description: Index of every vard command, each linking to its own reference page.
---

# Command reference

Every vard command, each linking to its own page with examples, options, and output contracts. Run `vard <command> --help` for the authoritative, always-current flag list (`-h` for the compact version).

`vard` watches directories and snapshots them into version control automatically. The daemon (`vard run`) does the watching; the other commands register what to watch, take snapshots on demand, inspect and restore history, and report health.

## Global flags

These flags are accepted on every command.

| Flag | Description |
|---|---|
| `--color <when>` | Color output: `auto`, `always`, or `never`. `auto` (the default) colors a TTY and disables color when piped. `NO_COLOR` and `CLICOLOR_FORCE` always win. |
| `--format <format>` | Output shape: `records`, `json`, or `jsonl`. See [Output classes](#output-classes) for how the default is chosen. |
| `-h`, `--help` | Print help. `-h` is a short summary; `--help` is the full reference. |
| `-V`, `--version` | Print the version. |

The config file lives at `$XDG_CONFIG_HOME/vard/config.toml` (`~/.config/vard/config.toml` by default). [`vard config path`](commands/config.md) prints its resolved location whether or not it exists yet, and [`vard config edit`](commands/config.md) opens it in your editor.

## Output classes

vard has two output classes. Which one a command belongs to determines how `--format` behaves.

**List surfaces — records / json / jsonl, auto-detected by destination.** Commands that emit a list of records (`watch list`, `status`, `log`, `snapshot`, the `watch add`/`remove`/`pause`/`resume` and `config set`/`unset` result rows) resolve `--format` against the output destination when it is not given explicitly: human-readable `records` on a terminal, machine-readable `json` when piped. `jsonl` (one JSON object per line) is opt-in. An explicit `--format` always wins. `diff` is the one exception in this class: a unified diff is inherently text, so `diff` is text-only and rejects `--format json`/`jsonl`. [`notify`](commands/notify.md) is not in this class either: built for shell hooks, it prints its human line form regardless of destination and emits JSON only on an explicit `--format json`/`jsonl`.

**Single-value surfaces — the TEXT class.** [`config get`](commands/config.md) and [`config path`](commands/config.md) emit a lone scalar. For these, absent an explicit `--format`, the bare value is printed regardless of destination — on a terminal and when piped alike — so `$(vard config get defaults.interval)` and `$(vard config path)` yield the value alone. The bare line is itself the machine format for this class (a parallel TEXT response type, simpler to consume in automation than JSON), not a human courtesy. Pass `--format json` for the enveloped object (`{key, value}` for `config get`, `{path}` for `config path`).

## Exit codes

vard commands share a three-value exit convention:

| Code | Meaning |
|---|---|
| `0` | Healthy — the operation succeeded and nothing needs attention. |
| `1` | Attention — the operation ran, but something needs a human (a watch has a problem, the daemon is not running, a queried key is unset). |
| `2` | Operational error — the command could not complete (bad input, a refused edit, a lock it could not acquire). |

[`status`](commands/status.md) and [`notify`](commands/notify.md) lean on this directly so a shell prompt or script can branch without parsing text. [`config get`](commands/config.md) uses the quiet `1` to mean "key not set", the way `git config` does.

## Daemon

| Command | Summary |
|---|---|
| [`run`](commands/run.md) | Run the vard daemon in the foreground: watch every active configured directory and snapshot changes until stopped. |

## Watch and snapshot

| Command | Summary |
|---|---|
| [`watch`](commands/watch.md) | Manage the set of watched directories: `add`, `remove`, `list`, `pause`, `resume`. |
| [`snapshot`](commands/snapshot.md) | Take a manual snapshot now — sweep a watch and commit its current state. |
| [`sync`](commands/sync.md) | Sync a watch with its remote now — fetch, reconcile out of tree, and push. |

## Inspect and restore

| Command | Summary |
|---|---|
| [`log`](commands/log.md) | Show a watch's snapshot history, most recent first. |
| [`diff`](commands/diff.md) | Show a raw unified diff for a watch: working tree against a snapshot. |
| [`restore`](commands/restore.md) | Restore a watch's tree (or one file) to a prior snapshot, protecting current state first. |

## Health

| Command | Summary |
|---|---|
| [`status`](commands/status.md) | Show the daemon's liveness and every watch's current state, read-only. |
| [`notify`](commands/notify.md) | Print one line per watch that needs attention, for a shell prompt or status bar. |

## Configuration

| Command | Summary |
|---|---|
| [`config`](commands/config.md) | Read and edit vard's configuration: `get`, `set`, `unset`, `edit`, `path`. |
