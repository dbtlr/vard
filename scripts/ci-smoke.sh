#!/usr/bin/env bash
# Post-build smoke assertions for the vard binary and its generated artifacts.
# Single source of truth for CI and local gate runs: .github/workflows/ci.yml
# invokes this after the release build, and the same script must pass locally
# before a push — if an assertion here drifts from the binary's behavior, it
# fails in both places, not just CI.
#
# Usage: scripts/ci-smoke.sh [path/to/vard]   (default: target/release/vard)
set -euo pipefail

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
VARD="${1:-$TARGET_DIR/release/vard}"

fail() {
  echo "::error::$1"
  exit 1
}

test -x "$VARD" || fail "$VARD not found — build first: cargo build -p vard --release --locked"

"$VARD" --version >/dev/null || fail "vard --version failed"

# Help must render through the custom CLI Help Output v2 path, not clap.
# Assert markers only the v2 renderer emits so a regression to clap help (or a
# broken interceptor) fails loudly.
"$VARD" -h | grep -q 'For full help, run' || fail "vard -h is missing the v2 short-help footer"
"$VARD" --help | grep -q '^EXAMPLES$' || fail "vard --help is missing the EXAMPLES section"
"$VARD" --help | grep -q 'watch' || fail "vard --help does not mention the watch command"
"$VARD" --help | grep -q 'status' || fail "vard --help does not mention the status command"
"$VARD" --help | grep -q 'config' || fail "vard --help does not mention the config command"
"$VARD" run --help | grep -q 'SIGHUP' || fail "vard run --help is missing the lifecycle prose"
"$VARD" help run >/dev/null || fail "vard help run did not render"

# Color contract: forced on emits ANSI even when piped; NO_COLOR suppresses
# them even under --color always.
esc=$(printf '\033')
"$VARD" --color always -h | grep -q "$esc" || fail "--color always emitted no ANSI output"
if NO_COLOR=1 "$VARD" --color always -h | grep -q "$esc"; then
  fail "NO_COLOR did not suppress ANSI output"
fi

# build.rs generates completions and manpages on every build; assert the
# expected command strings appear so a stale or truncated artifact fails
# loudly instead of passing a bare non-empty check.
for f in \
  "$TARGET_DIR"/completions/vard.bash \
  "$TARGET_DIR"/completions/_vard \
  "$TARGET_DIR"/completions/vard.fish \
  "$TARGET_DIR"/completions/vard.nu \
  "$TARGET_DIR"/man/vard.1 \
  "$TARGET_DIR"/man/vard-run.1 \
  "$TARGET_DIR"/man/vard-watch.1 \
  "$TARGET_DIR"/man/vard-watch-add.1; do
  test -s "$f" || fail "$f is missing or empty — stale artifact?"
done
# The root vard.1 must name its subcommands, not merely be non-empty.
grep -q 'run' "$TARGET_DIR"/man/vard.1 || fail "vard.1 does not name the 'run' subcommand — stale artifact?"
grep -q 'watch' "$TARGET_DIR"/man/vard.1 || fail "vard.1 does not name the 'watch' subcommand — stale artifact?"
grep -q 'run' "$TARGET_DIR"/man/vard-run.1 || fail "vard-run.1 does not name the 'run' subcommand — stale artifact?"
grep -q 'add' "$TARGET_DIR"/man/vard-watch.1 || fail "vard-watch.1 does not name the 'add' subcommand — stale artifact?"

# Help for the watch command tree must render through the custom v2 path.
"$VARD" watch -h | grep -q 'For full help, run' || fail "vard watch -h missing the v2 short-help footer"
"$VARD" watch add --help | grep -q 'canonicalized path' || fail "vard watch add --help missing its prose"
# The watch sync opt-in verb (VRD-40) renders help through the same v2 path.
"$VARD" watch sync -h | grep -q 'For full help, run' || fail "vard watch sync -h missing the v2 short-help footer"
"$VARD" watch sync --help | grep -q 'opt-in' || fail "vard watch sync --help missing its prose"

# The snapshot/history commands (VRD-16) render help through the same v2 path.
"$VARD" snapshot -h | grep -q 'For full help, run' || fail "vard snapshot -h missing the v2 short-help footer"
"$VARD" restore --help | grep -q 'protective snapshot' || fail "vard restore --help missing its prose"
"$VARD" diff --help | grep -q 'text-only' || fail "vard diff --help missing the text-only note"

