//! Generates shell completion scripts and the roff manpage as a side effect of
//! building `vard`. The outputs land under the workspace `target/` directory so
//! cargo-dist's `include` directive (in `dist-workspace.toml`) can bundle them
//! without a separate generation step in the release pipeline.
//!
//! The CLI surface is reused via `#[path = "src/cli.rs"]`, so this script
//! tracks the real `clap` definitions automatically. `cli.rs` is kept free of
//! intra-crate dependencies (see its module docs) to make the include trick
//! viable.

use std::env;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap_complete::{Shell, generate_to};
use clap_complete_nushell::Nushell;
use clap_mangen::Man;

#[path = "src/cli.rs"]
#[allow(dead_code)]
mod cli;

fn main() -> std::io::Result<()> {
    // CARGO_MANIFEST_DIR is the `vard` package dir (`<workspace>/vard`); its
    // parent is the workspace root.
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by cargo when running build.rs"),
    );
    let workspace_root = manifest_dir
        .parent()
        .expect("vard package directory must have a workspace-root parent")
        .to_path_buf();

    // Honor CARGO_TARGET_DIR (absolute, or relative to the workspace root per
    // cargo's contract); default to the workspace `target/`. dist builds use
    // the default target dir, which is what dist-workspace.toml's `include`
    // paths point at — those stay anchored at `target/` deliberately.
    let target_dir = match env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                workspace_root.join(dir)
            }
        }
        None => workspace_root.join("target"),
    };

    let completions_dir = target_dir.join("completions");
    let man_dir = target_dir.join("man");

    std::fs::create_dir_all(&completions_dir)?;
    std::fs::create_dir_all(&man_dir)?;

    let mut cmd = cli::Cli::command();
    generate_to(Shell::Bash, &mut cmd, "vard", &completions_dir)?;
    generate_to(Shell::Zsh, &mut cmd, "vard", &completions_dir)?;
    generate_to(Shell::Fish, &mut cmd, "vard", &completions_dir)?;
    generate_to(Nushell, &mut cmd, "vard", &completions_dir)?;

    // clap_mangen renders one page per command, so walk the whole tree: the
    // top-level page plus one for every (nested) subcommand — `vard.1`,
    // `vard-run.1`, and any future `vard-<parent>-<child>.1`.
    write_man_pages(&cmd, "vard", &man_dir)?;

    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");
    // The manpage and completions embed CARGO_PKG_VERSION / DESCRIPTION, which
    // come from the manifests — a version bump must regenerate the artifacts.
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../Cargo.toml");
    Ok(())
}

/// Render the roff manpage for `cmd` as `<full_name>.1`, then recurse into every
/// visible subcommand as `<full_name>-<child>.1`. `full_name` is the dashed
/// command path (`vard`, `vard-run`, …); the command is renamed to it so the
/// page title matches the file name.
fn write_man_pages(cmd: &clap::Command, full_name: &str, man_dir: &Path) -> std::io::Result<()> {
    // clap's `Command::name` wants a `'static` string; in a short-lived build
    // script, leaking the dashed name is the simplest way to satisfy that.
    let leaked: &'static str = Box::leak(full_name.to_owned().into_boxed_str());
    let named = cmd.clone().name(leaked);
    let mut buffer = Vec::new();
    Man::new(named).render(&mut buffer)?;
    std::fs::write(man_dir.join(format!("{full_name}.1")), buffer)?;

    for sub in cmd.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let child_name = format!("{full_name}-{}", sub.get_name());
        write_man_pages(sub, &child_name, man_dir)?;
    }
    Ok(())
}
