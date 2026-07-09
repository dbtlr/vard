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

use crate::cli::{Cli, ColorWhen, Command};
use crate::output::pager::{should_page, spawn_pager_or_passthrough};
use crate::output::palette::{self, Palette};

/// Called from `main()` BEFORE `Cli::parse()`. Scans `std::env::args()` for the
/// `help` subcommand and `-h` / `--help`, resolves the subcommand path from the
/// raw args, and renders help. Returns `Some(exit_code)` when help was rendered
/// (or a clean error emitted); `None` when control should pass to `Cli::parse()`.
///
/// The pre-parse approach is necessary because required positionals on a
/// subcommand would cause `Cli::parse()` to error out before help could be
/// intercepted. Scanning raw args first lets help render regardless.
pub fn intercept_from_args() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    let root = Cli::command();

    // A hand-rolled `--color` value that clap would reject must surface as
    // clap's invalid-value error, not silently render help with a default.
    let color = match parse_color_from_args(&root, &args) {
        Ok(c) => c,
        Err(()) => return None,
    };

    // `vard help [topic]` — we disabled clap's help subcommand, so mimic it.
    if let Some(code) = handle_help_subcommand(&root, &args, color) {
        return Some(code);
    }

    // Determine form: long (--help) takes priority over short (-h). Stop at a
    // `--` token — a help flag after it is a positional operand, not a request.
    let form = detect_help_form(&args)?;

    let (subcmd, cmd_path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
    if hit_unknown {
        // An unrecognized token appeared before the help flag (unknown
        // subcommand, or a value the command cannot accept). Let `Cli::parse()`
        // run so clap reports its real error.
        return None;
    }
    let model = build_model(subcmd, &root, &cmd_path, form);
    Some(emit(form, &model, color))
}

/// Scan the args (respecting a `--` terminator) for `-h`/`--help`. Long wins
/// over short regardless of order. `None` means no help flag is present.
fn detect_help_form(args: &[String]) -> Option<HelpForm> {
    let mut has_long = false;
    let mut has_short = false;
    for a in args.iter().skip(1) {
        if a == "--" {
            break;
        }
        if a == "--help" {
            has_long = true;
        } else if a == "-h" {
            has_short = true;
        }
    }
    if has_long {
        Some(HelpForm::Long)
    } else if has_short {
        Some(HelpForm::Short)
    } else {
        None
    }
}

/// Handle `vard help [topic...]` (disabled at the clap level via
/// `disable_help_subcommand`). Returns `Some(0)` after rendering long help for
/// the resolved topic, `Some(2)` after reporting an unknown topic, or `None`
/// when the args are not a `help` invocation.
fn handle_help_subcommand(root: &clap::Command, args: &[String], color: ColorWhen) -> Option<i32> {
    // Find the first operand (skipping flags and their values, stopping at `--`).
    let mut iter = args.iter().skip(1);
    let first = loop {
        match iter.next() {
            None => return None,
            Some(t) if t == "--" => return None,
            Some(t) if is_flag(t) => {
                if !t.contains('=') && flag_takes_value(root, root, t) {
                    iter.next();
                }
            }
            Some(t) => break t,
        }
    };
    if first != "help" {
        return None;
    }

    // Remaining operands form the topic path (e.g. `help run`).
    let mut current = root;
    let mut path = BIN_NAME.to_string();
    for t in iter {
        if t == "--" || is_flag(t) {
            continue;
        }
        match current
            .get_subcommands()
            .find(|c| c.get_name() == t.as_str())
        {
            Some(child) => {
                path = format!("{path} {t}");
                current = child;
            }
            None => {
                eprintln!("{BIN_NAME}: unrecognized help topic '{t}'");
                return Some(2);
            }
        }
    }
    let model = build_model(current, root, &path, HelpForm::Long);
    Some(emit(HelpForm::Long, &model, color))
}

/// Render short help for the root command (`vard` with no args and no help
/// flag). Returns the process exit code. `color` is the parsed `--color` value.
pub fn print_root_short(color: ColorWhen) -> i32 {
    let root = Cli::command();
    let model = build_model(&root, &root, BIN_NAME, HelpForm::Short);
    emit(HelpForm::Short, &model, color)
}

/// Fallback after a successful `Cli::parse()`: if the help flags survived
/// interception (they normally do not), render help rather than silently
/// starting the daemon. Returns `Some(exit_code)` when help was rendered.
pub fn render_parsed_help(cli: &Cli) -> Option<i32> {
    let form = if cli.help_long {
        HelpForm::Long
    } else if cli.help_short {
        HelpForm::Short
    } else {
        return None;
    };
    let root = Cli::command();
    let sub_name: Option<&str> = cli.command.as_ref().map(|c| match c {
        Command::Run => "run",
    });
    let (cmd, path) =
        match sub_name.and_then(|n| root.get_subcommands().find(|c| c.get_name() == n)) {
            Some(c) => (c, format!("{BIN_NAME} {}", c.get_name())),
            None => (&root, BIN_NAME.to_string()),
        };
    let model = build_model(cmd, &root, &path, form);
    Some(emit(form, &model, cli.color))
}