# The sync command (VRD-19) renders help through the same v2 path.
"$VARD" sync -h | grep -q 'For full help, run' || fail "vard sync -h missing the v2 short-help footer"
"$VARD" sync --help | grep -q 'out of tree' || fail "vard sync --help missing its prose"

# The status/config commands (VRD-17) render help through the same v2 path.
"$VARD" status -h | grep -q 'For full help, run' || fail "vard status -h missing the v2 short-help footer"
"$VARD" config -h | grep -q 'For full help, run' || fail "vard config -h missing the v2 short-help footer"
"$VARD" config set --help | grep -q 'inferred' || fail "vard config set --help missing its prose"

# vard logs (VRD-23): the daemon logfile reader. Help renders through the same
# v2 path. No daemon has run in this throwaway state dir, so there is no logfile
# yet: `vard logs` must report that cleanly and exit 1 (not 0, not a crash).
"$VARD" logs -h | grep -q 'For full help, run' || fail "vard logs -h missing the v2 short-help footer"
"$VARD" logs --help | grep -q 'daily-rolling' || fail "vard logs --help missing its prose"

# vard doctor (VRD-23): the read-only environment diagnosis. Help renders through
# the same v2 path; the functional run is asserted below once a watch exists.
"$VARD" doctor -h | grep -q 'For full help, run' || fail "vard doctor -h missing the v2 short-help footer"
"$VARD" doctor --help | grep -q 'read-only' || fail "vard doctor --help missing its prose"

# Watch command round-trip: add -> list -> pause -> resume -> remove, against a
# throwaway HOME/XDG/git config so nothing touches the developer's real state.
# Requires git on PATH (the release-artifact job has it).
SMOKE_TMP="$(mktemp -d)"
trap 'rm -rf "$SMOKE_TMP"' EXIT
export XDG_CONFIG_HOME="$SMOKE_TMP/config"
export XDG_STATE_HOME="$SMOKE_TMP/state"
export HOME="$SMOKE_TMP/home"
export GIT_CONFIG_GLOBAL="$SMOKE_TMP/gitconfig"
mkdir -p "$HOME"
git config --file "$GIT_CONFIG_GLOBAL" user.email smoke@example.com
git config --file "$GIT_CONFIG_GLOBAL" user.name "Vard Smoke"

WDIR="$SMOKE_TMP/repo"
mkdir -p "$WDIR"
"$VARD" watch add "$WDIR" --name smoke --init </dev/null >/dev/null || fail "vard watch add --init failed"
test -d "$WDIR/.git" || fail "vard watch add --init did not initialize a repository"
grep -q 'vard managed excludes' "$WDIR/.git/info/exclude" || fail "vard watch add did not seed .git/info/exclude"
"$VARD" --format json watch list | grep -q '"name":"smoke"' || fail "vard watch list did not show the added watch"
"$VARD" watch pause smoke >/dev/null || fail "vard watch pause failed"
grep -q 'paused = true' "$XDG_CONFIG_HOME/vard/config.toml" || fail "vard watch pause did not persist paused = true"
"$VARD" watch resume smoke >/dev/null || fail "vard watch resume failed"

# vard watch sync (VRD-40): the syncing opt-in gesture. A plain records-form add
# prints the one-line hint pointing at it; `watch sync --off` writes an explicit
# sync = false pin. The no-remote enable confirmation cycle is covered by the CLI
# test suite (it needs a controlled remote), so the smoke sticks to the
# deterministic hint and --off assertions.
HINTREPO="$SMOKE_TMP/hintrepo"
mkdir -p "$HINTREPO"
git -C "$HINTREPO" init -q -b main
"$VARD" --format records watch add "$HINTREPO" --name hintcheck </dev/null \
  | grep -q 'enable with: vard watch sync hintcheck' \
  || fail "vard watch add did not print the sync opt-in hint"
"$VARD" --format records watch sync hintcheck --off </dev/null \
  | grep -q 'disabled syncing for watch hintcheck' \
  || fail "vard watch sync --off did not report disabling"
grep -q 'sync = false' "$XDG_CONFIG_HOME/vard/config.toml" \
  || fail "vard watch sync --off did not pin sync = false"
"$VARD" watch remove hintcheck </dev/null >/dev/null || fail "cleanup: remove hintcheck failed"

