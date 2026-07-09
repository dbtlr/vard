mod cli;
mod config;
mod daemon;
mod instance;
mod journal;
mod paths;

use std::process::ExitCode;

use clap::{CommandFactory, Parser};

use cli::{Cli, Command};

/// Parse the CLI and dispatch. `vard run` starts the daemon; a bare `vard` (or
/// any not-yet-implemented invocation) prints help. clap handles `--help` and
/// `--version` during parsing.
fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Run) => daemon::run(),
        None => print_help(),
    }
}

/// Prints the long help and returns a success exit code, preserving the
/// pre-daemon default behavior for a bare `vard`.
fn print_help() -> ExitCode {
    if let Err(err) = Cli::command().print_long_help() {
        eprintln!("vard: {err}");
        return ExitCode::from(2);
    }
    println!();
    ExitCode::SUCCESS
}
