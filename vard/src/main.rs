mod cli;

// `paths` is validated XDG base-directory scaffolding that the CLI subcommands
// landing in VRD-15+ consume. It is not wired to a command yet, so it stays
// dead-code-allowed rather than being deleted and re-added.
#[allow(dead_code)]
mod paths;

use clap::{CommandFactory, Parser};

use cli::Cli;

/// Parse the CLI, then — with no subcommands defined yet (VRD-15+) — fall back
/// to printing help. clap handles `--help` and `--version` during parsing.
fn main() {
    let Cli {} = Cli::parse();
    if let Err(err) = Cli::command().print_long_help() {
        eprintln!("vard: {err}");
        std::process::exit(2);
    }
    println!();
}
