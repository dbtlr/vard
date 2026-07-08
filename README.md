# vard

*vörðr (Old Norse: "warden") — the watcher-spirit that follows a person through life.*

vard is a small, focused daemon that watches directories and automatically
snapshots them into version control. It is a flight recorder for directories
you care about but don't actively "develop" — dotfiles, markdown vaults,
notes, configuration trees — especially ones that AI agents or scripts may
modify. If something mucks things up, history exists and restore is one
command.

**Status: pre-alpha.** The design is settled; the implementation is just
beginning. Nothing here is usable yet.

## Principles

- Passive, continuous local history for watched directories.
- Optional reconciliation against a remote (backup + multi-machine sync).
- Safe by construction: no operation may ever destroy the only copy of
  anything.
- Small surface, composable pieces: the tool schedules git, it does not
  wrap it.

## Layout

A Cargo workspace producing one static binary:

- `vard-core/` — the embeddable engine: watch engine, snapshot engine, VCS
  backends, event bus. No CLI, no file-config I/O, no service opinions.
- `vard/` — the binary: CLI, daemon wiring, config management, hooks, health
  file, service and update machinery.

## License

MIT
