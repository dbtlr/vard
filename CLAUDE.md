# vard

`vard` watches directories and snapshots them into version control automatically.
It is a Cargo workspace: `vard-core` (the watcher/snapshot engine) and `vard` (the
CLI and daemon binary).

## Docs/help sync contract

The CLI has two layers of documentation that must agree:

- **`--help` is the authoritative flag list.** It is rendered from the clap
  definitions in `vard/src/cli.rs` — the `about`/`long_about` text and every
  `arg` — and is the single source of truth for what flags exist and what they
  do.
- **`docs/commands/*.md` carry the richer semantics** — workflows, worked
  examples, and output contracts — plus an index at `docs/commands.md`. They go
  beyond `--help` but must never contradict it.

Rules:

- Any change to the CLI surface — adding or removing a command or flag, editing
  `about`/`long_about` text, or changing output or exit-code behavior — updates
  the matching `docs/commands/<command>.md` page in the same change, or
  explicitly states in the change description that no doc impact exists.
- One docs page per **top-level** command; grouped commands (`watch`, `config`)
  document their subcommands within that one page.
- The `docs_coverage` integration test (`vard/tests/docs_coverage.rs`) enforces
  *structure* — that a page exists for every command, that every page maps to a
  real command, and that the index links each page. It does **not** and cannot
  check that the prose is accurate; that is on the author. Derive every factual
  claim from the real clap definitions and from running the command, never from
  memory.
- Never put local absolute paths in docs or examples — use `~/`-style or generic
  paths.

## Gate loop

All of these must pass locally before a push:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo build -p vard --release --locked && scripts/ci-smoke.sh
```

`scripts/ci-smoke.sh` is the single source of truth for post-build smoke
assertions, shared by CI (`.github/workflows/ci.yml`) and local runs; it asserts
on the release binary's real behavior, so it must pass against a fresh
`--release` build.
