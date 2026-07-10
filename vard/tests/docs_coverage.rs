//! Guard: the command reference under `docs/` stays structurally in sync with
//! the real clap command tree.
//!
//! # Mapping rule
//!
//! One documentation page per **top-level** `vard` command, mirroring the
//! granularity of the reference docs: each visible child of the root command
//! (`run`, `watch`, `snapshot`, …) has exactly one `docs/commands/<name>.md`
//! page, and grouped commands (`watch`, `config`) document their subcommands
//! *within* that single page rather than getting a page per leaf. Nested
//! subcommands therefore do not map to their own files. Hidden commands
//! (`is_hide_set`) are excluded, as they are from `--help` and the manpages.
//!
//! This test asserts the structure only — that pages exist, that names match,
//! and that the index links every page. The accuracy of each page's prose is
//! the author's responsibility (see the docs/help sync contract in the repo
//! `CLAUDE.md`).

#[path = "../src/cli.rs"]
#[allow(dead_code)]
mod cli;

use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::CommandFactory;

/// The workspace-root `docs/` directory. `CARGO_MANIFEST_DIR` is the `vard`
/// package dir; its parent is the workspace root.
fn docs_dir() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vard package dir has a workspace-root parent")
        .join("docs")
}

/// The visible top-level command names — the leaves the mapping rule covers.
fn top_level_commands() -> BTreeSet<String> {
    cli::Cli::command()
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| sub.get_name().to_string())
        .collect()
}

/// The basenames (without `.md`) of every page under `docs/commands/`.
fn doc_pages(commands_dir: &std::path::Path) -> BTreeSet<String> {
    std::fs::read_dir(commands_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", commands_dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
        .map(|p| {
            p.file_stem()
                .expect("md file has a stem")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[test]
fn every_command_has_a_docs_page() {
    let commands_dir = docs_dir().join("commands");
    let pages = doc_pages(&commands_dir);
    for command in top_level_commands() {
        assert!(
            pages.contains(&command),
            "no docs page for `vard {command}`; add \
             docs/commands/{command}.md (one page per top-level command)"
        );
    }
}

#[test]
fn every_docs_page_has_a_command() {
    let commands_dir = docs_dir().join("commands");
    let commands = top_level_commands();
    for page in doc_pages(&commands_dir) {
        assert!(
            commands.contains(&page),
            "docs/commands/{page}.md has no matching `vard` command; \
             remove it or rename it to a real top-level command"
        );
    }
}

#[test]
fn index_links_every_page() {
    let index_path = docs_dir().join("commands.md");
    let index = std::fs::read_to_string(&index_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", index_path.display()));
    for command in top_level_commands() {
        // Match the markdown link target form `](commands/<name>.md)`, not a
        // bare substring — a prose mention or comment must not satisfy the gate.
        let link = format!("](commands/{command}.md)");
        assert!(
            index.contains(&link),
            "docs/commands.md (the index) has no markdown link to \
             commands/{command}.md; add a row for `vard {command}`"
        );
    }
}
