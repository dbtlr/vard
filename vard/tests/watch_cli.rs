//! End-to-end tests for `vard watch add/remove/list/pause/resume`, driving the
//! real binary against tempdir-isolated config, state, and HOME — nothing here
//! touches the developer's real environment. Repositories are created with a
//! throwaway global git config so commits and inits are deterministic in CI.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::{Env, code, stderr, stdout};

/// Runs `git <args>` against the throwaway global git config, asserting success.
fn run_git(git_config: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", git_config)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?} failed");
}

/// A directory that is already a git repository on branch `main`.
fn repo(env: &Env, name: &str) -> PathBuf {
    let path = env.root.path().join(name);
    std::fs::create_dir_all(&path).unwrap();
    let out = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["init", "-q", "-b", "main"])
        .env("GIT_CONFIG_GLOBAL", &env.git_config)
        .output()
        .expect("git init");
    assert!(
        out.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // canonicalize so assertions compare against what vard stores.
    std::fs::canonicalize(&path).unwrap()
}

#[test]
fn add_existing_repo_registers_and_lists() {
    let env = Env::new();
    let path = repo(&env, "notes");
    let path_str = path.to_str().unwrap();

    let out = env.vard(&["watch", "add", path_str, "--name", "notes"]);
    assert!(out.status.success(), "add failed: {}", stderr(&out));

    let list = env.vard(&["--format", "json", "watch", "list"]);
    let json = stdout(&list);
    assert!(json.contains("\"name\":\"notes\""), "got: {json}");
    assert!(json.contains("\"paused\":false"), "got: {json}");
    // The stored path is the canonical one.
    assert!(json.contains(path_str), "got: {json}");
}

#[test]
fn add_non_repo_without_init_fails_noninteractively() {
    let env = Env::new();
    let dir = env.root.path().join("plain");
    std::fs::create_dir_all(&dir).unwrap();

    let out = env.vard(&["watch", "add", dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--init"), "stderr: {}", stderr(&out));
    // Nothing was written.
    assert!(env.config_text().is_empty());
}

#[test]
fn add_non_repo_with_init_creates_repository() {
    let env = Env::new();
    let dir = env.root.path().join("fresh");
    std::fs::create_dir_all(&dir).unwrap();

    let out = env.vard(&[
        "watch",
        "add",
        dir.to_str().unwrap(),
        "--init",
        "--branch",
        "main",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"initialized\":true"),
        "got: {}",
        stdout(&out)
    );
    // A real repository now exists.
    assert!(dir.join(".git").exists());
}

#[test]
fn add_seeds_git_excludes_idempotently() {
    let env = Env::new();
    let path = repo(&env, "proj");
    let exclude = path.join(".git").join("info").join("exclude");

    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "proj"]);
    let first = std::fs::read_to_string(&exclude).unwrap();
    assert!(first.contains("node_modules/"));
    assert!(first.contains(".env"));
    assert!(first.contains("*.pem"));

    // Re-add: the managed block must not be duplicated.
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "proj"]);
    let second = std::fs::read_to_string(&exclude).unwrap();
    assert_eq!(
        second.matches(">>> vard managed excludes >>>").count(),
        1,
        "exclude block duplicated on re-add"
    );
}

