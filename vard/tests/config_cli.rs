//! End-to-end tests for `vard config` (VRD-17), driving the real binary against
//! tempdir-isolated XDG dirs. Covers `path`, the get/set/unset round-trip
//! (including comment preservation and validation refusals), and `edit` with a
//! scripted `$EDITOR` fixture (both a valid save and a rejected one).

use std::process::Stdio;

mod common;
use common::{Env, code, stderr, stdout};

const WITH_COMMENTS: &str = "\
version = 1

# important defaults section
[defaults]
interval = \"15m\"
quiesce = \"10s\"  # settle time
";

#[test]
fn path_prints_the_config_location() {
    let env = Env::new();
    let out = env.vard(&["--format", "records", "config", "path"]);
    assert_eq!(code(&out), 0);
    assert_eq!(stdout(&out).trim(), env.config_path().to_str().unwrap());
}

#[test]
fn path_piped_defaults_to_the_bare_path() {
    // A single-value surface: absent an explicit `--format`, `config path` emits
    // the bare absolute path even when piped (not a TTY), so `$(vard config
    // path)` yields the path alone. No JSON braces.
    let env = Env::new();
    let out = env.vard(&["config", "path"]);
    assert_eq!(code(&out), 0);
    let printed = stdout(&out);
    assert_eq!(printed.trim(), env.config_path().to_str().unwrap());
    assert!(
        !printed.contains('{') && !printed.contains('}'),
        "piped path must be bare, not the JSON object, got: {printed}"
    );
    assert!(
        printed.trim().starts_with('/'),
        "expected a bare absolute path, got: {printed}"
    );
}

