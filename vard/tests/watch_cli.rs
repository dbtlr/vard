//! End-to-end tests for `vard watch add/remove/list/pause/resume`, driving the
//! real binary against tempdir-isolated config, state, and HOME — nothing here
//! touches the developer's real environment. Repositories are created with a
//! throwaway global git config so commits and inits are deterministic in CI.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::{Env, stderr, stdout};

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
