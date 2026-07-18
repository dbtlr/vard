---
title: service
description: Run vard as a login-session service — install, uninstall, start, stop, restart the daemon under launchd or systemd.
---

# vard service

Run the vard daemon as a service in your login session. [`vard run`](run.md) is the foreground daemon; the `service` verbs wrap it in your platform's login-session service manager so it starts at login and respawns on failure, instead of you keeping a terminal or a hand-rolled supervisor open. The unit only execs `vard run`, so all watching and snapshotting still happens there.

```bash
vard service install     # write the unit, load and start it, verify the daemon came up
vard service status      # (use `vard status` — the service verbs do not report state)
vard service restart     # pick up an upgraded binary or a changed unit
vard service uninstall   # stop, unload, and remove the unit
```

`service` is a grouped command: a bare `vard service` prints this short help, and each subcommand has its own `--help`.

## Why a login-session service

vard is installed as a **per-user** service — a macOS LaunchAgent or a systemd **user** unit — not a system-wide (root) daemon. That is deliberate: vard commits as you and reconciles with your remotes, so it needs your identity — your keychain, your `ssh-agent`, your git credential helper, your `HOME`. A root-owned system service would run as the wrong user, without that session, and could neither read your SSH keys nor sign commits as you. Running inside your login session keeps vard's snapshots and pushes indistinguishable from ones you took by hand.

## Platforms

| Platform | Manager | Unit file | Identity |
|---|---|---|---|
| macOS | launchd LaunchAgent | `~/Library/LaunchAgents/com.dbtlr.vard.plist` | loaded into your GUI login domain (`gui/<uid>`) with `launchctl` |
| Linux | systemd user unit | `~/.config/systemd/user/vard.service` (honors `$XDG_CONFIG_HOME`) | managed with `systemctl --user`; survives logout only with lingering enabled |

The LaunchAgents path is dictated by launchd and is deliberately not an XDG location. The systemd unit path follows `$XDG_CONFIG_HOME` (default `~/.config`) like the rest of vard's config.

Other platforms are unsupported: `vard service` exits `2` and points you at running `vard run` under your own supervisor.

## The binary path

`install` records the path `vard` was invoked through — the same path, symlinks **not** resolved. A Homebrew install therefore keeps `/opt/homebrew/bin/vard` (the stable shim) in the unit rather than a versioned `Cellar` path that a later upgrade would move out from under the service. If `vard` was invoked by a bare name it is looked up on `PATH`; if it carried a path it is made absolute against the working directory. When the invoked path cannot be resolved, vard falls back to the running executable's own path.

## Unit contents

### macOS (LaunchAgent plist)

```xml
<key>Label</key>            <string>com.dbtlr.vard</string>
<key>ProgramArguments</key> <array><string>/path/to/vard</string><string>run</string></array>
<key>RunAtLoad</key>        <true/>
<key>KeepAlive</key>        <dict><key>SuccessfulExit</key><false/></dict>
<key>ProcessType</key>      <string>Background</string>
<key>ThrottleInterval</key> <integer>10</integer>
```

`RunAtLoad` starts the daemon at login. `KeepAlive { SuccessfulExit = false }` respawns it only after a **failure** exit — a clean `SIGTERM` shutdown stays down, so `vard service stop` is not fought by launchd. `ThrottleInterval 10` keeps a crashing daemon out of a tight restart loop (it exits `2` on lock contention). Stdout and stderr are deliberately **not** redirected: the daemon writes its own rotated logfile (see [`logs`](logs.md)), and foreground `vard run` is how you watch it start.

### Linux (systemd user unit)