/// Render `model` in `form`, then write it to stdout — paging the long form on
/// a TTY when it overflows the screen. Returns the process exit code.
fn emit(form: HelpForm, model: &model::HelpModel, color: ColorWhen) -> i32 {
    // Resolve the terminal once: a single isatty probe and a single size query
    // feed palette resolution and paging alike.
    let is_tty = io::stdout().is_terminal();
    let term_height = terminal_size::terminal_size()
        .map(|(_, h)| h.0 as usize)
        .unwrap_or(24);
    let palette: Palette = palette::resolve_with_tty(color, is_tty);

    let mut buf: Vec<u8> = Vec::new();
    let render_result = match form {
        HelpForm::Short => render::render_short(&mut buf, model, &palette),
        HelpForm::Long => render::render_long(&mut buf, model, &palette),
    };
    if let Err(err) = render_result {
        eprintln!("{BIN_NAME}: help render failed: {err}");
        return 1;
    }

    let result = match form {
        HelpForm::Long => {
            let buffer_lines = buf.iter().filter(|b| **b == b'\n').count();
            if should_page(buffer_lines, /* no_pager */ false, is_tty, term_height) {
                let mut stderr = io::stderr();
                let mut out = io::stdout().lock();
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

/// Whether `token` is a flag (`-x` / `--long`), as opposed to a positional
/// operand. A bare `-` (stdin convention) is an operand, not a flag.
fn is_flag(token: &str) -> bool {
    token.starts_with('-') && token.len() > 1
}

/// Walk the raw args to find the deepest recognised subcommand chain, then
/// return the matching `clap::Command`, the user-facing path string, and a flag
/// indicating whether an unknown non-flag token was encountered.
///
/// `hit_unknown = true` means the args contain something like `vard graph
/// --help` where `graph` is not a known subcommand; the caller then passes
/// through to clap for proper error reporting.
///
/// Strategy: skip the binary name (`args[0]`) and any flag-like tokens (skipping
/// the value of a value-taking flag, derived from clap's arg metadata), walking
/// non-flag tokens as subcommand names into the clap tree. A non-flag token that
/// is neither a known subcommand nor a positional the command can accept sets
/// `hit_unknown` so clap surfaces its real error. Scanning stops at `--`.
fn resolve_subcmd_from_raw_args<'a>(
    root: &'a clap::Command,
    args: &[String],
) -> (&'a clap::Command, String, bool) {
    let mut current = root;
    let mut path = BIN_NAME.to_string();
    let mut positionals_seen = 0usize;

    let mut iter = args.iter().skip(1);
    while let Some(token) = iter.next() {
        // Everything after `--` is a positional operand, not a subcommand.
        if token == "--" {
            break;
        }
        // Skip flags and (for value-taking flags without `=`) their value.
        if is_flag(token) {
            if !token.contains('=') && flag_takes_value(current, root, token) {
                let _ = iter.next();
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
            positionals_seen = 0;
        } else if positionals_seen < positional_capacity(current) {
            // A value the command accepts as a positional — valid, keep scanning.
            positionals_seen += 1;
        } else {
            // Neither a subcommand nor an acceptable positional: unknown. Defer
            // to clap for the real error message.
            return (current, path, true);
        }
    }

    (current, path, false)
}

/// How many positional values `cmd` can accept. A variadic positional yields
/// `usize::MAX` (unbounded). Derived from clap so the interceptor tracks the real
/// definition (today `run` takes none).
fn positional_capacity(cmd: &clap::Command) -> usize {
    let mut total = 0usize;
    for p in cmd.get_positionals() {
        let max = p.get_num_args().map(|r| r.max_values()).unwrap_or(1);
        if max == usize::MAX {
            return usize::MAX;
        }
        total = total.saturating_add(max);
    }
    total
}

/// Whether the flag named by `token` (`--long` or `-x`) consumes a following
/// value, derived from clap's `ArgAction` (`Set`/`Append` take a value;
/// `SetTrue`, help, version, etc. do not). Checked on `current` first, then
/// `root` (where globals like `--color` are declared).
fn flag_takes_value(current: &clap::Command, root: &clap::Command, token: &str) -> bool {
    arg_takes_value(current, token)
        .or_else(|| arg_takes_value(root, token))
        .unwrap_or(false)
}

fn arg_takes_value(cmd: &clap::Command, token: &str) -> Option<bool> {
    let arg = if let Some(long) = token.strip_prefix("--") {
        cmd.get_arguments().find(|a| a.get_long() == Some(long))
    } else if let Some(rest) = token.strip_prefix('-') {
        let short = rest.chars().next()?;
        cmd.get_arguments().find(|a| a.get_short() == Some(short))
    } else {
        None
    };
    arg.map(|a| a.get_action().takes_values())
}

/// Parse `--color <VALUE>` (or `--color=VALUE`) from raw args using the same
/// `ValueEnum` clap uses, so an invalid value is not silently coerced. Returns
/// `Ok(Auto)` when absent, `Ok(_)` for a valid value, and `Err(())` for an
/// invalid value — in which case the caller defers to clap so its
/// invalid-value error (exit 2) surfaces even alongside a help flag.
fn parse_color_from_args(root: &clap::Command, args: &[String]) -> Result<ColorWhen, ()> {
    use clap::ValueEnum;
    let raw = extract_color_value(root, args);
    match raw {
        None => Ok(ColorWhen::Auto),
        Some(val) => ColorWhen::from_str(&val, /* ignore_case */ true).map_err(|_| ()),
    }
}

/// Pull the raw string value of the last `--color` occurrence (space or `=`
/// form), respecting a `--` terminator. `None` when `--color` is absent.
fn extract_color_value(root: &clap::Command, args: &[String]) -> Option<String> {
    let mut value = None;
    let mut iter = args.iter().skip(1);
    while let Some(token) = iter.next() {
        if token == "--" {
            break;
        }
        if token == "--color" {
            // Only consume a following value when the flag actually takes one.
            if flag_takes_value(root, root, token) {
                value = iter.next().cloned();
            }
        } else if let Some(val) = token.strip_prefix("--color=") {
            value = Some(val.to_string());
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_color_defaults_to_auto() {
        let root = Cli::command();
        let args = argv(&["vard", "run"]);
        assert_eq!(parse_color_from_args(&root, &args), Ok(ColorWhen::Auto));
    }

    #[test]
    fn parse_color_space_form() {
        let root = Cli::command();
        let args = argv(&["vard", "--color", "never"]);
        assert_eq!(parse_color_from_args(&root, &args), Ok(ColorWhen::Never));
    }

    #[test]
    fn parse_color_equals_form() {
        let root = Cli::command();
        let args = argv(&["vard", "--color=always"]);
        assert_eq!(parse_color_from_args(&root, &args), Ok(ColorWhen::Always));
    }

    #[test]
    fn parse_color_invalid_value_defers_to_clap() {
        let root = Cli::command();
        let args = argv(&["vard", "--color", "bogus", "--help"]);
        assert_eq!(
            parse_color_from_args(&root, &args),
            Err(()),
            "an invalid --color value must defer to clap, not coerce to Auto"
        );
    }

    #[test]
    fn detect_form_long_beats_short() {
        assert_eq!(
            detect_help_form(&argv(&["vard", "-h", "--help"])),
            Some(HelpForm::Long)
        );
    }

    #[test]
    fn detect_form_short() {
        assert_eq!(
            detect_help_form(&argv(&["vard", "run", "-h"])),
            Some(HelpForm::Short)
        );
    }

    #[test]
    fn detect_form_stops_at_double_dash() {
        // `--help` after `--` is a positional operand, not a help request.
        assert_eq!(detect_help_form(&argv(&["vard", "--", "--help"])), None);
    }

    #[test]
    fn detect_form_absent() {
        assert_eq!(detect_help_form(&argv(&["vard", "run"])), None);
    }

    #[test]
    fn resolve_root_when_no_subcommand() {
        let root = Cli::command();
        let args = argv(&["vard", "--help"]);
        let (_, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard");
        assert!(!hit_unknown);
    }

    #[test]
    fn resolve_descends_into_run() {
        let root = Cli::command();
        let args = argv(&["vard", "run", "--help"]);
        let (cmd, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard run");
        assert_eq!(cmd.get_name(), "run");
        assert!(!hit_unknown);
    }

    #[test]
    fn resolve_flags_unknown_subcommand() {
        let root = Cli::command();
        let args = argv(&["vard", "bogus", "--help"]);
        let (_, _, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert!(hit_unknown, "unknown subcommand must be flagged for clap");
    }

    #[test]
    fn resolve_flags_unknown_positional_on_leaf() {
        // `run` takes no positionals, so a stray token must defer to clap.
        let root = Cli::command();
        let args = argv(&["vard", "run", "bogus", "--help"]);
        let (_, _, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert!(
            hit_unknown,
            "a token `run` cannot accept must be flagged for clap"
        );
    }

    #[test]
    fn resolve_skips_color_value_before_subcommand() {
        let root = Cli::command();
        let args = argv(&["vard", "--color", "never", "run"]);
        let (cmd, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard run");
        assert_eq!(cmd.get_name(), "run");
        assert!(!hit_unknown);
    }

    #[test]
    fn resolve_stops_at_double_dash() {
        // After `--`, `run` is an operand, not a subcommand descent.
        let root = Cli::command();
        let args = argv(&["vard", "--", "run"]);
        let (cmd, path, hit_unknown) = resolve_subcmd_from_raw_args(&root, &args);
        assert_eq!(path, "vard");
        assert_eq!(cmd.get_name(), "vard");
        assert!(!hit_unknown);
    }

    #[test]
    fn flag_takes_value_derived_from_action() {
        let root = Cli::command();
        // `--color` takes a value (ArgAction::Set); `-h`/`--help` do not.
        assert!(flag_takes_value(&root, &root, "--color"));
        assert!(!flag_takes_value(&root, &root, "--help"));
        assert!(!flag_takes_value(&root, &root, "-h"));
    }
}
