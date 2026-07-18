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
| `vard watch set <name\|path>` | Change one or more settings on an existing watch (or `--unset` to clear one). |
| `vard watch pause <name\|path>` | Stop snapshotting a watch without unregistering it. |
| `vard watch resume <name\|path>` | Resume a paused watch. |
| `vard watch sync <name\|path> [--off]` | Turn syncing on for a watch (and confirm with a first sync), or `--off` to turn it off. |

## Examples

```bash
vard watch add ~/notes --name notes
# register ~/notes as a watch called "notes"

vard watch add ~/project --no-sync --init
# register a local-only watch, running `git init` non-interactively if needed

vard watch list
# show every watch and its settings

vard watch set notes --interval 30m --trigger both
# change settings on the "notes" watch in place

vard watch set notes --unset interval
# clear a setting so it re-inherits its default

vard watch pause notes
# stop the daemon snapshotting "notes" (metadata is kept)

vard watch resume notes
# resume it on the daemon's next reload

vard watch sync notes
# turn syncing on for "notes" and run a first sync to confirm

vard watch sync notes --off
# turn syncing back off (an explicit local-only pin)

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
| `--sync-interval <DURATION>` | Pull-sync cadence, e.g. `20m`; `0s` turns the pull timer off. Written into the new entry when given. |
| `--no-sync` | Force the watch local-only: never sync to a remote, even if `defaults.sync = true`. Writes an explicit `sync = false` pin. Conflicts with `--sync`. |
| `--sync` | Turn syncing on for the new watch and run a first sync to confirm — the same opt-in flow as [`vard watch sync`](#sync). Writes `sync = true`. Conflicts with `--no-sync`. |
| `--init` | If the directory is not a git repository, `git init` it without prompting (the non-interactive escape hatch). |

Syncing is **off by default**: a newly added watch is local-only until you turn it on. Pass `--sync` to opt in at add time (it writes `sync = true` and runs a confirmation cycle), or turn it on later with [`vard watch sync <name>`](#sync). The `--remote` and `--branch` flags are **optional** — they default to `origin` and the repository's current branch — so what a sync actually needs is that the selected remote **exists in the repository** (`git remote add origin <url>`), not that you pass the flags. See [`sync`](sync.md). Adding with `--sync` before that remote exists still writes `sync = true` and exits `1` from the confirmation cycle — see [`sync`](#sync) below for what that exit means to a script.

When an `add` leaves syncing **off** — neither `--sync` nor a `defaults.sync = true` — the records-form output ends with a single hint line:

```text
added watch notes → ~/notes
syncing is off — enable with: vard watch sync notes
```

The hint is records-form only (the machine forms already carry the effective `sync` value via [`list`](#list)); it is suppressed by `--sync`, by an explicit `--no-sync`, and when `defaults.sync = true` already resolves syncing on.

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
  sync      no
  paused    no
```

```bash
vard watch list --format json
```

```json
[{"name":"notes","path":"~/notes","branch":null,"remote":"origin","trigger":"both","interval":"15m","sync":false,"paused":false,"aliases":null}]
```

(The `sync` field is the effective value: `no`/`false` here because a default-added watch is local-only until syncing is enabled in the config.)

## set

Change one or more settings on a watch that is already registered. The edit is applied to the watch's `[[watch]]` table in the config file in place, preserving your comments and formatting; the running daemon reloads it on its own. The watch is chosen by the same `<name|path>` selector as `remove`/`pause`/`resume`, and an unresolved selector exits `2`.

```bash
vard watch set notes --interval 30m
```

```text
updated watch notes: interval = 30m
```

