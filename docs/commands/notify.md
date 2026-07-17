---
title: notify
description: Print one line per watch that needs attention — built for a shell prompt or status bar.
---

# vard notify

Print a short health summary designed to be wired into a shell prompt, tmux status line, or starship module. `notify` is built for speed above all else: it opens the small health file the [daemon](run.md) keeps up to date, reads a few bytes, and exits. It never talks to the daemon and never runs git, so it is safe to call on every shell prompt. Where [`status`](status.md) lists **every** watch on demand, `notify` prints **only problems** and stays completely silent when all is well — so it can run before every prompt without adding noise.

## Examples

```bash
vard notify
# nothing, exit 0, when every watch is healthy

vard --color always notify
# keep the warning glyph colored inside a prompt substitution

vard notify --format json
# a stable array of problem objects (empty when healthy)
```

Wire it into a prompt by running it before each prompt and showing its output. Because it exits non-zero on trouble, a prompt can also branch on the status without parsing the text.

## Output

Human line form by default, **regardless of destination** — unlike the list surfaces, `notify` never auto-switches to JSON when piped, because its primary consumer is a shell hook echoing the line verbatim. A machine consumer (a status-bar program) passes `--format json`/`jsonl` explicitly. When every watch is healthy it **prints nothing and exits 0**. When something needs attention it prints one line per problem and exits `1`:

```text
⚠ vard: daemon not running — start it with `vard run`
```

Each line is a watch that is blocked (unsafe-repo), snapshots-failing, conflicted, sync-erroring, or attention-needing, with how long it has been in that state. A watch whose [hook](run.md#hooks) has failed 3 consecutive times is reported here too (kind `hook-failing`), clearing on the hook's next success; a failing global `[hooks]` hook is reported as a daemon hook. A watch you deliberately paused is **not** reported (that is not a problem); [`status`](status.md) lists paused watches. Hook **suppression** — coalesced hook events — is telemetry, not a problem: `notify` stays silent about it (a watch that has only coalesced hooks is healthy), so it never adds prompt noise; [`status`](status.md) is where the coalesced count is shown. If the daemon is not running that is itself one reported line (it replaces any stale per-watch entries), and while it is starting or stopping `notify` says so rather than reporting a false all-clear — so a prompt hook can tell "all quiet" from "nothing is watching your files".

```bash
vard notify --format json
```

```json
[{"watch":null,"state":"daemon-not-running","kind":"daemon-not-running","summary":"the vard daemon is not running; your watches are not being snapshotted","since":null,"elapsed_seconds":null}]
```

The JSON/JSONL array is empty when healthy.

## Color and glyph in a prompt

The warning glyph is colored only when color is enabled. `--color auto` (the default) disables color when its output is captured — which a prompt substitution always does — so pass `--color always` (or set `CLICOLOR_FORCE=1`) to keep the glyph colored in a prompt. Set `VARD_ASCII` (or use a non-UTF-8 locale) for an ASCII fallback glyph instead of the Unicode warning sign.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Healthy — nothing printed. |
| `1` | Problems — including a stopped, starting, or stale daemon. |
| `2` | Operational error. |

## See also

- [`status`](status.md) — the on-demand review that lists healthy and paused watches too.
- [`run`](run.md) — the daemon whose health file `notify` reads.
- Run `vard notify --help` for the full reference.
