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

# The snapshot/history commands (VRD-16) render help through the same v2 path.
"$VARD" snapshot -h | grep -q 'For full help, run' || fail "vard snapshot -h missing the v2 short-help footer"
"$VARD" restore --help | grep -q 'protective snapshot' || fail "vard restore --help missing its prose"
"$VARD" diff --help | grep -q 'text-only' || fail "vard diff --help missing the text-only note"

# The status/config commands (VRD-17) render help through the same v2 path.
"$VARD" status -h | grep -q 'For full help, run' || fail "vard status -h missing the v2 short-help footer"
"$VARD" config -h | grep -q 'For full help, run' || fail "vard config -h missing the v2 short-help footer"
"$VARD" config set --help | grep -q 'inferred' || fail "vard config set --help missing its prose"

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

# vard config (VRD-17): round-trip a scalar key and locate the config file.
"$VARD" config path | grep -q 'config.toml' || fail "vard config path did not print the location"
"$VARD" config set defaults.interval 30m >/dev/null || fail "vard config set failed"
grep -q 'interval = "30m"' "$XDG_CONFIG_HOME/vard/config.toml" \
  || fail "vard config set did not persist defaults.interval"
test "$("$VARD" --format records config get defaults.interval)" = "30m" \
  || fail "vard config get did not read the value back"
"$VARD" config unset defaults.interval >/dev/null || fail "vard config unset failed"
if "$VARD" config get defaults.interval >/dev/null; then
  fail "vard config get of an unset key must exit non-zero"
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
"$VARD" --format json log smoke | grep -q '"trigger":"manual"' || fail "vard log did not show the manual snapshot"
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

echo "smoke: all assertions passed ($VARD)"
