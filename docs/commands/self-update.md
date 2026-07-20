---
title: self-update
description: Update the installed vard binary to the latest release or a pinned version — receipt-gated, sha256-verified, applied with an atomic rename. Pinning is the rollback.
---

# vard self-update

Update the installed `vard` binary in place. `self-update` fetches cargo-dist's `dist-manifest.json` from the GitHub release, resolves the artifact for your platform, downloads it, **verifies its sha256 from the manifest before unpacking**, stages the new binary next to the current one, and replaces it with a single atomic `rename`. Integrity is TLS plus that sha256 — there is no separate signed manifest.

It is gated on the cargo-dist **install receipt**: only installs from the official vard installer can self-update. A `cargo install`, a Homebrew install, or a source build has no receipt and is refused, with a pointer back to that tooling — vard never swaps a binary it did not install.

## Examples

```bash
vard self-update
# update to the latest release (a no-op if you are already current)

vard self-update --dry-run
# resolve the target version, artifact, and install path; print the plan; change nothing

vard self-update --version 0.1.0
# install a specific version — downgrades allowed; pinning is the rollback

vard self-update --format json
# the plan/outcome as a stable machine object
```

## Options

| Flag | Description |
|---|---|
| `--version <X.Y.Z>` | Pin a specific version instead of the latest. **Downgrades are allowed** — pinning an older version is the rollback; there is no separate revert command. |
| `--dry-run` | Resolve the target version, artifact URL, sha256, and install path and print the plan, without downloading or changing anything. |

## Behavior

- **Receipt gate.** Before anything else, `self-update` checks for the cargo-dist install receipt at `~/.config/vard/vard-receipt.json` (next to `config.toml`; honors `XDG_CONFIG_HOME`). Absent or unreadable, the command refuses and points you at re-running the installer or the package manager you originally used — nothing is fetched or changed.
- **Latest or pinned.** With no `--version`, the target is the latest release. `--version X.Y.Z` targets that release's tag. Either way the true latest version is still reported, so a pinned downgrade shows both what it installed and what the newest release is.
- **Already current.** If the target version equals the running version, the command reports "up to date" and exits `0` — a success that downloads and changes nothing.
- **Integrity first.** The tarball's sha256 is checked against the manifest **before** it is unpacked. On a mismatch the command fails and the installed binary is left byte-for-byte untouched — nothing is swapped until a verified binary has been staged.
- **Atomic swap.** The new binary is staged as a hidden sibling of the install path (same filesystem) and moved over the current one with a single `rename`. The running process keeps executing from the old file until it exits — expected and safe.
- **Post-swap restart and verify.** After a successful swap, `self-update` checks whether a `vard` service unit is loaded (the macOS LaunchAgent or the Linux systemd user unit):
  - **Unit loaded.** It restarts the daemon through the same machinery as `vard service restart`, then polls the health file until it reports the **target version** running, bounded by a timeout. On success it reports the confirmed running version and exits `0`.
  - **Verify not confirmed.** If the restart fails or the health file does not report the new version within the timeout, the **swap itself still succeeded** — the new binary is in place. The command says so, prints the exact recovery gesture, and exits non-zero. It asserts only on the daemon's heartbeat and version, never on watch state, so a problem that predates the update is never blamed on it. The recovery gesture is to pin the previous version (`vard self-update --version <previous>`) or reinstall via the installer.
  - **No unit loaded.** The swap succeeds and the command tells you to restart your own foreground `vard run` to pick up the new binary, exiting `0`. (Nothing to restart automatically.)
- **`--dry-run` states the post-swap plan.** A dry run also prints what the post-swap step would be — restart-and-verify when a unit is loaded, or a manual restart when it is not — without swapping or restarting anything.

## Output

A list surface (records / json / jsonl), resolved against the destination like the other list commands: a human plan on a terminal, a machine object when piped.

