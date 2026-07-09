//! Hand-authored `--help` examples and conceptual prose, keyed by command path.
//!
//! Examples are biased toward fewer — empty is the correct answer for many
//! commands, and the renderer skips the `EXAMPLES` / conceptual blocks when a
//! table is empty. Each example is `(command_line, comment)`; comments are
//! short, lowercase except for required literals, no trailing period. The
//! command line uses the literal `vard` prefix; the renderer styles tokens.

/// Return canned examples for the given command path (e.g. `"vard run"`).
///
/// Returns `vec![]` for unknown paths and for paths intentionally without
/// examples.
pub fn examples_for(cmd_path: &str) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match cmd_path {
        "vard" => &[(
            "vard run",
            "watch every configured directory and snapshot on change",
        )],
        "vard run" => &[("vard run", "run in the foreground until SIGINT or SIGTERM")],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(cmd, comment)| (cmd.to_string(), comment.to_string()))
        .collect()
}

/// Return conceptual prose sections for the given command path.
///
/// Each entry is `(heading, body)`. Headings render in `dim` bold uppercase;
/// bodies are markdown-light paragraphs separated by blank lines. Returns
/// `vec![]` for command paths with no sections.
pub fn conceptual_sections_for(cmd_path: &str) -> Vec<(String, String)> {
    let pairs: &[(&str, &str)] = match cmd_path {
        "vard" => &[(
            "How vard works",
            "vard watches the directories named in its config file and commits a \
snapshot into version control whenever their contents change, so a working \
tree carries its own timeline without manual commits.\n\nThe daemon is the \
whole of vard today: `vard run` holds a single-instance lock on its state \
directory, resolves the config into per-directory watch specs, and supervises \
the watch-and-snapshot engine until it is stopped. Query and control \
subcommands land in later releases.",
        )],
        "vard run" => &[(
            "Lifecycle and signals",
            "On startup the daemon acquires the single-instance lock for its \
state directory; a second daemon contending for the same directory exits with \
status 2. It then loads the config into watch specs — a missing or watch-less \
config is a startup error — recovers any stale version-control index locks \
left by a previous crash, and starts the engine.\n\nWhile running it stays \
attached to the terminal and logs each event to stderr. It reloads on SIGHUP \
or when the config file changes on disk, rebuilds a watch whose event source \
dies (with exponential backoff), and shuts down cleanly on SIGINT or SIGTERM.",
        )],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(heading, body)| (heading.to_string(), body.to_string()))
        .collect()
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
    fn run_has_lifecycle_section() {
        let sections = conceptual_sections_for("vard run");
        let (_, body) = sections
            .iter()
            .find(|(h, _)| h == "Lifecycle and signals")
            .expect("lifecycle section present");
        assert!(body.contains("SIGHUP"));
        assert!(body.contains("status 2"));
    }
}
