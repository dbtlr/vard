mod cli;

// `config` and `paths` are the file-config layer and its path resolution. Both
// are fully built and tested, but main does not wire them into runtime behavior
// yet — the CLI subcommands that consume them land in VRD-14/15/17. Until then
// they are unreachable from `main`, so a module-level allow keeps the strict
// dead-code lint quiet without scattering per-item allows. Remove each allow
// when its consuming command lands.
#[allow(dead_code)]
mod config;
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