```text
vard self-update…
  current       0.1.0
  latest        0.2.0
  target        0.2.0
  triple        aarch64-apple-darwin
  install path  /opt/homebrew/bin/vard
  asset url     https://github.com/dbtlr/vard/releases/download/v0.2.0/vard-aarch64-apple-darwin.tar.xz
  asset sha256  9f2b…
Updated vard 0.1.0 → 0.2.0
Restarted the vard service; 0.2.0 is now running.
```

If the restart cannot be verified, the swap-succeeded outcome and the recovery gesture are printed instead:

```text
Updated vard 0.1.0 → 0.2.0
The binary was swapped, but the restart could not be verified: restarted the service, but the daemon's health file did not report 0.2.0 running within 5s.
The new binary is in place; the daemon may still be on 0.1.0.
To go back to 0.1.0:
    vard self-update --version 0.1.0
or reinstall via the installer:
    curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dbtlr/vard/releases/latest/download/vard-installer.sh | sh
```

The machine form is a single stable object (a one-element array under `--format json`, one line under `--format jsonl`). The `asset_url` and `asset_sha256` fields are `null` on a no-op; the post-swap fields (`restart_attempted`, `verify_outcome`, `running_version`) describe the phase-2 outcome and are `false`/`null` when nothing was swapped.

```bash
vard self-update --dry-run --format json
```

```json
[
  {
    "action": "would_update",
    "update_available": true,
    "current_version": "0.1.0",
    "latest_version": "0.2.0",
    "target_version": "0.2.0",
    "target_triple": "aarch64-apple-darwin",
    "install_path": "/opt/homebrew/bin/vard",
    "asset_url": "https://github.com/dbtlr/vard/releases/download/v0.2.0/vard-aarch64-apple-darwin.tar.xz",
    "asset_sha256": "9f2b…",
    "dry_run": true,
    "unit_installed": true,
    "restart_attempted": false,
    "verify_outcome": null,
    "running_version": null
  }
]
```

| Field | Meaning |
|---|---|
| `action` | `would_update` / `would_no_op` (dry run), or `updated` / `no_op` (real run). |
| `update_available` | Whether the latest release differs from the current version. |
| `current_version` | The running binary's version. |
| `latest_version` | The newest published version (reported even when a lower version is pinned). |
| `target_version` | The version this run targets — the pin, or the latest. |
| `target_triple` | The platform triple the artifact was selected for. |
| `install_path` | The binary that was (or would be) replaced. |
| `asset_url`, `asset_sha256` | The resolved artifact and its manifest checksum; `null` on a no-op. |
| `dry_run` | Whether this was a dry run. |
| `unit_installed` | Whether a `vard` service unit is loaded (drives the post-swap step and the dry-run plan). |
| `restart_attempted` | Whether a loaded unit was restarted after the swap. `false` on a dry run, a no-op, or when no unit is loaded. |
| `verify_outcome` | `verified` (new version confirmed running), `failed` (restart/verify not confirmed), `no_unit` (swapped, nothing to restart), or `null` when nothing was swapped. |
| `running_version` | The version confirmed running after a verified restart; `null` otherwise. |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success — updated (and, when a service unit is loaded, the new version verified running; when none is loaded, swapped for you to restart), already current, or a dry-run plan. |
| `1` | The updater will not proceed and you must act elsewhere: no install receipt (re-run the installer, or use your package manager), or no release artifact published for this platform. Nothing was downloaded or changed. |
| `2` | An operational failure: a pinned version that does not exist on GitHub; a network, checksum, extraction, or swap error; or a swap whose post-swap service restart/verify could not be confirmed within the timeout (the binary is swapped, but the running daemon may still be on the old version — use the printed recovery gesture). |

## See also

- [`service`](service.md) — restart the daemon after an update so it runs the new binary.
- [`run`](run.md) — the foreground daemon a self-update leaves running on the old binary until restarted.
- Run `vard self-update --help` for the full reference.
