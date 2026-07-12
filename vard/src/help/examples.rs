//! Hand-authored `--help` examples and conceptual prose, keyed by command path.
//!
//! Examples are biased toward fewer — empty is the correct answer for many
//! commands, and the renderer skips the `EXAMPLES` / conceptual blocks when a
//! table is empty. Each example is `(command_line, comment)`; comments are
//! short, lowercase except for required literals, no trailing period. The
//! command line uses the `BIN_NAME` prefix; the renderer styles tokens.

use super::bin_name::BIN_NAME;

/// Return canned examples for the given command path (e.g. `"vard run"`).
///
/// Keys and example command strings are composed from [`BIN_NAME`] so a binary
/// rename keeps the examples attached to the right command paths.
///
/// Returns `vec![]` for unknown paths and for paths intentionally without
/// examples.
pub fn examples_for(cmd_path: &str) -> Vec<(String, String)> {
    let run = format!("{BIN_NAME} run");
    let watch = format!("{BIN_NAME} watch");
    if cmd_path == BIN_NAME {
        vec![
            (
                run.clone(),
                "watch every configured directory and snapshot on change".to_string(),
            ),
            (
                format!("{watch} add ~/notes"),
                "start watching a directory".to_string(),
            ),
        ]
    } else if cmd_path == run.as_str() {
        vec![(
            run,
            "run in the foreground until SIGINT or SIGTERM".to_string(),
        )]
    } else if cmd_path == watch.as_str() {
        vec![
            (
                format!("{watch} add ~/notes"),
                "register ~/notes (offering git init if needed)".to_string(),
            ),
            (format!("{watch} list"), "show every watch".to_string()),
        ]
    } else if cmd_path == format!("{watch} add") {
        vec![
            (
                format!("{watch} add ~/notes"),
                "watch ~/notes, naming it after the directory".to_string(),
            ),
            (
                format!("{watch} add ~/site --name blog --no-sync"),
                "watch locally under a custom name, never pushing".to_string(),
            ),
            (
                format!("{watch} add /srv/data --init --branch backup"),
                "init a repo on branch backup, non-interactively".to_string(),
            ),
        ]
    } else if cmd_path == format!("{watch} remove") {
        vec![
            (
                format!("{watch} remove notes"),
                "unregister the watch named notes".to_string(),
            ),
            (
                format!("{watch} remove ~/notes --purge"),
                "unregister by path and drop its metadata".to_string(),
            ),
        ]
    } else if cmd_path == format!("{watch} list") {
        vec![(
            format!("{watch} list --format json"),
            "emit the watch list as JSON for a script".to_string(),
        )]
    } else if cmd_path == format!("{watch} pause") {
        vec![(
            format!("{watch} pause notes"),
            "stop snapshotting notes until resumed".to_string(),
        )]
    } else if cmd_path == format!("{watch} resume") {
        vec![(
            format!("{watch} resume notes"),
            "resume a paused watch".to_string(),
        )]
    } else if cmd_path == format!("{BIN_NAME} snapshot") {
        vec![
            (
                format!("{BIN_NAME} snapshot"),
                "snapshot every configured watch now".to_string(),
            ),
            (
                format!("{BIN_NAME} snapshot notes -m \"before the demo\""),
                "snapshot notes with a message on the subject".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} sync") {
        vec![
            (
                format!("{BIN_NAME} sync"),
                "sync every sync-enabled watch with its remote now".to_string(),
            ),
            (
                format!("{BIN_NAME} sync notes"),
                "fetch, reconcile, and push just the notes watch".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} log") {
        vec![
            (
                format!("{BIN_NAME} log notes"),
                "show the full snapshot history of notes".to_string(),
            ),
            (
                format!("{BIN_NAME} log notes --since 2h"),
                "show only snapshots from the last two hours".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} diff") {
        vec![
            (
                format!("{BIN_NAME} diff notes"),
                "show uncommitted changes against the last snapshot".to_string(),
            ),
            (
                format!("{BIN_NAME} diff notes HEAD~5"),
                "show everything that changed since five snapshots ago".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} notify") {
        vec![
            (
                format!("{BIN_NAME} notify"),
                "print a line per troubled watch, silent when healthy".to_string(),
            ),
            (
                format!("{BIN_NAME} notify --format json"),
                "emit problems as a JSON array for a status bar".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} status") {
        vec![
            (
                format!("{BIN_NAME} status"),
                "show the daemon and every watch's state".to_string(),
            ),
            (
                format!("{BIN_NAME} status notes"),
                "show just the notes watch".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} config") {
        vec![
            (
                format!("{BIN_NAME} config get defaults.interval"),
                "print the default snapshot interval".to_string(),
            ),
            (
                format!("{BIN_NAME} config set daemon.log_level debug"),
                "raise the daemon's log verbosity".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} config get") {
        vec![(
            format!("{BIN_NAME} config get daemon.log_level"),
            "print a key's value (exit 1 if unset)".to_string(),
        )]
    } else if cmd_path == format!("{BIN_NAME} config set") {
        vec![
            (
                format!("{BIN_NAME} config set defaults.interval 30m"),
                "snapshot every 30 minutes by default".to_string(),
            ),
            (
                format!("{BIN_NAME} config set defaults.sync true"),
                "sync watches to their remotes by default".to_string(),
            ),
        ]
    } else if cmd_path == format!("{BIN_NAME} config unset") {
        vec![(
            format!("{BIN_NAME} config unset defaults.interval"),
            "restore the built-in default interval".to_string(),
        )]
    } else if cmd_path == format!("{BIN_NAME} config edit") {
        vec![(
            format!("{BIN_NAME} config edit"),
            "open the config in $EDITOR and validate on save".to_string(),
        )]
    } else if cmd_path == format!("{BIN_NAME} config path") {
        vec![(
            format!("{BIN_NAME} config path"),
            "print the config file's location".to_string(),
        )]
    } else if cmd_path == format!("{BIN_NAME} restore") {
        vec![
            (
                format!("{BIN_NAME} restore notes --at 3d --dry-run"),
                "preview restoring to three days ago".to_string(),
            ),
            (
                format!("{BIN_NAME} restore notes --ref a1b2c3d --file todo.md"),
                "restore one file from a specific snapshot".to_string(),
            ),
        ]
    } else {
        vec![]
    }
}

/// Return conceptual prose sections for the given command path.
///
/// Each entry is `(heading, body)`. Headings render in `dim` bold uppercase;
/// bodies are markdown-light paragraphs separated by blank lines. Returns
/// `vec![]` for command paths with no sections.
pub fn conceptual_sections_for(cmd_path: &str) -> Vec<(String, String)> {
    // Only the root carries a conceptual section. `vard run`'s lifecycle prose
    // is authoritative in clap's `long_about` (cli.rs), so it is not duplicated
    // here — the long_about is the single source for that material.
    if cmd_path == BIN_NAME {
        vec![(
            format!("How {BIN_NAME} works"),
            format!(
                "{BIN_NAME} watches the directories named in its config file and commits a \
snapshot into version control whenever their contents change, so a working \
tree carries its own timeline without manual commits.\n\nManage the watch set \
with `{BIN_NAME} watch add`, `remove`, `list`, `pause`, and `resume`; each edits the \
config file in place. `{BIN_NAME} run` then holds a single-instance lock on its state \
directory, resolves the config into per-directory watch specs, and supervises \
the watch-and-snapshot engine until it is stopped."
            ),
        )]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_path_returns_empty() {
        assert!(examples_for("vard nonexistent").is_empty());
    }

    #[test]
    fn root_path_has_examples() {
        assert!(!examples_for("vard").is_empty());
    }

    #[test]
    fn run_path_has_examples() {
        assert!(!examples_for("vard run").is_empty());
    }

    #[test]
    fn watch_subcommands_have_examples() {
        for path in [
            "vard watch",
            "vard watch add",
            "vard watch remove",
            "vard watch list",
            "vard watch pause",
            "vard watch resume",
        ] {
            assert!(
                !examples_for(path).is_empty(),
                "missing examples for {path}"
            );
        }
    }

    #[test]
    fn examples_are_keyed_off_bin_name() {
        // Every example command line starts with the configured BIN_NAME, so a
        // rename keeps them attached to the right paths.
        for (cmd, _) in examples_for("vard watch add") {
            assert!(
                cmd.starts_with(BIN_NAME),
                "example not BIN_NAME-prefixed: {cmd}"
            );
        }
    }

    #[test]
    fn notify_path_has_examples() {
        assert!(!examples_for("vard notify").is_empty());
    }

    #[test]
    fn status_and_config_paths_have_examples() {
        for path in [
            "vard status",
            "vard config",
            "vard config get",
            "vard config set",
            "vard config unset",
            "vard config edit",
            "vard config path",
        ] {
            assert!(
                !examples_for(path).is_empty(),
                "missing examples for {path}"
            );
        }
    }

    #[test]
    fn conceptual_sections_for_unknown_path_returns_empty() {
        assert!(conceptual_sections_for("vard nonexistent").is_empty());
    }

    #[test]
    fn root_has_how_vard_works_section() {
        let sections = conceptual_sections_for("vard");
        assert!(sections.iter().any(|(h, _)| h == "How vard works"));
    }

    #[test]
    fn run_has_no_conceptual_section() {
        // Lifecycle prose now lives authoritatively in clap's long_about, so
        // `vard run` carries no conceptual section here.
        assert!(conceptual_sections_for("vard run").is_empty());
    }
}