#[test]
fn pause_and_resume_preserve_comments() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    // Inject a hand-written comment ahead of the config.
    let cfg = env.config_path();
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("# keep this note\n{original}")).unwrap();

    let out = env.vard(&["watch", "pause", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let paused = env.config_text();
    assert!(
        paused.contains("# keep this note"),
        "comment lost: {paused}"
    );
    assert!(paused.contains("paused = true"), "got: {paused}");

    let out = env.vard(&["watch", "resume", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let resumed = env.config_text();
    assert!(
        resumed.contains("# keep this note"),
        "comment lost: {resumed}"
    );
    assert!(
        !resumed.contains("paused"),
        "paused key not cleared: {resumed}"
    );
}

#[test]
fn remove_leaves_repository_intact() {
    let env = Env::new();
    let path = repo(&env, "notes");
    std::fs::write(path.join("a.txt"), "hi").unwrap();
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "remove", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // Config no longer names it; the repo and file are untouched.
    assert!(!env.config_text().contains("name = \"notes\""));
    assert!(path.join(".git").exists());
    assert!(path.join("a.txt").exists());
}

#[test]
fn remove_by_path_selects_the_watch() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "remove", path.to_str().unwrap()]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let list = env.vard(&["--format", "json", "watch", "list"]);
    assert_eq!(stdout(&list).trim(), "[]");
}

#[test]
fn readd_existing_name_at_new_path_relinks() {
    let env = Env::new();
    let first = repo(&env, "one");
    let second = repo(&env, "two");
    env.vard(&["watch", "add", first.to_str().unwrap(), "--name", "w"]);

    let out = env.vard(&["watch", "add", second.to_str().unwrap(), "--name", "w"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"relinked\":true"),
        "got: {}",
        stdout(&out)
    );

    // Exactly one watch, now pointing at the second path.
    let list = env.vard(&["--format", "json", "watch", "list"]);
    let json = stdout(&list);
    assert_eq!(json.matches("\"name\":\"w\"").count(), 1, "got: {json}");
    assert!(json.contains(second.to_str().unwrap()), "got: {json}");
    assert!(
        !json.contains(first.to_str().unwrap()),
        "old path lingered: {json}"
    );
}

#[test]
fn remove_unknown_watch_is_an_error() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "remove", "ghost"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("ghost"), "stderr: {}", stderr(&out));
}

/// Number of `*.journal` files under the isolated state's journal dir.
fn journal_count(env: &Env) -> usize {
    std::fs::read_dir(env.journal_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|n| n.ends_with(".journal"))
                })
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn remove_purge_deletes_the_watchs_journal() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    // An in-process snapshot (no daemon) brackets the operation in the journal,
    // leaving a path-keyed file behind.
    std::fs::write(path.join("a.txt"), "hi").unwrap();
    let snap = env.vard(&["snapshot", "notes"]);
    assert!(snap.status.success(), "snapshot failed: {}", stderr(&snap));
    assert_eq!(journal_count(&env), 1, "the snapshot must leave a journal");

    let out = env.vard(&["watch", "remove", "notes", "--purge"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"purged\":true"),
        "got: {}",
        stdout(&out)
    );
    assert_eq!(journal_count(&env), 0, "purge must delete the journal");
}

