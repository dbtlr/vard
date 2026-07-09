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
  "$TARGET_DIR"/man/vard-run.1; do
  test -s "$f" || fail "$f is missing or empty — stale artifact?"
  grep -q 'run' "$f" || fail "$f does not name the 'run' subcommand — stale artifact?"
done

echo "smoke: all assertions passed ($VARD)"
