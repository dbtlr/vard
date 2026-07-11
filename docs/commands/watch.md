---
title: watch
description: Manage the set of watched directories — add, remove, list, pause, resume.
---

# vard watch

Manage the set of directories vard watches. Each watch is one directory tracked as its own git repository, keyed by its canonicalized path and a stable name — selectors accept either. These commands edit the config file in place, preserving your comments and formatting; a running [daemon](run.md) reloads the change automatically, so edits take effect without a restart.

This is where you declare *what* to watch. [`snapshot`](snapshot.md) and [`run`](run.md) act on the watches registered here.

## Subcommands

| Command | Purpose |
|---|---|
| `vard watch add <path>` | Register a directory (offering `git init` when it is not yet a repo). |
| `vard watch remove <name\|path>` | Unregister a watch, never touching the repository or its history. |
| `vard watch list` | Show every watch and its settings. |
| `vard watch pause <name\|path>` | Stop snapshotting a watch without unregistering it. |
| `vard watch resume <name\|path>` | Resume a paused watch. |

## Examples

```bash
vard watch add ~/notes --name notes
# register ~/notes as a watch called "notes"

vard watch add ~/project --no-sync --init
# register a local-only watch, running `git init` non-interactively if needed

vard watch list
# show every watch and its settings

vard watch pause notes
# stop the daemon snapshotting "notes" (metadata is kept)

vard watch resume notes
# resume it on the daemon's next reload

vard watch remove notes
# unregister "notes"; the repo and its snapshots are untouched
```

## add

Register a directory as a watch. The directory must be a git repository; if it is not, `vard watch add` offers to run `git init` — on a terminal it prompts, non-interactively it declines unless `--init` is passed. The watch is recorded by its canonicalized path (symlinks resolved) plus a stable name (`--name`, or the directory's own name by default). Re-adding an existing name at a new path relinks that watch to the new location, keeping its metadata — the recovery path for a directory that moved.

A running daemon resolves and caches each watch's canonical path once, at startup and on each config reload. If a watched path is a **symlink that you retarget to a different repository while the daemon is running**, the daemon keeps keying that watch's operation lock by the old target until it is **restarted** (or reloads). A CLI operation resolves the path freshly, so across that window the two could take different locks and not fully exclude each other — restart the daemon after retargeting a watched symlink if strict cross-process exclusion matters. Moving or relinking a directory the ordinary way (`vard watch add` at the new path, then reload) is unaffected.

Adding also seeds the repository's private `.git/info/exclude` (never your tracked `.gitignore`) with vard's default excludes: dependency and build directories, OS cruft, and well-known secret shapes such as `.env`, `*.pem`, and `id_rsa*`. The write is idempotent — re-adding never duplicates lines and leaves your own exclude entries untouched.

| Flag | Effect |
|---|---|
| `--name <NAME>` | Stable name for the watch. Defaults to the directory's own name. |
| `--remote <REMOTE>` | Remote the watch pushes to and pulls from. Default `origin`. |
| `--branch <BRANCH>` | Branch the watch commits to. Default: the repository's current branch. |
| `--trigger <MODE>` | Which automatic triggers arm snapshots: `events`, `interval`, or `both`. |
| `--interval <DURATION>` | Interval between periodic snapshots, e.g. `15m` or `1h30m`. |
| `--quiesce <DURATION>` | How long file activity must settle before a snapshot, e.g. `10s`. |
| `--no-sync` | Register the watch as local-only: never sync to a remote. |
| `--init` | If the directory is not a git repository, `git init` it without prompting (the non-interactive escape hatch). |

## remove

Unregister a watch. This removes the watch from the config file only — it never touches the repository, its working tree, or its history. The directory and every snapshot vard took remain exactly as they were.

Removing also *drains* the watch: it settles any operation still in flight and cleans a stale git lock left by a crashed vard operation (proven to be vard's own), so a removed directory never wedges on a lock only vard could vouch for. The drain is best-effort:

- **A running daemon** drains the watch as it reloads the change, so `remove` skips the synchronous drain — the daemon is the one writer of the journal.
- **No daemon running**: `remove` drains synchronously, taking vard's instance lock for the moment it runs recovery.
- **A busy peer command** (another `vard` operation holding the lock) is waited out only briefly (about 3 seconds) and then skipped — `remove` never blocks on it. Anything skipped is covered by the daemon's next reload or the next daemon start's journal sweep, which now recovers a since-removed watch's journal from the repository path recorded inside it.

The repository is never modified. The one residual that automatic recovery cannot cover is a journal written by a much older vard version (before journals recorded their repository path) whose watch is already gone — that leaves a manual cleanup, which vard logs when it retains such a file.

| Flag | Effect |
|---|---|
| `--purge` | After draining, also delete vard's own metadata for the watch (its operation journal). The journal is deleted only when it is safe to: when this command drained it, or when it records no open operation. If a daemon or peer command holds the lock **and** the journal still records an in-flight operation, the journal is **retained** (the command says so) so that open operation stays recoverable — the daemon's reload-drain or the next start's sweep settles it. Never touches the repository. By default this metadata is kept so re-adding the same path resumes cleanly. |

## list

List every registered watch. Each watch reports its name, path, branch and remote, trigger and interval, whether it syncs, and whether it is paused. If two watches resolve to the same canonical repository (for example a path and a symlink to it), only the first is supervised; `list` marks the later one with an `aliases` field naming the watch it collides with.

Output follows the global `--format`: human-readable records on a terminal, JSON (or JSONL) when piped.

```text
1 watches
────────────────────────────────────────────────────────────
  name      notes
  path      ~/notes
  branch    —
  remote    origin
  trigger   both
  interval  15m
  sync      yes
  paused    no
```

```bash
vard watch list --format json
```

```json
[{"name":"notes","path":"~/notes","branch":null,"remote":"origin","trigger":"both","interval":"15m","sync":true,"paused":false,"aliases":null}]
```

## pause / resume

`pause` stops the daemon snapshotting a watch until it is resumed. The watch stays registered and keeps all of its metadata; the pause is recorded as `paused = true` in the config file, so it survives a daemon restart and applies on the next reload. `resume` clears the flag; resuming a watch that is not paused is a no-op. A paused watch is reported by [`status`](status.md) but stays silent in [`notify`](notify.md) — a deliberate pause is not a problem.

## Output contract

`list` is a list surface (records/json/jsonl). The mutating verbs (`add`, `remove`, `pause`, `resume`) report their result the same way — a record on a terminal, JSON when piped:

```text
added watch notes → ~/notes
```

```json
{"name":"notes","path":"~/notes","initialized":false,"relinked":false}
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The change was applied. |
| `2` | The change was refused (e.g. a name/path that does not resolve, or `add` declining a non-repo non-interactively). |

## See also

- [`config`](config.md) — edit scalar config keys. Watch settings are **not** edited there; a `watch.*` key is refused with a pointer back here.
- [`snapshot`](snapshot.md) — snapshot a registered watch on demand.
- [`status`](status.md) — the current state of every watch, including paused ones.
- Run `vard watch --help` for the full subcommand list.
