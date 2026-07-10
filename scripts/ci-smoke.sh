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
"$VARD" watch remove smoke >/dev/null || fail "vard watch remove failed"
test -d "$WDIR/.git" || fail "vard watch remove touched the repository"
test "$("$VARD" --format json watch list)" = "[]" || fail "vard watch list not empty after remove"

echo "smoke: all assertions passed ($VARD)"
