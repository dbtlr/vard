//! Guard: every manpage `build.rs` generates must be listed in the workspace
//! `dist-workspace.toml` include list, so the release archive ships them.
//!
//! `build.rs` emits one roff page per (nested) subcommand — `vard.1`,
//! `vard-run.1`, and any future `vard-<parent>-<child>.1`. cargo-dist's
//! `include` is an explicit file list, so adding a subcommand without adding
//! its page here would silently drop the page from releases. This test walks
//! the real clap `Command` tree and fails when a page is unlisted.

#[path = "../src/cli.rs"]
#[allow(dead_code)]
mod cli;

use clap::CommandFactory;

/// Collect the dashed page basenames (`vard.1`, `vard-run.1`, …) for `cmd` and
/// every visible subcommand, mirroring `build.rs::write_man_pages`.
fn collect_page_names(cmd: &clap::Command, full_name: &str, out: &mut Vec<String>) {
    out.push(format!("{full_name}.1"));
    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        collect_page_names(sub, &format!("{full_name}-{}", sub.get_name()), out);
    }
}

#[test]
fn dist_include_lists_every_generated_manpage() {
    // CARGO_MANIFEST_DIR is the `vard` package dir; its parent is the workspace
    // root, where dist-workspace.toml lives.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dist_path = manifest_dir
        .parent()
        .expect("vard package dir has a workspace-root parent")
        .join("dist-workspace.toml");
    let dist = std::fs::read_to_string(&dist_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", dist_path.display()));

    let cmd = cli::Cli::command();
    let root_name = cmd.get_name().to_string();
    let mut pages = Vec::new();
    collect_page_names(&cmd, &root_name, &mut pages);

    for page in &pages {
        let needle = format!("target/man/{page}");
        assert!(
            dist.contains(&needle),
            "dist-workspace.toml `include` is missing {needle}; add it when \
             introducing a subcommand so the release ships its manpage"
        );
    }
}