Each flag sets one key, using the same vocabulary — and the same parsing and validation — as [`add`](#add):

| Flag | Key set |
|---|---|
| `--trigger <MODE>` | `trigger` (`events`, `interval`, or `both`). |
| `--interval <DURATION>` | `interval`, e.g. `15m` or `1h30m`. |
| `--quiesce <DURATION>` | `quiesce`, e.g. `10s`. |
| `--sync-interval <DURATION>` | `sync_interval`, e.g. `20m`; `0s` turns the pull timer off. |
| `--remote <REMOTE>` | `remote`. |
| `--branch <BRANCH>` | `branch`. |

Several flags may be combined in one invocation; the machine forms report the change as a single flat object (the watch name plus one field per changed key — its new value for a set, `null` for an unset):

```bash
vard watch set notes --trigger both --sync-interval 20m --format json
```

```json
{"name":"notes","trigger":"both","sync_interval":"20m"}
```

### `--unset <key>`

`--unset <key>` (repeatable) removes a key from the watch's entry so it re-inherits its default. Its keys are the config-key names of the flags above: `trigger`, `interval`, `quiesce`, `sync_interval`, `remote`, `branch`.

```bash
vard watch set notes --unset interval --unset quiesce
```

- Unsetting a key the watch does not set is reported and **exits `2`** (parity with [`vard config unset`](config.md#unset)), leaving the config untouched.
- Setting and unsetting the same key in one invocation is a usage error (**exit `2`**).

At least one `--<key>` flag or `--unset` is required; a bare `vard watch set <name|path>` with no changes is a usage error (**exit `2`**) that lists what can be set. An invalid value (a bad trigger or duration) is refused (**exit `2`**) and the config is left untouched — the edit lands only if the result still resolves to a valid watch.

### What `set` deliberately does not touch

Syncing, the paused flag, the path, and the name each have their own verb and are **not** settable here:

- **syncing** — [`vard watch sync <name>`](#sync) (or `--off`); enabling it is a consent gesture that runs a confirmation cycle.
- **paused** — [`vard watch pause`](#pause--resume) / `resume`.
- **path** — re-run [`vard watch add`](#add) at the new location to relink a moved directory.
- **name** and **exclude** — not editable through `set`.
- **secret scanning** (`secret_scan` / `secret_patterns`) — config-file-only; edit them in `[defaults]` or the watch's `[[watch]]` table with [`vard config edit`](config.md#secret-scanning-secret_scan--secret_patterns). There is no `watch set` flag (a per-watch boolean like this has no dedicated verb, and `secret_patterns` is a list).

## pause / resume

`pause` stops the daemon snapshotting a watch until it is resumed. The watch stays registered and keeps all of its metadata; the pause is recorded as `paused = true` in the config file, so it survives a daemon restart and applies on the next reload. `resume` clears the flag; resuming a watch that is not paused is a no-op. A paused watch is reported by [`status`](status.md) but stays silent in [`notify`](notify.md) — a deliberate pause is not a problem.

## sync

Syncing is **off by default** (a watch is local-only until you opt in). `vard watch sync <name|path>` is that one-step opt-in: it writes `sync = true` on the watch (preserving your comments and formatting) and then triggers **one** sync cycle for it — the very dispatch [`vard sync <name>`](sync.md) uses. The first cycle **is** the confirmation, reported honestly, and — exactly like `vard sync` — *where* it runs and *what* is reported depend on whether the daemon is up:

- **No daemon running:** the cycle runs in-process under the single-instance lock and the command reports the **real per-watch outcome** (`pushed`, `pulled`, `synced`, or `up to date`).
- **The daemon is running:** it owns the repositories, so the cycle is handed to it and runs **asynchronously**. The command reports only that the request was **queued** (`sync request for <name> handed to the running daemon`), not the cycle result — check the daemon log or [`status`](status.md) for the outcome.

There is no prompt in either direction — invoking the command is the consent.

Opting in never creates a remote; vard does not touch remotes. A watch whose repository has **no configured remote** is still enabled, and the confirmation cycle reports the missing remote and points at how to add one:

```text
1 syncs
────────────────────────────────────────────────────────────
  name     notes
  status   disabled
  detail   no remote "origin" in the repository; add it first
  commits  —
  ref      —
  no "origin" remote in the repository yet — add one, then re-sync: git remote add origin <url>
```

Add the remote (`git remote add origin <url>`), then re-run `vard watch sync notes` to sync.

The exit `1` here is a refusal reported honestly, **not** a rollback: `sync = true` has already landed in the config, and a running [daemon](run.md) logs the remote-less watch as a skip and **picks up a remote added later on its next sync, live** — no restart, no re-enable (see [`sync`](sync.md)). For a provisioning script that registers watches **before** their remotes exist, that makes exit `1` the expected "enabled, pending a remote" outcome, distinct from `2` (a real fault). How the refusal is reported depends on the mode: with **no daemon** the confirmation cycle runs and the machine forms carry it as a row (`status: "disabled"`, the `detail` naming the missing remote); with a **daemon running** the up-front check refuses before any cycle, as a message on stderr only — `add --sync`'s JSON then carries an empty `sync` array — so the exit code is the signal to script against.

`--off` turns syncing off instead: it writes an explicit `sync = false` — a pin that also overrides a `defaults.sync = true` — and runs **no** cycle, reporting plainly like pause/resume:

```text
disabled syncing for watch notes
```

The selector and its errors match `pause`/`resume`: a name or path selects the watch, and an unresolved selector exits 2. The enable path's exit code mirrors [`sync`](sync.md):

- **No daemon:** `0` when the confirmation cycle synced or was `up to date`, `1` when the watch is **paused**, has no remote yet, or a reconcile conflict latched, `2` on an operational failure.
- **Daemon running:** `0` once the request is **queued** (the actual outcome is asynchronous), `1` when the up-front check refuses (the watch is **paused** — the daemon will not sync it — or has no configured remote yet), `2` when its repository cannot be opened.

(Enabling still writes `sync = true` even for a paused watch; the confirmation just reports that it is paused. Resume it, then re-sync.)

`--off` always exits `0` on success — it only writes the config and runs no cycle.

## Output contract

`list` is a list surface (records/json/jsonl). The mutating verbs (`add`, `remove`, `set`, `pause`, `resume`) report their result the same way — a record on a terminal, JSON when piped:

```text
added watch notes → ~/notes
```

```json
{"name":"notes","path":"~/notes","initialized":false,"relinked":false}
```

`add --sync` reports the add **and** its confirmation cycle. In the records and JSONL forms these appear back to back (the add result, then the sync rows). In the single-document **JSON** form the confirmation rows are folded into the one add object under a `sync` array, so stdout stays one parseable document:

```json
{"name":"notes","path":"~/notes","initialized":false,"relinked":false,"sync":[{"name":"notes","status":"up to date","detail":null,"commits":null,"ref":null}]}
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The change was applied. |
| `1` | Attention — the change was applied but its `sync` confirmation could not complete: the watch is paused, has no configured remote yet, or a reconcile conflict latched (only `sync` / `add --sync` produce this). |
| `2` | The change was refused (e.g. a name/path that does not resolve, or `add` declining a non-repo non-interactively). |

## See also

- [`config`](config.md) — edit scalar config keys. Watch settings are **not** edited there; a `watch.*` key is refused with a pointer back here.
- [`snapshot`](snapshot.md) — snapshot a registered watch on demand.
- [`status`](status.md) — the current state of every watch, including paused ones.
- Run `vard watch --help` for the full subcommand list.
