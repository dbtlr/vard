---
title: config
description: Read and edit vard's TOML configuration — get, set, unset, edit, path.
---

# vard config

Read and edit vard's TOML configuration file. These commands address scalar keys in the `[daemon]`, `[defaults]`, `[ai]`, and `[update]` sections by their dotted names (`daemon.log_level`, `defaults.interval`, `ai.model`). Edits preserve your comments and formatting and are written atomically, so the running [daemon](run.md) — which watches the file — reloads a clean, whole config every time.

Watch settings are **not** edited here: a `watch.*` key is refused with a pointer to [`vard watch set`](watch.md#set) — the verb that edits a watch's settings by identity — and the other [`vard watch`](watch.md) verbs (`add`, `remove`, `pause`, `resume`). The top-level `version` is managed by vard and is not settable either.

## Subcommands

| Command | Purpose |
|---|---|
| `vard config get <key>` | Print a key's value (exit `1` when the key is not set). |
| `vard config set <key> <value>` | Set a key to a value, rejecting an edit that would break the config. |
| `vard config unset <key>` | Remove a key, restoring its inherited default. |
| `vard config edit` | Open the config in `$VISUAL`/`$EDITOR` and validate the result. |
| `vard config path` | Print the config file's path. |

## Examples

```bash
vard config path
# print ~/.config/vard/config.toml (whether or not it exists yet)

vard config get defaults.interval
# print just the value, e.g. 15m — or nothing, exit 1, if unset

vard config set defaults.interval 30m
# set a key; the write is validated before it lands

vard config unset defaults.interval
# remove a key, restoring its default

vard config edit
# open the config in your editor; the result is validated on save

$EDITOR "$(vard config path)"
# the TEXT output of `path` composes directly into another command
```

## get

Print the value of a config key. Only what the file actually sets is printed — an inherited default is not materialized here — so a key the config does not set prints nothing and **exits `1`**, the way `git config` reports an unset key.

By default the **bare value** is printed — the TEXT form — whether on a terminal or piped, so `$(vard config get defaults.interval)` yields the value alone:

```text
15m
```

Pass `--format json` for the `{key, value}` object:

```json
{"key":"defaults.interval","value":"15m"}
```

## set

Set a config key to a value. The value's type is inferred (`true`/`false` a boolean, a bare integer a number, otherwise a string) and then validated: the edit is applied to a comment-preserving copy of the file and the result must still parse as a valid config. An edit that would turn a valid config invalid is **refused (exit `2`)** — for example a non-integer `daemon.log_retention_days`. A `watch.*` key is refused with a pointer to [`vard watch set`](watch.md#set); `version` is not settable.

```text
set defaults.interval = 30m
```

## unset

Remove a config key, restoring its inherited default. Removing a key that is not set is reported and **exits `2`**. As with `set`, the result is validated before it lands and a `watch.*` key is refused.

```text
unset defaults.interval
```

## edit

Open the config file in your editor and validate what you save. The file is copied to a temporary file, `$VISUAL` (falling back to `$EDITOR`) is launched on it, and the result is validated before it replaces the config — written atomically under the config lock so the running daemon never sees a half-written file. If the config changed on disk while you were editing, or the edit would turn a valid config invalid, it is refused: the reason and the temporary file's path are printed (so your work is not lost) and the command **exits `2`**. The daemon reloads the change on its own; no signal is needed.

## path

Print the path to vard's config file. Resolves the same `$XDG_CONFIG_HOME/vard/config.toml` (`~/.config/vard/config.toml` by default) location the daemon and other commands use, whether or not the file exists yet, so it can seed a script or an editor invocation.

By default the **bare path** prints — the TEXT form — whether on a terminal or piped, so `$(vard config path)` and `$EDITOR "$(vard config path)"` yield the path alone. Pass `--format json` for the `{path}` object.

## Output classes

`config` spans both of vard's output classes:

- **`get` and `path`** are single-value **TEXT** surfaces: absent an explicit `--format` they print the bare scalar on a terminal and when piped alike. `--format json` gives the enveloped object.
- **`set` and `unset`** are ordinary list surfaces: a result record on a terminal, JSON when piped.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The read or write succeeded. |
| `1` | `get` on a key the config does not set (quiet, `git config`-style). |
| `2` | A refused or impossible write — an invalid result, an unset key on `unset`, a `watch.*`/`version` key, or a stale/failed `edit`. |

## See also

- [`watch`](watch.md) — edit the set of watched directories (not editable here).
- [`run`](run.md) — the daemon that reloads config changes automatically.
- Run `vard config --help` for the full subcommand list.
