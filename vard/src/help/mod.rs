//! Custom help renderer per the CLI Help Output v2 spec.
//!
//! This module owns rendering for both `-h` and `--help`. clap is the argument
//! parser and the source of arg metadata; it never emits help text. The
//! [`intercept_from_args`] entry point runs before `Cli::parse()` so that a
//! subcommand with required positionals can still render help without clap
//! erroring on the missing positional.

pub mod bin_name;
pub mod examples;
pub mod extract;
pub mod model;
pub mod render;

pub use bin_name::BIN_NAME;
pub use extract::build_model;
pub use model::HelpForm;

use std::io::{self, IsTerminal, Write};

use clap::CommandFactory;

use crate::cli::{Cli, ColorWhen};
use crate::output::pager::{should_page, spawn_pager_or_passthrough};
use crate::output::palette::{self, Palette};

/// Called from `main()` BEFORE `Cli::parse()`. Scans `std::env::args()` for
/// `-h` / `--help`, resolves the subcommand path from the raw args, and renders
/// help. Returns `Some(exit_code)` when help was rendered; `None` otherwise.
///
/// The pre-parse approach is necessary because required positionals on a
/// subcommand would cause `Cli::parse()` to error out before help could be
/// intercepted. Scanning raw args first lets help render regardless.
pub fn intercept_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();

    // Determine form: long (--help) takes priority over short (-h).
    let form = if args.iter().any(|a| a == "--help") {
        HelpForm::Long
    } else if args.iter().any(|a| a == "-h") {
        HelpForm::Short
    } else {
        return None;
    };

    let color = parse_color_from_args(&args);
    let root = Cli::command();
    let (subcmd, cmd_path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
    if hit_unknown {
        // An unknown token appeared before the help flag. Let `Cli::parse()`
        // run so clap can report the "unrecognized subcommand" error.
        return None;
    }
    let model = build_model(subcmd, &root, &cmd_path, form);
    Some(emit(form, &model, color))
}

/// Render short help for the root command (`vard` with no args and no help
/// flag). Returns the process exit code.
pub fn print_root_short() -> i32 {
    let root = Cli::command();
    let model = build_model(&root, &root, BIN_NAME, HelpForm::Short);
    emit(HelpForm::Short, &model, ColorWhen::Auto)
}

/// Render `model` in `form`, then write it to stdout — paging the long form on
/// a TTY when it overflows the screen. Returns the process exit code.
fn emit(form: HelpForm, model: &model::HelpModel, color: ColorWhen) -> i32 {
    let palette: Palette = palette::resolve(color);
    let term_width = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80);

    let mut buf: Vec<u8> = Vec::new();
    let render_result = match form {
        HelpForm::Short => render::render_short(&mut buf, model, &palette, term_width),
        HelpForm::Long => render::render_long(&mut buf, model, &palette, term_width),
    };
    if let Err(err) = render_result {
        eprintln!("{BIN_NAME}: help render failed: {err}");
        return 1;
    }

    let stdout = io::stdout();
    let is_tty = stdout.is_terminal();
    let result = match form {
        HelpForm::Long => {
            let buffer_lines = buf.iter().filter(|b| **b == b'\n').count();
            if should_page(buffer_lines, /* no_pager */ false, is_tty) {
                let mut stderr = io::stderr();
                let mut out = stdout.lock();
                spawn_pager_or_passthrough(&buf, &mut out, &mut stderr, "vard --help")
            } else {
                io::stdout().write_all(&buf)
            }
        }
        HelpForm::Short => io::stdout().write_all(&buf),
    };
    if let Err(err) = result
        && err.kind() != io::ErrorKind::BrokenPipe
    {
        eprintln!("{BIN_NAME}: writing help failed: {err}");
        return 1;
    }
    0
}

