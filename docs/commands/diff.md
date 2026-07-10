---
title: diff
description: Show a raw unified diff for a watch — working tree against a snapshot.
---

# vard diff

Show a raw unified diff for a watch. Read-only. With no reference the diff is the watched directory's working tree against its last snapshot (`HEAD`) — the uncommitted changes a [`snapshot`](snapshot.md) would capture. Given a `<ref>`, the diff runs from that reference to the current working tree, showing everything that changed since it. Where [`log`](log.md) lists *when* snapshots happened, `diff` shows *what* is different.

## Examples

```bash
vard diff notes
# working tree vs the last snapshot: what a snapshot would capture now

vard diff notes 67a3776f
# everything that changed since snapshot 67a3776f

vard diff notes main
# changes since the tip of a branch

vard diff notes > changes.patch
# piped output is plain diff text — feed it to patch or git apply
```

## Targets and arguments

| Argument | Effect |
|---|---|
| `<NAME\|PATH>` | The watch to diff, by name or by path. Required. |
| `<REF>` | The reference to diff from — a snapshot id, branch, tag, or any revision git understands. Defaults to `HEAD`, the last snapshot. |

## Output contract

The output is a raw unified diff and nothing else. On a terminal it is paged; piped it passes through untouched, so it feeds `patch`, `git apply`, or a file directly.

```text
diff --git a/a.md b/a.md
index 974e8f8..40227bf 100644
--- a/a.md
+++ b/a.md
@@ -1,2 +1,3 @@
 hello
 changed content
+uncommitted line
```

Because a unified diff is inherently a text artifact, `diff` is **text-only**: unlike the list-surface commands, it does not auto-detect a JSON form, and an explicit `--format json` or `--format jsonl` is rejected. The piped default still yields plain diff text, so `vard diff notes > changes.patch` works as expected. A watch with no differences prints nothing and exits `0`.

## See also

- [`log`](log.md) — the snapshot history whose ids you diff against.
- [`restore`](restore.md) — apply a prior state instead of just viewing it; `restore --dry-run` shows this same diff.
- [`snapshot`](snapshot.md) — commit the changes this diff shows.
- Run `vard diff --help` for the full reference.
