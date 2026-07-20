//! End-to-end tests for `vard self-update` (VRD-25) that drive the real binary
//! against a tempdir-isolated config/HOME. These exercise only the paths that
//! need no network — help rendering and the install-receipt gate — so nothing
//! here touches the real network or the developer's environment. The full
//! manifest → download → verify → swap flow is covered by the mock-server unit
//! tests inside `src/self_update`.

mod common;
use common::{Env, code, stderr, stdout};

#[test]
fn help_renders_through_the_v2_path() {
    let env = Env::new();
    let out = env.vard(&["self-update", "-h"]);
    assert!(
        out.status.success(),
        "self-update -h failed: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("For full help, run"),
        "short help missing the v2 footer: {}",
        stdout(&out)
    );

    let long = env.vard(&["self-update", "--help"]);
    assert!(long.status.success());
    let text = stdout(&long);
    assert!(
        text.contains("install receipt"),
        "long help missing prose: {text}"
    );
    assert!(
        text.contains("--version"),
        "long help missing --version: {text}"
    );
    assert!(
        text.contains("--dry-run"),
        "long help missing --dry-run: {text}"
    );
}

#[test]
fn missing_receipt_blocks_with_exit_1() {
    // A fresh isolated env has no install receipt: the gate blocks before any
    // network access, exits 1 (attention), and points at the installer.
    let env = Env::new();
    let out = env.vard(&["self-update"]);
    assert_eq!(code(&out), 1, "a receipt block must exit 1");
    let err = stderr(&out);
    assert!(err.contains("install receipt"), "message missing: {err}");
    assert!(
        err.contains("installer"),
        "message should point at the installer: {err}"
    );
    // Nothing is written to stdout on a block.
    assert!(
        stdout(&out).is_empty(),
        "unexpected stdout: {}",
        stdout(&out)
    );
}

#[test]
fn missing_receipt_dry_run_also_blocks_before_the_network() {
    // The receipt gate runs before anything else, so even --dry-run blocks with
    // no receipt — and therefore never reaches the network.
    let env = Env::new();
    let out = env.vard(&["self-update", "--dry-run"]);
    assert_eq!(code(&out), 1);
    assert!(
        stderr(&out).contains("install receipt"),
        "got: {}",
        stderr(&out)
    );
}

#[test]
fn json_block_still_reports_on_stderr() {
    // A block is an error, not a report: even under --format json the message
    // goes to stderr and stdout stays empty.
    let env = Env::new();
    let out = env
        .command(&["--format", "json", "self-update"])
        .output()
        .unwrap();
    assert_eq!(code(&out), 1);
    assert!(
        stdout(&out).is_empty(),
        "no JSON report on a block: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("install receipt"),
        "got: {}",
        stderr(&out)
    );
}