#[test]
fn remove_without_purge_keeps_the_watchs_journal() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    std::fs::write(path.join("a.txt"), "hi").unwrap();
    env.vard(&["snapshot", "notes"]);
    assert_eq!(journal_count(&env), 1);

    let out = env.vard(&["watch", "remove", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // Metadata is kept so re-adding the same path resumes cleanly.
    assert_eq!(
        journal_count(&env),
        1,
        "a plain remove keeps the watch's journal"
    );
}

#[test]
fn list_with_no_config_is_empty() {
    let env = Env::new();
    let out = env.vard(&["--format", "json", "watch", "list"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "[]");
}

/// Writes `text` to the environment's config path, creating its directory.
fn write_config(env: &Env, text: &str) {
    let cfg = env.config_path();
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, text).unwrap();
}

#[test]
fn inline_watch_array_errors_cleanly_without_wipe_or_panic() {
    // The read layer tolerates `watch = [{...}]`, but the comment-preserving
    // editor must refuse it — never coercing (which would drop the watch) nor
    // panicking on an index from a different parse.
    let env = Env::new();
    let path = repo(&env, "notes");
    let inline = format!(
        "version = 1\nwatch = [{{ name = \"notes\", path = {:?} }}]\n",
        path.to_str().unwrap()
    );
    write_config(&env, &inline);

    // pause: exit 2, actionable message, config byte-identical (no wipe).
    let out = env.vard(&["watch", "pause", "notes"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("[[watch]]"),
        "message should tell the user to rewrite as [[watch]]: {}",
        stderr(&out)
    );
    assert_eq!(env.config_text(), inline, "pause must not touch the file");

    // add: also refused, and the file is left untouched (no watches destroyed).
    let out = env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "other"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert_eq!(env.config_text(), inline, "add must not wipe the file");
}

#[test]
fn mutating_a_symlinked_config_preserves_the_symlink() {
    // config.toml is a symlink to a real file elsewhere; a mutation must edit
    // the real file (via the resolved directory) and leave the link intact.
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let cfg = env.config_path();
    let real = env.root.path().join("real-config.toml");
    std::fs::rename(&cfg, &real).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &cfg).unwrap();

    let out = env.vard(&["watch", "pause", "notes"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // The config path is still a symlink pointing at the real file...
    assert!(
        std::fs::symlink_metadata(&cfg)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the symlink must survive the mutation"
    );
    assert_eq!(std::fs::read_link(&cfg).unwrap(), real);
    // ...and the edit landed in the real file.
    assert!(
        std::fs::read_to_string(&real)
            .unwrap()
            .contains("paused = true")
    );
}

#[test]
fn add_fails_before_writing_when_defaults_make_the_watch_invalid() {
    // A [defaults] interval of 0s makes every inheriting watch invalid. The add
    // must be refused by the pre-write revalidation, leaving the config as it
    // was — never installing a config that would wedge the daemon's reloads.
    let env = Env::new();
    let path = repo(&env, "notes");
    write_config(&env, "version = 1\n\n[defaults]\ninterval = \"0s\"\n");
    let before = env.config_text();

    let out = env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("interval"),
        "error should name the offending field: {}",
        stderr(&out)
    );
    assert_eq!(env.config_text(), before, "no watch may be written");
}

#[test]
fn list_warns_but_still_lists_a_duplicate_name_config() {
    // A duplicate name fails full resolution, but `list` is the one read-only
    // diagnostic — it must render the watches as written and exit 1 (attention),
    // never exit 2 on the very config it exists to inspect.
    let env = Env::new();
    write_config(
        &env,
        "version = 1\n\n[[watch]]\nname = \"dup\"\npath = \"/a\"\n\n[[watch]]\nname = \"dup\"\npath = \"/b\"\n",
    );

    let out = env.vard(&["--format", "json", "watch", "list"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let json = stdout(&out);
    assert_eq!(json.matches("\"name\":\"dup\"").count(), 2, "got: {json}");
    assert!(
        stderr(&out).contains("not fully valid"),
        "a warning must be emitted: {}",
        stderr(&out)
    );
}

#[test]
fn list_emits_sync_as_a_boolean_in_both_valid_and_lenient_paths() {
    // The machine JSON must carry `sync` with the same type — boolean, or null
    // when genuinely unknown — regardless of whether the config fully resolved,
    // so a consumer's parse never depends on config validity.
    let env = Env::new();

    // Valid path: a resolved watch lists `sync` as a JSON boolean.
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    let list = env.vard(&["--format", "json", "watch", "list"]);
    assert!(list.status.success(), "stderr: {}", stderr(&list));
    let json = stdout(&list);
    assert!(
        json.contains("\"sync\":true") || json.contains("\"sync\":false"),
        "valid path must emit sync as a boolean: {json}"
    );
    assert!(
        !json.contains("\"sync\":\"yes\"") && !json.contains("\"sync\":\"no\""),
        "sync must never be a string: {json}"
    );

    // Lenient path: an invalid (duplicate-name) config still renders, and `sync`
    // keeps the same JSON types — boolean when set, null when unset.
    write_config(
        &env,
        "version = 1\n\n[[watch]]\nname = \"dup\"\npath = \"/a\"\nsync = false\n\n\
         [[watch]]\nname = \"dup\"\npath = \"/b\"\n",
    );
    let list = env.vard(&["--format", "json", "watch", "list"]);
    assert_eq!(list.status.code(), Some(1), "stderr: {}", stderr(&list));
    let json = stdout(&list);
    assert!(
        json.contains("\"sync\":false"),
        "a set sync must render as a boolean: {json}"
    );
    assert!(
        json.contains("\"sync\":null"),
        "an unset sync must render as null: {json}"
    );
    assert!(
        !json.contains("\"sync\":\"no\""),
        "sync must never be a string: {json}"
    );
}

#[test]
fn remove_repairs_an_already_invalid_duplicate_path_config() {
    // A hand-edited config with two watches on the same path is already invalid
    // (a duplicate path). Removing one of the pair is the natural repair, and it
    // must succeed (exit 0) — the pre-write revalidation refuses only edits that
    // take a *valid* config to invalid, never edits that repair a broken one.
    let env = Env::new();
    write_config(
        &env,
        "version = 1\n\n[[watch]]\nname = \"one\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"two\"\npath = \"/a\"\n",
    );

    let out = env.vard(&["watch", "remove", "one"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    // The surviving config resolves cleanly: `list` exits 0 with a single watch.
    let list = env.vard(&["--format", "json", "watch", "list"]);
    assert!(list.status.success(), "stderr: {}", stderr(&list));
    let json = stdout(&list);
    assert_eq!(json.matches("\"name\":").count(), 1, "got: {json}");
    assert!(json.contains("\"name\":\"two\""), "got: {json}");
}

#[test]
fn pause_on_an_already_invalid_config_writes_but_warns() {
    // Pausing a watch unrelated to a pre-existing duplicate-path breakage must
    // not be blocked: the edit did not introduce the invalidity, so it is
    // honored (the pause lands) but flagged — exit 1 (attention) with a warning
    // that the config is still not fully valid.
    let env = Env::new();
    write_config(
        &env,
        "version = 1\n\n[[watch]]\nname = \"one\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"two\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"other\"\npath = \"/b\"\n",
    );

    let out = env.vard(&["watch", "pause", "other"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("still not fully valid"),
        "a warning must name the remaining invalidity: {}",
        stderr(&out)
    );
    // The pause landed despite the pre-existing invalidity.
    assert!(
        env.config_text().contains("paused = true"),
        "the pause must be written: {}",
        env.config_text()
    );
}

#[test]
fn remove_purge_on_an_already_invalid_config_still_purges_and_reports() {
    // The attention outcome (config was already invalid, stays invalid) must
    // not short-circuit the post-write steps: once the removal is written the
    // watch can no longer be selected, so a skipped purge would orphan its
    // journal with no CLI path to clean it, and a suppressed success line
    // would report a landed write as if it hadn't happened.
    let env = Env::new();
    write_config(
        &env,
        "version = 1\n\n[[watch]]\nname = \"one\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"two\"\npath = \"/a\"\n\n\
         [[watch]]\nname = \"other\"\npath = \"/b\"\n",
    );

    let out = env.vard(&["watch", "remove", "other", "--purge"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("still not fully valid"),
        "the remaining invalidity must be flagged: {}",
        stderr(&out)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"purged\":true"),
        "the purge confirmation must still be reported: {stdout}"
    );
    assert!(
        !env.config_text().contains("other"),
        "the removal must be written: {}",
        env.config_text()
    );
}

#[test]
fn add_that_breaks_a_valid_config_is_refused() {
    // Starting from a valid config, an add that would make it invalid (here a
    // [defaults] interval of 0s that every inheriting watch rejects) is a hard
    // refusal — exit 2, config untouched. The CLI must never introduce breakage.
    let env = Env::new();
    let path = repo(&env, "notes");
    write_config(&env, "version = 1\n\n[defaults]\ninterval = \"0s\"\n");
    let before = env.config_text();

    let out = env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("interval"),
        "the error should name the offending field: {}",
        stderr(&out)
    );
    assert_eq!(
        env.config_text(),
        before,
        "a breaking add must write nothing"
    );
}

#[test]
fn add_registers_a_linked_worktree_writing_the_shared_exclude() {
    // `.git` in a linked worktree is a file, not a directory. The exclude file
    // must be resolved via git (the shared info/exclude), and the add succeeds.
    let env = Env::new();
    let main = repo(&env, "main");
    // A worktree needs a commit to branch from: stage a file, then commit it.
    std::fs::write(main.join("seed.txt"), "x").unwrap();
    run_git(
        &env.git_config,
        &["-C", main.to_str().unwrap(), "add", "-A"],
    );
    run_git(
        &env.git_config,
        &[
            "-C",
            main.to_str().unwrap(),
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "seed",
        ],
    );
    let wt = env.root.path().join("wt");
    run_git(
        &env.git_config,
        &[
            "-C",
            main.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "wtbranch",
            wt.to_str().unwrap(),
        ],
    );
    let wt_canon = std::fs::canonicalize(&wt).unwrap();

    let out = env.vard(&["watch", "add", wt_canon.to_str().unwrap(), "--name", "wt"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        wt.join(".git").is_file(),
        "a linked worktree's .git is a gitlink file, not a dir"
    );
    // The managed block landed in the shared exclude under the main repo's .git.
    let shared = main.join(".git").join("info").join("exclude");
    assert!(
        std::fs::read_to_string(&shared)
            .unwrap()
            .contains("vard managed excludes"),
        "excludes should be written to the shared info/exclude"
    );
    // And the watch is registered.
    let list = env.vard(&["--format", "json", "watch", "list"]);
    assert!(
        stdout(&list).contains("\"name\":\"wt\""),
        "got: {}",
        stdout(&list)
    );
}

// --- sync opt-in (VRD-40) --------------------------------------------------

#[test]
fn watch_sync_enables_and_confirms_against_a_no_remote_repo() {
    // The opt-in gesture: `watch sync <name>` writes sync = true, then runs one
    // confirmation cycle. A repo with no configured remote is still enabled; the
    // cycle honestly reports the missing remote (proving it ran for this watch)
    // and the records output points at `git remote add`, exiting 1.
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["--format", "records", "watch", "sync", "notes"]);
    assert_eq!(
        code(&out),
        1,
        "no-remote confirmation exits 1: {}",
        stderr(&out)
    );
    assert!(
        env.config_text().contains("sync = true"),
        "enable must persist sync = true: {}",
        env.config_text()
    );
    let text = stdout(&out);
    assert!(
        text.contains("name     notes") && text.contains("status   disabled"),
        "the cycle must run and report the notes watch: {text}"
    );
    assert!(
        text.contains("no remote \"origin\" in the repository"),
        "the no-remote reason must appear: {text}"
    );
    assert!(
        text.contains("git remote add origin"),
        "the records output must point at how to add a remote: {text}"
    );
}

#[test]
fn watch_sync_off_pins_sync_false_and_runs_no_cycle() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    env.vard(&["watch", "sync", "notes"]);

    let out = env.vard(&["--format", "records", "watch", "sync", "notes", "--off"]);
    assert!(out.status.success(), "--off must exit 0: {}", stderr(&out));
    assert!(
        stdout(&out).contains("disabled syncing for watch notes"),
        "off reports plainly, no sync cycle: {}",
        stdout(&out)
    );
    let cfg = env.config_text();
    assert!(
        cfg.contains("sync = false"),
        "off must pin sync = false: {cfg}"
    );
    assert!(
        !cfg.contains("sync = true"),
        "no stale sync = true key: {cfg}"
    );
}

#[test]
fn watch_sync_selects_by_path() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    // By path (not name): the write must land on the notes watch.
    let out = env.vard(&[
        "--format",
        "json",
        "watch",
        "sync",
        path.to_str().unwrap(),
        "--off",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("\"sync\":false"),
        "got: {}",
        stdout(&out)
    );
    assert!(env.config_text().contains("sync = false"));
}

#[test]
fn watch_sync_unknown_watch_is_an_error() {
    // Consistent with pause/resume/remove: an unresolved selector exits 2 and
    // names the selector.
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "sync", "ghost"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("ghost"), "stderr: {}", stderr(&out));
}

#[test]
fn watch_sync_enable_is_idempotent() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    env.vard(&["watch", "sync", "notes"]);
    env.vard(&["watch", "sync", "notes"]);
    let cfg = env.config_text();
    assert_eq!(
        cfg.matches("sync =").count(),
        1,
        "re-enabling must not duplicate the sync key: {cfg}"
    );
    assert!(cfg.contains("sync = true"), "got: {cfg}");
}

#[test]
fn add_sync_writes_sync_true_in_the_new_entry() {
    let env = Env::new();
    let path = repo(&env, "notes");
    // The repo has no remote, so the confirmation cycle reports disabled (exit
    // 1); the entry write is what this test asserts.
    let out = env.vard(&[
        "watch",
        "add",
        path.to_str().unwrap(),
        "--name",
        "notes",
        "--sync",
    ]);
    assert!(
        env.config_text().contains("sync = true"),
        "add --sync must write sync = true: {}",
        env.config_text()
    );
    // The add itself succeeded; the confirmation cycle then flagged the missing
    // remote (exit 1). Either way the write landed.
    assert!(
        stdout(&out).contains("added watch notes") || code(&out) == 1,
        "stdout: {} / stderr: {}",
        stdout(&out),
        stderr(&out)
    );
}

#[test]
fn add_sync_json_is_a_single_document() {
    // Finding 2: `add --sync --format json` must fold the confirmation cycle
    // into the single add object (nested under "sync"), never emit two
    // top-level documents.
    let env = Env::new();
    let path = repo(&env, "notes");
    let out = env.vard(&[
        "--format",
        "json",
        "watch",
        "add",
        path.to_str().unwrap(),
        "--name",
        "notes",
        "--sync",
    ]);
    let s = stdout(&out);
    let trimmed = s.trim();
    // Compact JSON is a single line; two documents would be two lines (or two
    // objects concatenated). Assert exactly one object carrying nested rows.
    assert_eq!(
        trimmed.lines().count(),
        1,
        "expected a single JSON document, got: {s}"
    );
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "not one object: {s}"
    );
    assert!(
        trimmed.contains(r#""sync":["#),
        "the confirmation rows must be nested under \"sync\": {s}"
    );
    assert!(
        trimmed.contains(r#""name":"notes""#),
        "the add fields must be present: {s}"
    );
    // Lightweight balance check (no JSON parser in the test deps).
    let opens = trimmed.matches('{').count() + trimmed.matches('[').count();
    let closes = trimmed.matches('}').count() + trimmed.matches(']').count();
    assert_eq!(opens, closes, "unbalanced JSON delimiters: {s}");
}

#[test]
fn add_sync_conflicts_with_no_sync() {
    let env = Env::new();
    let path = repo(&env, "notes");
    let out = env.vard(&[
        "watch",
        "add",
        path.to_str().unwrap(),
        "--name",
        "notes",
        "--sync",
        "--no-sync",
    ]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot be used with"),
        "clap conflict message expected: {}",
        stderr(&out)
    );
    // Nothing was written.
    assert!(env.config_text().is_empty());
}

#[test]
fn add_hint_present_on_plain_add_absent_with_sync_and_defaults() {
    // Records-form plain add prints exactly one opt-in hint; --sync suppresses
    // it (that add runs the confirmation cycle instead), and a defaults.sync =
    // true config suppresses it (syncing resolves on).
    let env = Env::new();
    let plain = repo(&env, "plain");
    let out = env.vard(&[
        "--format",
        "records",
        "watch",
        "add",
        plain.to_str().unwrap(),
        "--name",
        "plain",
    ]);
    let text = stdout(&out);
    assert!(
        text.contains("syncing is off — enable with: vard watch sync plain"),
        "plain add must print the opt-in hint: {text}"
    );

    // --sync: no hint (the cycle is the confirmation).
    let synced = repo(&env, "synced");
    let out = env.vard(&[
        "--format",
        "records",
        "watch",
        "add",
        synced.to_str().unwrap(),
        "--name",
        "synced",
        "--sync",
    ]);
    assert!(
        !stdout(&out).contains("syncing is off"),
        "an --sync add must not print the hint: {}",
        stdout(&out)
    );

    // JSON form: the hint is records-only.
    let jpath = repo(&env, "asjson");
    let out = env.vard(&[
        "--format",
        "json",
        "watch",
        "add",
        jpath.to_str().unwrap(),
        "--name",
        "asjson",
    ]);
    assert!(
        !stdout(&out).contains("syncing is off"),
        "the hint must not appear in the machine form: {}",
        stdout(&out)
    );
}

#[test]
fn relink_preserving_a_sync_pin_suppresses_the_off_hint() {
    // Re-adding an existing `sync = true` watch at a new path (a relink) with no
    // --sync preserves the pin; the opt-in hint must read the EFFECTIVE sync and
    // stay silent — not falsely claim "syncing is off".
    let env = Env::new();
    let first = repo(&env, "one");
    // Enable syncing on the watch. The repo has no remote, so the confirmation
    // cycle exits 1, but the write lands `sync = true` regardless.
    env.vard(&[
        "watch",
        "add",
        first.to_str().unwrap(),
        "--name",
        "w",
        "--sync",
    ]);
    assert!(
        env.config_text().contains("sync = true"),
        "setup: sync pin not written: {}",
        env.config_text()
    );

    // Relink to a new path, records form, no --sync.
    let second = repo(&env, "two");
    let out = env.vard(&[
        "--format",
        "records",
        "watch",
        "add",
        second.to_str().unwrap(),
        "--name",
        "w",
    ]);
    assert!(out.status.success(), "relink failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("relinked watch w"),
        "expected a relink: {}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("syncing is off"),
        "a relink preserving sync = true must not print the off hint: {}",
        stdout(&out)
    );
    // The preserved pin survived the relink.
    assert!(
        env.config_text().contains("sync = true"),
        "the relink must preserve the sync pin: {}",
        env.config_text()
    );
}

#[test]
fn add_hint_absent_when_defaults_sync_is_on() {
    let env = Env::new();
    write_config(&env, "version = 1\n\n[defaults]\nsync = true\n");
    let path = repo(&env, "notes");
    let out = env.vard(&[
        "--format",
        "records",
        "watch",
        "add",
        path.to_str().unwrap(),
        "--name",
        "notes",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        !stdout(&out).contains("syncing is off"),
        "defaults.sync = true resolves sync on, so no hint: {}",
        stdout(&out)
    );
}

// --- watch set -------------------------------------------------------------

#[test]
fn set_writes_a_key_preserving_comments() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    // Inject a hand-written comment ahead of the config.
    let cfg = env.config_path();
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("# keep this note\n{original}")).unwrap();

    let out = env.vard(&["watch", "set", "notes", "--interval", "30m"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = env.config_text();
    assert!(text.contains("interval = \"30m\""), "got: {text}");
    assert!(text.contains("# keep this note"), "comment lost: {text}");
}

#[test]
fn set_multiple_keys_in_one_invocation() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&[
        "watch",
        "set",
        "notes",
        "--trigger",
        "both",
        "--interval",
        "30m",
        "--sync-interval",
        "20m",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = env.config_text();
    assert!(text.contains("trigger = \"both\""), "got: {text}");
    assert!(text.contains("interval = \"30m\""), "got: {text}");
    assert!(text.contains("sync_interval = \"20m\""), "got: {text}");
}

#[test]
fn set_json_reports_the_applied_changes() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&[
        "--format", "json", "watch", "set", "notes", "--branch", "backup",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let json = stdout(&out);
    assert!(json.contains("\"name\":\"notes\""), "got: {json}");
    assert!(json.contains("\"branch\":\"backup\""), "got: {json}");
}

#[test]
fn set_zero_sync_interval_turns_the_pull_timer_off() {
    // `0s` is a valid sync_interval (pull timer off) — it must be accepted, not
    // rejected as a zero duration the way interval/quiesce are.
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "set", "notes", "--sync-interval", "0s"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        env.config_text().contains("sync_interval = \"0s\""),
        "got: {}",
        env.config_text()
    );
}

#[test]
fn set_a_bad_duration_is_refused_and_leaves_the_config_untouched() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    let before = env.config_text();

    let out = env.vard(&["watch", "set", "notes", "--interval", "5x"]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    assert_eq!(env.config_text(), before, "config must be untouched");
}

#[test]
fn set_unset_removes_a_key_so_it_re_inherits_the_default() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    env.vard(&[
        "watch",
        "set",
        "notes",
        "--interval",
        "30m",
        "--quiesce",
        "10s",
    ]);

    let out = env.vard(&["watch", "set", "notes", "--unset", "interval"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = env.config_text();
    assert!(
        !text.lines().any(|l| l.trim_start().starts_with("interval")),
        "interval must be gone: {text}"
    );
    assert!(text.contains("quiesce = \"10s\""), "sibling kept: {text}");
}

#[test]
fn set_unset_of_an_unset_key_is_an_error() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);
    let before = env.config_text();

    let out = env.vard(&["watch", "set", "notes", "--unset", "quiesce"]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("does not set quiesce"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(env.config_text(), before, "config must be untouched");
}

#[test]
fn set_and_unset_the_same_key_is_a_usage_error() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&[
        "watch",
        "set",
        "notes",
        "--trigger",
        "events",
        "--unset",
        "trigger",
    ]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("set and --unset"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn set_with_no_flags_is_a_usage_error() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "set", "notes"]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("nothing to set"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn set_on_an_unknown_watch_is_an_error() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "set", "ghost", "--trigger", "both"]);
    assert_eq!(code(&out), 2, "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("no watch named or rooted at"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn set_selects_by_path() {
    let env = Env::new();
    let path = repo(&env, "notes");
    env.vard(&["watch", "add", path.to_str().unwrap(), "--name", "notes"]);

    let out = env.vard(&["watch", "set", path.to_str().unwrap(), "--remote", "backup"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        env.config_text().contains("remote = \"backup\""),
        "got: {}",
        env.config_text()
    );
}

#[test]
fn add_sync_interval_writes_the_key() {
    let env = Env::new();
    let path = repo(&env, "notes");
    let out = env.vard(&[
        "watch",
        "add",
        path.to_str().unwrap(),
        "--name",
        "notes",
        "--sync-interval",
        "20m",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        env.config_text().contains("sync_interval = \"20m\""),
        "got: {}",
        env.config_text()
    );
}