```ini
[Unit]
Description=vard — automatic directory snapshots into version control

[Service]
Type=simple
ExecStart=/path/to/vard run
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

`Restart=on-failure` respawns the daemon after a crash (a clean stop stays down), throttled by `RestartSec=5`. `ExecReload` sends `SIGHUP`, which the daemon treats as a config reload — so `systemctl --user reload vard.service` reloads config **without a full restart**. Stderr flows to the journal naturally.

## install

Renders the platform unit, writes it atomically (creating the LaunchAgents or `systemd/user` directory as needed), loads and starts the service, and then **verifies the daemon actually came up**: it polls the single-instance lock — the same liveness signal [`status`](status.md) and [`notify`](notify.md) use — for up to five seconds. If the unit is in place but the daemon never takes the lock, `install` exits `1` and tells you to run `vard run` in the foreground to see why.

Re-running `install` is idempotent: an already-loaded service is unloaded and re-bootstrapped (macOS) or re-enabled (Linux) rather than rejected.

### Lingering (Linux only)

A systemd **user** manager normally stops when your last session ends, which would stop vard at logout. After starting the service, `install` handles that consent:

| Invocation | Behavior |
|---|---|
| `--linger` | Enable lingering (`loginctl enable-linger`) so the service survives logout. |
| `--no-linger` | Leave lingering off. |
| neither, on a terminal | Prompt: `User services stop at logout. Enable lingering so vard survives logout? [y/N]` |
| neither, non-interactive | Leave lingering off and print a one-line notice saying so. |

`--linger` and `--no-linger` conflict, and they exist only on Linux — they do not appear in macOS help or completions (launchd has no equivalent; `RunAtLoad` already starts vard at each login).

### `--dry-run`

`vard service install --dry-run` prints the resolved binary path, the unit file path, the full rendered unit, and the actions that would run (including what the linger step would do) — then exits `0` having **written and run nothing**. It is safe on any machine and is the way to preview exactly what `install` would place.

```text
$ vard service install --dry-run
Dry run — nothing was written.
Binary:    /opt/homebrew/bin/vard
Unit file: ~/Library/LaunchAgents/com.dbtlr.vard.plist

Rendered LaunchAgent:
  <?xml version="1.0" encoding="UTF-8"?>
  ...
Would write the plist, bootstrap it into gui/501, and verify the daemon came up.
```

## uninstall

Stops and unloads the service, then removes its unit file. Nothing in your repositories or config is touched — only the service registration. Uninstalling when nothing is installed is a success: it says so and exits `0`.

## start, stop, restart

- **start** loads (macOS) or starts (Linux) the installed service and verifies the daemon came up. With no unit installed it exits with an error pointing at `vard service install`.
- **stop** unloads (macOS) or stops (Linux) the service. Stopping an already-stopped service is an idempotent success.
- **restart** restarts the service and verifies the daemon came back up — the way to pick up an upgraded `vard` binary or a changed unit. If the service is not loaded, restart loads it.

### stop is not uninstall

`stop` only stops the *running* daemon; the unit stays installed and **re-arms at your next login**:

- **macOS:** the plist's `RunAtLoad` starts vard again the next time you log in. To keep it from coming back, `uninstall` (or remove the plist).
- **Linux:** the unit stays *enabled*, so it starts again at your next login until you `uninstall` it (or `systemctl --user disable vard.service`).

Use `stop` to pause the daemon for this session; use `uninstall` to remove it for good.

### Reload vs restart

launchd has **no reload signal**, so on macOS a `restart` is how you re-exec the daemon after an upgrade. On Linux the unit additionally carries an `ExecReload` that sends `SIGHUP`, so `systemctl --user reload vard.service` performs a **config-only** reload without a full restart; `restart` is still the way to pick up a new *binary*.

## Non-systemd Linux

This release supports **systemd user services only** on Linux. If `systemctl` is absent, or the `systemctl --user` session is unreachable (no user bus), `vard service` exits `2` and advises running `vard run` under your own supervisor (a supervisor of your choice, a container entrypoint, or a tmux pane). The unit renderer is still exercised by `--dry-run`, which needs no systemd.

## Output

Service verbs print human status lines to stdout and are **text-only** — they ignore the global `--format` and reject an explicit `--format json`/`jsonl` (the same class as [`logs`](logs.md)). To observe the running service's state as records or JSON, use [`status`](status.md).

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. A stop-when-already-stopped and an uninstall-when-nothing-installed are idempotent successes and say so. |
| `1` | Attention — the unit was installed and the service started, but the daemon did not come up within five seconds. Run `vard run` in the foreground to see why. |
| `2` | Operational error — an unsupported platform, a `launchctl`/`systemctl`/`loginctl` failure, an unreachable systemd session, or an unresolvable `HOME`/config path. |

## See also

- [`run`](run.md) — the foreground daemon the service wraps.
- [`status`](status.md) — whether a daemon is running and each watch's state.
- [`logs`](logs.md) — the daemon's own rotated log.
- Run `vard service --help` (or `vard service <subcommand> --help`) for the full, always-current reference.