#[test]
fn path_explicit_json_still_emits_the_object() {
    let env = Env::new();
    let out = env.vard(&["--format", "json", "config", "path"]);
    assert_eq!(code(&out), 0);
    assert!(
        stdout(&out).contains(r#""path":"#) && stdout(&out).contains('{'),
        "explicit --format json must emit the {{path}} object, got: {}",
        stdout(&out)
    );
}

#[test]
fn set_get_unset_round_trip_preserves_comments() {
    let env = Env::new();
    env.write_config(WITH_COMMENTS);

    // set: change a key.
    let set = env.vard(&["config", "set", "defaults.interval", "30m"]);
    assert_eq!(code(&set), 0, "set failed: {}", stderr(&set));
    let written = env.read_config();
    assert!(written.contains("interval = \"30m\""), "got: {written}");
    // Comments elsewhere survive the edit.
    assert!(
        written.contains("# important defaults section"),
        "header comment lost: {written}"
    );
    assert!(
        written.contains("# settle time"),
        "sibling inline comment lost: {written}"
    );

    // get: a single-value surface, so the piped default (no `--format`) is the
    // bare value, not a JSON object — the scripting ergonomics.
    let get = env.vard(&["config", "get", "defaults.interval"]);
    assert_eq!(code(&get), 0);
    assert_eq!(stdout(&get).trim(), "30m");
    assert!(
        !stdout(&get).contains('{'),
        "piped get must be bare, got: {}",
        stdout(&get)
    );

    // get JSON: the {key, value} object.
    let get_json = env.vard(&["--format", "json", "config", "get", "defaults.interval"]);
    assert!(
        stdout(&get_json).contains(r#""key":"defaults.interval""#)
            && stdout(&get_json).contains(r#""value":"30m""#),
        "got: {}",
        stdout(&get_json)
    );

    // unset: remove it.
    let unset = env.vard(&["config", "unset", "defaults.interval"]);
    assert_eq!(code(&unset), 0, "unset failed: {}", stderr(&unset));
    assert!(
        !env.read_config().contains("interval"),
        "{}",
        env.read_config()
    );

    // get after unset: exit 1, empty stdout (the `git config` contract).
    let gone = env.vard(&["config", "get", "defaults.interval"]);
    assert_eq!(code(&gone), 1, "an unset key must exit 1");
    assert!(
        stdout(&gone).is_empty(),
        "must be empty, got: {}",
        stdout(&gone)
    );
}

#[test]
fn set_infers_a_boolean_value() {
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["config", "set", "defaults.sync", "true"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(
        env.read_config().contains("sync = true"),
        "{}",
        env.read_config()
    );
}

#[test]
fn set_rejects_a_value_that_would_break_the_config() {
    let env = Env::new();
    let original = "version = 1\n\n[daemon]\nlog_retention_days = 14\n";
    env.write_config(original);

    // A non-integer value for an integer key: valid TOML string, invalid schema.
    let out = env.vard(&["config", "set", "daemon.log_retention_days", "forever"]);
    assert_eq!(code(&out), 2, "a valid→invalid set must exit 2");
    assert!(
        stderr(&out).contains("would make a valid config invalid"),
        "got: {}",
        stderr(&out)
    );
    assert_eq!(env.read_config(), original, "the config must be untouched");
}

#[test]
fn set_on_a_watch_key_points_at_the_watch_verbs() {
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["config", "set", "watch.0.name", "notes"]);
    assert_eq!(code(&out), 2);
    // The write pointer names the setting-editing verb specifically.
    assert!(
        stderr(&out).contains("vard watch set"),
        "got: {}",
        stderr(&out)
    );
}

#[test]
fn set_on_version_is_refused() {
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["config", "set", "version", "2"]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("not settable"),
        "got: {}",
        stderr(&out)
    );
}

#[test]
fn unset_a_missing_key_is_an_error() {
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["config", "unset", "defaults.interval"]);
    assert_eq!(code(&out), 2);
    assert!(stderr(&out).contains("is not set"), "got: {}", stderr(&out));
}

#[test]
fn edit_installs_a_valid_result_from_the_editor() {
    let env = Env::new();
    env.write_config("version = 1\n\n[daemon]\nlog_level = \"info\"\n");
    let editor = env.editor_writing("version = 1\n\n[daemon]\nlog_level = \"debug\"\n");

    let out = env
        .command(&["config", "edit"])
        .env("EDITOR", &editor)
        .stdin(Stdio::null())
        .output()
        .expect("spawn vard");
    assert_eq!(code(&out), 0, "edit failed: {}", stderr(&out));
    assert!(
        env.read_config().contains("log_level = \"debug\""),
        "got: {}",
        env.read_config()
    );
}

#[test]
fn edit_rejection_preserves_the_temp_file_and_the_config() {
    let env = Env::new();
    let original = "version = 1\n\n[daemon]\nlog_level = \"info\"\n";
    env.write_config(original);
    // Valid TOML, but no version ⇒ schema-invalid: a valid→invalid edit.
    let editor = env.editor_writing("[daemon]\nlog_level = \"debug\"\n");

    let out = env
        .command(&["config", "edit"])
        .env("EDITOR", &editor)
        .stdin(Stdio::null())
        .output()
        .expect("spawn vard");
    assert_eq!(code(&out), 2, "a rejected edit must exit 2");
    assert!(
        stderr(&out).contains("preserved at"),
        "must name the preserved temp file, got: {}",
        stderr(&out)
    );
    assert_eq!(env.read_config(), original, "the config must be untouched");
    // The preserved temp scratch still exists next to the config.
    let leftovers: Vec<_> = std::fs::read_dir(env.config_home.join("vard"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".config-edit-"))
        .collect();
    assert_eq!(leftovers.len(), 1, "the edit must be preserved");
}

#[test]
fn edit_without_an_editor_configured_errors() {
    let env = Env::new();
    env.write_config("version = 1\n");
    // Env::command already removes EDITOR and VISUAL.
    let out = env.vard(&["config", "edit"]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("no editor configured"),
        "got: {}",
        stderr(&out)
    );
}

#[test]
fn edit_prefers_visual_over_editor() {
    // C5: $VISUAL wins over $EDITOR (the git/historical convention).
    let env = Env::new();
    env.write_config("version = 1\n\n[daemon]\nlog_level = \"info\"\n");
    let visual = env.editor_writing("version = 1\n\n[daemon]\nlog_level = \"trace\"\n");
    let editor = env.editor_writing("version = 1\n\n[daemon]\nlog_level = \"debug\"\n");

    let out = env
        .command(&["config", "edit"])
        .env("VISUAL", &visual)
        .env("EDITOR", &editor)
        .stdin(Stdio::null())
        .output()
        .expect("spawn vard");
    assert_eq!(code(&out), 0, "edit failed: {}", stderr(&out));
    assert!(
        env.read_config().contains("log_level = \"trace\""),
        "VISUAL must win over EDITOR, got: {}",
        env.read_config()
    );
}

#[test]
fn get_reads_the_managed_version() {
    // F10: `config get version` prints the value and exits 0 (version is
    // readable, though not settable).
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["--format", "records", "config", "get", "version"]);
    assert_eq!(code(&out), 0, "got: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "1");
}

#[test]
fn get_of_a_watch_key_points_at_watch_list() {
    // F10: a read of a watch.* key points at inspection, not the mutation verbs.
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["config", "get", "watch.0.name"]);
    assert_eq!(code(&out), 2);
    assert!(
        stderr(&out).contains("vard watch list"),
        "got: {}",
        stderr(&out)
    );
}

