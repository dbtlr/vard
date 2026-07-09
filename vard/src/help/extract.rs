//! Build a [`HelpModel`] from a [`clap::Command`].

use clap::Command;

use super::model::{FlagEntry, FlagGroup, GlobalEntry, HelpExtras, HelpForm, HelpModel};

/// Heading used for a flag that has no `help_heading` annotation. Rendered
/// uppercased by the renderer.
const DEFAULT_FLAG_HEADING: &str = "Options";

/// Arg ids that never render in the model (clap's help/version plumbing and
/// the intercepted `-h`/`--help` flags).
fn is_meta_arg(id: &str) -> bool {
    matches!(id, "help" | "help_short" | "help_long" | "version")
}

/// Walk the given clap `Command` and produce a fully-populated `HelpModel`.
///
/// - `cmd_path` is the user-facing path string, e.g. `"vard run"`. The caller
///   assembles this from `BIN_NAME` and the subcommand chain.
/// - `root` is the root `Cli::command()`. When `cmd` is a subcommand, global
///   options are read from `root` (clap only marks them `global_set` on the
///   declaring command, not on inherited copies in subcommands).
/// - `form` selects whether long_about/long_help are carried into the model.
pub fn build_model(cmd: &Command, root: &Command, cmd_path: &str, form: HelpForm) -> HelpModel {
    let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
    let long_about = match form {
        HelpForm::Long => cmd.get_long_about().map(|s| s.to_string()),
        HelpForm::Short => None,
    };

    let mut positionals: Vec<FlagEntry> = Vec::new();
    let mut groups: Vec<FlagGroup> = Vec::new();

    // Collect globals from the root command (the source of truth for global
    // args). Clap propagates globals to subcommands but `is_global_set()` only
    // returns `true` on the declaring command, not on inherited copies.
    let globals: Vec<GlobalEntry> = root
        .get_arguments()
        .filter(|a| a.is_global_set() && !is_meta_arg(a.get_id().as_str()))
        .map(|a| {
            let entry = flag_entry_from_arg(a, form);
            GlobalEntry {
                short: entry.short,
                long: entry.long,
                value_name: entry.value_name,
                short_desc: entry.short_desc,
            }
        })
        .collect();

    // Walk this command's args. Globals were already collected from `root`; the
    // `is_global_set()` skip below prevents double-collection.
    for arg in cmd.get_arguments() {
        if is_meta_arg(arg.get_id().as_str()) {
            continue;
        }
        if arg.is_global_set() {
            continue;
        }
        let entry = flag_entry_from_arg(arg, form);
        if arg.is_positional() {
            positionals.push(entry);
            continue;
        }
        let heading = arg
            .get_help_heading()
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_FLAG_HEADING.to_string());
        if let Some(g) = groups.iter_mut().find(|g| g.heading == heading) {
            g.flags.push(entry);
        } else {
            groups.push(FlagGroup {
                heading,
                flags: vec![entry],
            });
        }
    }

    let subcommands = cmd
        .get_subcommands()
        .filter(|sc| !sc.is_hide_set())
        .map(|sc| {
            (
                sc.get_name().to_string(),
                sc.get_about().map(|s| s.to_string()).unwrap_or_default(),
            )
        })
        .collect();

    HelpModel {
        command_path: cmd_path.to_string(),
        about,
        long_about,
        positionals,
        groups,
        globals,
        subcommands,
        extras: HelpExtras {
            canned_examples: super::examples::examples_for(cmd_path),
            conceptual_sections: super::examples::conceptual_sections_for(cmd_path),
        },
    }
}

/// Map a single clap `Arg` to a `FlagEntry`, gating `long_desc` on `form`.
fn flag_entry_from_arg(arg: &clap::Arg, form: HelpForm) -> FlagEntry {
    let short = arg.get_short();
    let long = arg.get_long().map(|s| s.to_string());
    // SetTrue / SetFalse flags take no value — suppress any clap-generated
    // placeholder even if `get_value_names` returns one.
    let value_name = match arg.get_action() {
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => None,
        _ => arg
            .get_value_names()
            .and_then(|v| v.first())
            .map(|s| s.to_string()),
    };
    let short_desc = arg.get_help().map(|s| s.to_string()).unwrap_or_default();
    let long_desc = match form {
        HelpForm::Long => arg.get_long_help().map(|s| s.to_string()),
        HelpForm::Short => None,
    };
    // Collect enum possible values (e.g. ["records", "json", "jsonl"] for
    // --format). These replace clap's "[possible values: …]" annotation.
    let possible_values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|pv| !pv.is_hide_set())
        .map(|pv| pv.get_name().to_string())
        .collect();
    FlagEntry {
        short,
        long,
        value_name,
        short_desc,
        long_desc,
        possible_values,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};

    fn sample_command() -> Command {
        Command::new("run")
            .about("Run the daemon")
            .long_about("Run the vard daemon.\n\nWatches and snapshots directories.")
            .arg(
                Arg::new("once")
                    .long("once")
                    .action(ArgAction::SetTrue)
                    .help("Snapshot once then exit")
                    .help_heading("Run options"),
            )
            .arg(
                Arg::new("interval")
                    .long("interval")
                    .value_name("SECS")
                    .help("Debounce interval")
                    .help_heading("Timing"),
            )
            .arg(
                Arg::new("color")
                    .long("color")
                    .global(true)
                    .value_name("WHEN")
                    .help("Color output")
                    .help_heading("Global options"),
            )
    }

    #[test]
    fn extracts_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        assert_eq!(model.about, "Run the daemon");
    }

    #[test]
    fn short_form_omits_long_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        assert!(model.long_about.is_none());
    }

    #[test]
    fn long_form_includes_long_about() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Long);
        assert!(model.long_about.as_deref().unwrap().contains("daemon"));
    }

    #[test]
    fn groups_flags_by_help_heading() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        let run = model
            .groups
            .iter()
            .find(|g| g.heading == "Run options")
            .expect("Run options group");
        assert_eq!(run.flags.len(), 1);
    }

    #[test]
    fn groups_preserve_first_seen_order() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        let headings: Vec<&str> = model.groups.iter().map(|g| g.heading.as_str()).collect();
        assert_eq!(headings, vec!["Run options", "Timing"]);
    }

    #[test]
    fn globals_are_separated() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        assert_eq!(model.globals.len(), 1);
        assert_eq!(model.globals[0].long.as_deref(), Some("color"));
        for g in &model.groups {
            assert!(g.flags.iter().all(|f| f.long.as_deref() != Some("color")));
        }
    }

    #[test]
    fn value_names_are_captured() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        let interval = model
            .groups
            .iter()
            .flat_map(|g| g.flags.iter())
            .find(|f| f.long.as_deref() == Some("interval"))
            .expect("interval flag");
        assert_eq!(interval.value_name.as_deref(), Some("SECS"));
    }

    #[test]
    fn settrue_flag_has_no_placeholder() {
        let cmd = sample_command();
        let model = build_model(&cmd, &cmd, "vard run", HelpForm::Short);
        let once = model
            .groups
            .iter()
            .flat_map(|g| g.flags.iter())
            .find(|f| f.long.as_deref() == Some("once"))
            .expect("once flag");
        assert!(once.value_name.is_none());
    }
}