# vard watch set (VRD-48): edit a setting on an existing watch, then clear it.
# Help renders through the same v2 path, and a bare `set` with no flags is a
# usage error.
"$VARD" watch set -h | grep -q 'For full help, run' || fail "vard watch set -h missing the v2 short-help footer"
"$VARD" watch set --help | grep -q 'settable' || fail "vard watch set --help missing its prose"
"$VARD" watch set smoke --interval 45m </dev/null >/dev/null || fail "vard watch set failed"
grep -q 'interval = "45m"' "$XDG_CONFIG_HOME/vard/config.toml" \
  || fail "vard watch set did not persist interval = 45m"
"$VARD" watch set smoke --unset interval </dev/null >/dev/null || fail "vard watch set --unset failed"
if grep -q 'interval = "45m"' "$XDG_CONFIG_HOME/vard/config.toml"; then
  fail "vard watch set --unset did not remove the interval key"
fi
if "$VARD" watch set smoke </dev/null >/dev/null 2>&1; then
  fail "vard watch set with no flags must exit non-zero"
fi

# vard status (VRD-17): no daemon runs in the smoke env, so status reports the
# stopped daemon and exits 1. With nothing monitoring it, the configured watch
# projects to `unknown` (not `ok` — that would falsely imply it is being watched).
if status_out="$("$VARD" --format records status)"; then
  fail "vard status with no daemon must exit non-zero, not 0"
else
  test "$?" -eq 1 || fail "vard status with no daemon must exit 1"
fi
printf '%s\n' "$status_out" | grep -q 'daemon: not running' \
  || fail "vard status did not report the stopped daemon"
printf '%s\n' "$status_out" | grep -q 'smoke: unknown' \
  || fail "vard status did not project the unmonitored smoke watch as unknown"

# vard doctor (VRD-23), functional: read-only, so it must change nothing. With
# git present and no daemon, every check is ok/skipped and it exits 0. Its JSON
# form is a single array carrying the git check row.
doctor_json="$("$VARD" --format json doctor)" || fail "vard doctor must exit 0 when all checks pass"
printf '%s\n' "$doctor_json" | grep -q '"check":"git"' \
  || fail "vard doctor --format json did not emit the git check row"
printf '%s\n' "$doctor_json" | grep -q '"check":"health-file"' \
  || fail "vard doctor --format json did not emit the health-file check row"
printf '%s\n' "$doctor_json" | grep -q '"check":"secret-audit"' \
  || fail "vard doctor --format json did not emit the per-watch secret-audit row"
printf '%s\n' "$doctor_json" | grep -q '"check":"remote-auth"' \
  || fail "vard doctor --format json did not emit the remote-auth check row"
# --offline skips the network check and still exits 0 on this all-clear env.
"$VARD" doctor --offline >/dev/null || fail "vard doctor --offline must exit 0 when all checks pass"
"$VARD" doctor --help | grep -q -- '--offline' \
  || fail "vard doctor --help missing the --offline flag"

# vard config (VRD-17): round-trip a scalar key and locate the config file.
# config path/get are single-value surfaces (VRD-36): piped (as here) they emit
# the bare value — the TEXT form — not the JSON envelope, so `$(vard config
# path)` is directly usable. Assert the bare shape: a line ending in
# config.toml with no JSON braces.
config_path_out="$("$VARD" config path)"
case "$config_path_out" in
  *'{'*|*'}'*) fail "vard config path must emit the bare path, not JSON: $config_path_out" ;;
esac
case "$config_path_out" in
  */config.toml) : ;;
  *) fail "vard config path did not print a bare path ending in config.toml: $config_path_out" ;;
esac
"$VARD" config set defaults.interval 30m >/dev/null || fail "vard config set failed"
grep -q 'interval = "30m"' "$XDG_CONFIG_HOME/vard/config.toml" \
  || fail "vard config set did not persist defaults.interval"
# Piped get defaults to the bare value too — no --format needed.
test "$("$VARD" config get defaults.interval)" = "30m" \
  || fail "vard config get did not read the bare value back"
"$VARD" config unset defaults.interval >/dev/null || fail "vard config unset failed"
if "$VARD" config get defaults.interval >/dev/null; then
  fail "vard config get of an unset key must exit non-zero"
fi

# vard logs (VRD-23), no daemon: nothing has written a logfile in this throwaway
# state dir, so logs reports the missing log and exits 1 (attention), never 0.
if logs_out="$("$VARD" logs 2>&1)"; then
  fail "vard logs with no logfile must exit non-zero, not 0"
else
  test "$?" -eq 1 || fail "vard logs with no logfile must exit 1"
fi
printf '%s\n' "$logs_out" | grep -q 'no daemon logfile yet' \
  || fail "vard logs did not report the missing logfile"