/// Walk the raw args to find the deepest recognised subcommand chain, then
/// return the matching `clap::Command`, the user-facing path string, and a flag
/// indicating whether an unknown non-flag token was encountered.
///
/// `hit_unknown = true` means the args contain something like `vard graph
/// --help` where `graph` is not a known subcommand; the caller then passes
/// through to clap for proper error reporting.
///
/// Strategy: skip the binary name (`args[0]`) and any flag-like tokens, walking
/// non-flag tokens as subcommand names into the clap tree as long as each token
/// is a valid subcommand. Stop at the first token that is not a known
/// subcommand.
fn resolve_subcmd_from_raw_args<'a>(
    root: &'a clap::Command,
    args: &[String],
) -> (&'a clap::Command, String, bool) {
    let mut current = root;
    let mut path = BIN_NAME.to_string();

    let mut iter = args.iter().skip(1);
    while let Some(token) = iter.next() {
        // Skip flags and their inline values.
        if token.starts_with('-') {
            if !token.contains('=') {
                // Value-taking global flags: skip their next token so it is not
                // mistaken for a subcommand.
                let flag_stem = token.trim_start_matches('-');
                if matches!(flag_stem, "color" | "format") {
                    let _ = iter.next();
                }
            }
            continue;
        }
        // Try to descend into this token as a subcommand name.
        if let Some(child) = current
            .get_subcommands()
            .find(|c| c.get_name() == token.as_str())
        {
            path = format!("{path} {token}");
            current = child;
        } else {
            // Not a known subcommand: a positional value (valid) or an unknown
            // subcommand name (error). Flag it as unknown only when this level
            // still accepts subcommands, so clap can report it.
            let expecting_subcommand = current.has_subcommands();
            return (current, path, expecting_subcommand);
        }
    }

    (current, path, false)
}

/// Parse `--color <VALUE>` (or `--color=VALUE`) from raw args, defaulting to
/// `ColorWhen::Auto`.
fn parse_color_from_args(args: &[String]) -> ColorWhen {
    let mut iter = args.iter();
    while let Some(token) = iter.next() {
        if token == "--color" {
            if let Some(val) = iter.next() {
                return color_from_str(val);
            }
        } else if let Some(val) = token.strip_prefix("--color=") {
            return color_from_str(val);
        }
    }
    ColorWhen::Auto
}

fn color_from_str(val: &str) -> ColorWhen {
    match val {
        "always" => ColorWhen::Always,
        "never" => ColorWhen::Never,
        _ => ColorWhen::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_defaults_to_auto() {
        let args = vec!["vard".to_string(), "run".to_string()];
        assert_eq!(parse_color_from_args(&args), ColorWhen::Auto);
    }

    #[test]
    fn parse_color_space_form() {
        let args = vec![
            "vard".to_string(),
            "--color".to_string(),
            "never".to_string(),
        ];
        assert_eq!(parse_color_from_args(&args), ColorWhen::Never);
    }

    #[test]
    fn parse_color_equals_form() {
        let args = vec!["vard".to_string(), "--color=always".to_string()];
        assert_eq!(parse_color_from_args(&args), ColorWhen::Always);
    }

    #[test]
    fn resolve_root_when_no_subcommand() {
        let root = Cli::command();
        let args = vec!["vard".to_string(), "--help".to_string()];
        let (_, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard");
        assert!(!hit_unknown);
    }

    #[test]
    fn resolve_descends_into_run() {
        let root = Cli::command();
        let args = vec!["vard".to_string(), "run".to_string(), "--help".to_string()];
        let (cmd, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard run");
        assert_eq!(cmd.get_name(), "run");
        assert!(!hit_unknown);
    }

    #[test]
    fn resolve_flags_unknown_subcommand() {
        let root = Cli::command();
        let args = vec![
            "vard".to_string(),
            "bogus".to_string(),
            "--help".to_string(),
        ];
        let (_, _, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert!(hit_unknown, "unknown subcommand must be flagged for clap");
    }

    #[test]
    fn resolve_skips_color_value_before_subcommand() {
        let root = Cli::command();
        let args = vec![
            "vard".to_string(),
            "--color".to_string(),
            "never".to_string(),
            "run".to_string(),
        ];
        let (cmd, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard run");
        assert_eq!(cmd.get_name(), "run");
        assert!(!hit_unknown);
    }
}