#[test]
fn get_of_a_boolean_is_a_bare_json_bool() {
    // F12: a boolean key emits a JSON boolean, not a stringified "true".
    let env = Env::new();
    env.write_config("version = 1\n\n[defaults]\nsync = true\n");
    let out = env.vard(&["--format", "json", "config", "get", "defaults.sync"]);
    assert_eq!(code(&out), 0, "got: {}", stderr(&out));
    assert!(
        stdout(&out).contains(r#""value":true"#),
        "expected a bare JSON bool, got: {}",
        stdout(&out)
    );
}

#[test]
fn set_a_bare_number_for_a_duration_surfaces_the_parse_error() {
    // F3: `defaults.interval 3600` infers an integer the schema rejects; the
    // string retry ("3600") fails duration parsing, so that parse error surfaces
    // rather than the opaque integer type error.
    let env = Env::new();
    let original = "version = 1\n";
    env.write_config(original);
    let out = env.vard(&["config", "set", "defaults.interval", "3600"]);
    assert_eq!(code(&out), 2, "both forms invalid must exit 2");
    assert!(
        stderr(&out).contains("missing unit") || stderr(&out).contains("invalid duration"),
        "expected the duration parse error, got: {}",
        stderr(&out)
    );
    assert_eq!(env.read_config(), original, "the config must be untouched");
}

#[test]
fn set_a_bare_integer_for_an_integer_key_stays_an_integer() {
    // F3: a valid integer key keeps the integer type (no string retry).
    let env = Env::new();
    env.write_config("version = 1\n");
    let out = env.vard(&["config", "set", "daemon.log_retention_days", "14"]);
    assert_eq!(code(&out), 0, "got: {}", stderr(&out));
    assert!(
        env.read_config().contains("log_retention_days = 14"),
        "must stay a bare integer, got: {}",
        env.read_config()
    );
    // The confirmation JSON reports the value typed as an integer.
    let json = env.vard(&[
        "--format",
        "json",
        "config",
        "set",
        "daemon.log_retention_days",
        "30",
    ]);
    assert!(
        stdout(&json).contains(r#""value":30"#),
        "set must report the stored typed value, got: {}",
        stdout(&json)
    );
}

#[test]
fn set_a_negative_integer_surfaces_the_range_error_not_a_type_error() {
    // A negative value for the u32 `log_retention_days`: the inferred integer
    // matches the field's type and fails on the range, so the accurate u32 error
    // must surface — never masked behind the string candidate's type error. The
    // `--` guards the leading-`-` value from being parsed as a flag.
    let env = Env::new();
    let original = "version = 1\n\n[daemon]\nlog_retention_days = 14\n";
    env.write_config(original);
    let out = env.vard(&["config", "set", "daemon.log_retention_days", "--", "-5"]);
    assert_eq!(code(&out), 2, "a valid→invalid set must exit 2");
    assert!(
        stderr(&out).contains("u32"),
        "expected the u32 range error, got: {}",
        stderr(&out)
    );
    assert!(
        !stderr(&out).contains("invalid type: string"),
        "the string candidate's type error must not mask the range error, got: {}",
        stderr(&out)
    );
    assert_eq!(env.read_config(), original, "the config must be untouched");
}

#[test]
fn set_repairs_a_broken_config_with_a_warning() {
    // Repair mode: the config is already invalid (two watches share a name). An
    // unrelated, well-typed edit must be allowed through — written, with an
    // exit-1 warning that the config is still not fully valid — rather than
    // trapping the user behind the pre-existing breakage.
    let env = Env::new();
    env.write_config(
        "version = 1\n\n\
         [[watch]]\nname = \"dup\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"dup\"\npath = \"/b\"\n",
    );
    let out = env.vard(&["config", "set", "daemon.log_level", "debug"]);
    assert_eq!(
        code(&out),
        1,
        "a repair write must exit 1: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("still not fully valid"),
        "expected the still-invalid warning, got: {}",
        stderr(&out)
    );
    assert!(
        env.read_config().contains("log_level = \"debug\""),
        "the repair edit must land, got: {}",
        env.read_config()
    );
}

#[test]
fn set_on_a_broken_config_writes_the_fallback_candidate() {
    // Repair mode where neither candidate cleanly resolves the field: setting
    // `defaults.interval 3600` on an already-broken config falls back to the
    // inferred integer per the selection rule, written with an exit-1 warning.
    let env = Env::new();
    env.write_config(
        "version = 1\n\n\
         [[watch]]\nname = \"dup\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"dup\"\npath = \"/b\"\n",
    );
    let out = env.vard(&["config", "set", "defaults.interval", "3600"]);
    assert_eq!(
        code(&out),
        1,
        "a repair write must exit 1: {}",
        stderr(&out)
    );
    assert!(
        env.read_config().contains("interval = 3600"),
        "the fallback (inferred integer) must be stored, got: {}",
        env.read_config()
    );
}
