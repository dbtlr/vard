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
use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::{Shell, generate_to};
use clap_complete_nushell::Nushell;
use clap_mangen::Man;

#[path = "src/cli.rs"]
#[allow(dead_code)]
mod cli;

fn main() -> std::io::Result<()> {
    // CARGO_MANIFEST_DIR is the `vard` package dir (`<workspace>/vard`); its
    // parent is the workspace root, where the shared `target/` lives and where
    // dist-workspace.toml's `include` paths are anchored.
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by cargo when running build.rs"),
    );
    let workspace_root = manifest_dir
        .parent()
        .expect("vard package directory must have a workspace-root parent")
        .to_path_buf();

    let completions_dir = workspace_root.join("target").join("completions");
    let man_dir = workspace_root.join("target").join("man");

    std::fs::create_dir_all(&completions_dir)?;
    std::fs::create_dir_all(&man_dir)?;

    let mut cmd = cli::Cli::command();
    generate_to(Shell::Bash, &mut cmd, "vard", &completions_dir)?;
    generate_to(Shell::Zsh, &mut cmd, "vard", &completions_dir)?;
    generate_to(Shell::Fish, &mut cmd, "vard", &completions_dir)?;
    generate_to(Nushell, &mut cmd, "vard", &completions_dir)?;

    let man = Man::new(cmd);
    let mut buffer = Vec::new();
    man.render(&mut buffer)?;
    std::fs::write(man_dir.join("vard.1"), buffer)?;

    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
