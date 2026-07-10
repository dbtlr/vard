mod cli;
mod config;
mod config_edit;
mod daemon;
mod help;
mod instance;
mod journal;
mod output;
mod paths;
mod watch;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};

/// Parse the CLI and dispatch. Help is intercepted before parsing so `-h` /
/// `--help` render through the custom CLI Help Output v2 path (never clap).
/// `vard run` starts the daemon; a bare `vard` prints short help.
fn main() -> ExitCode {
    // Render `-h` / `--help` before clap parses (see `help::intercept_from_args`).
    if let Some(code) = help::intercept_from_args() {
        return ExitCode::from(code as u8);
    }

    let cli = Cli::parse();

    // Fallback: if a help flag survived interception (it normally does not),
    // render help rather than silently starting the daemon.
    if let Some(code) = help::render_parsed_help(&cli) {
        return ExitCode::from(code as u8);
    }

    match cli.command {
        Some(Command::Run) => daemon::run(),
        // A bare `vard watch` (no subcommand) prints watch's short help, like a
        // bare `vard`.
        Some(Command::Watch { command: None }) => {
            ExitCode::from(help::print_command_short(&["watch"], cli.color) as u8)
        }
        Some(Command::Watch {
            command: Some(watch_cmd),
        }) => watch::run(watch_cmd, cli.color, cli.format),
        None => ExitCode::from(help::print_root_short(cli.color) as u8),
    }
}
