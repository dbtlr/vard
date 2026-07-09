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
    if cmd_path == BIN_NAME {
        vec![(
            run,
            "watch every configured directory and snapshot on change".to_string(),
        )]
    } else if cmd_path == run.as_str() {
        vec![(
            run,
            "run in the foreground until SIGINT or SIGTERM".to_string(),
        )]
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
tree carries its own timeline without manual commits.\n\nThe daemon is the \
whole of {BIN_NAME} today: `{BIN_NAME} run` holds a single-instance lock on its state \
directory, resolves the config into per-directory watch specs, and supervises \
the watch-and-snapshot engine until it is stopped. Query and control \
subcommands land in later releases."
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