# logs is text-only like diff: an explicit --format json must be rejected (exit 2).
if "$VARD" --format json logs >/dev/null 2>&1; then
  fail "vard --format json logs must be rejected as text-only"
else
  test "$?" -eq 2 || fail "vard --format json logs must exit 2 (text-only rejection)"
fi

# Snapshot/log round-trip (VRD-16), no daemon: an in-process snapshot must land
# a real commit, leave the operation journal compacted (no dangling begin), and
# be visible in the log. A second snapshot with no changes must be a clean no-op.
echo "smoke snapshot content" > "$WDIR/note.txt"
"$VARD" --format json snapshot smoke | grep -q '"status":"committed"' || fail "vard snapshot did not commit"
test "$(git -C "$WDIR" rev-list --count HEAD)" = "1" || fail "vard snapshot did not land exactly one commit"
# The in-process snapshot MUST have written a journal for the watch, and it must
# be compacted to empty (no dangling begin). nullglob keeps a missing journal
# from making the loop vacuously pass on the literal glob pattern.
shopt -s nullglob
journals=("$XDG_STATE_HOME"/vard/journal/*.journal)
shopt -u nullglob
test "${#journals[@]}" -gt 0 || fail "no operation journal was written for the in-process snapshot"
for j in "${journals[@]}"; do
  test ! -s "$j" || fail "operation journal $j holds a dangling begin after a clean snapshot"
done
"$VARD" --format json history smoke | grep -q '"trigger":"manual"' || fail "vard history did not show the manual snapshot"
"$VARD" --format json snapshot smoke | grep -q '"status":"no changes"' || fail "second snapshot was not a clean no-op"
# diff is text-only: an explicit --format json must be rejected (exit 2).
if "$VARD" --format json diff smoke >/dev/null 2>&1; then
  fail "vard --format json diff should be rejected as text-only"
fi

# vard notify (VRD-18): the shell-prompt health hook. No daemon is running in
# the smoke env, so notify must report the daemon-not-running line and exit 1
# (not 0) — the exit code is the contract prompts/tmux/cron branch on.
if notify_out="$("$VARD" notify)"; then
  fail "vard notify with no daemon must exit non-zero, not 0"
else
  test "$?" -eq 1 || fail "vard notify with no daemon must exit 1"
fi
printf '%s\n' "$notify_out" | grep -q 'daemon not running' \
  || fail "vard notify did not report the stopped daemon"
# --format json yields the machine shape: a non-empty problems array carrying
# the daemon-not-running object (an empty array is the healthy case, not this).
if notify_json="$("$VARD" notify --format json)"; then
  fail "vard notify --format json with no daemon must exit non-zero"
else
  test "$?" -eq 1 || fail "vard notify --format json with no daemon must exit 1"
fi
printf '%s\n' "$notify_json" | grep -q '"state":"daemon-not-running"' \
  || fail "vard notify --format json missing the daemon-not-running object"
# Performance contract (structural): notify must not depend on config.toml — it
# reads only the health file and the lock. Move the config aside and prove it
# still runs and reports identically.
mv "$XDG_CONFIG_HOME/vard/config.toml" "$SMOKE_TMP/config-away.toml"
if "$VARD" notify >/dev/null 2>&1; then
  fail "vard notify without a config must still exit non-zero (daemon not running)"
else
  test "$?" -eq 1 || fail "vard notify must not require config.toml (should exit 1 with it removed)"
fi
mv "$SMOKE_TMP/config-away.toml" "$XDG_CONFIG_HOME/vard/config.toml"

"$VARD" watch remove smoke >/dev/null || fail "vard watch remove failed"
test -d "$WDIR/.git" || fail "vard watch remove touched the repository"
test "$("$VARD" --format json watch list)" = "[]" || fail "vard watch list not empty after remove"

# vard sync (VRD-19): with no watches configured there is nothing to sync, so it
# exits 1 (attention) with a clear message — a deterministic check that needs no
# remote (the live cycle is covered by the CLI test suite and daemon test).
if sync_out="$("$VARD" sync 2>&1)"; then
  fail "vard sync with no sync-enabled watches must exit non-zero, not 0"
else
  test "$?" -eq 1 || fail "vard sync with no sync-enabled watches must exit 1"
fi
printf '%s\n' "$sync_out" | grep -q 'no sync-enabled watches' \
  || fail "vard sync did not report the absence of sync-enabled watches"

echo "smoke: all assertions passed ($VARD)"
