//! Render a `HelpModel` to a byte buffer, per the CLI Help Output v2 spec:
//! - flag names render in `accent`
//! - value placeholders render in `fg`
//! - section headers render in `dim` bold uppercase
//! - short descriptions render in `dim`
//! - `-h` uses a single global aligned column across all groups
//! - `--help` uses hanging indent (flag on its own line, prose beneath)

use std::borrow::Cow;
use std::io::{self, Write};

use super::model::{FlagEntry, GlobalEntry, HelpModel};
use crate::output::palette::Palette;

const GLOBAL_DESC_MAX: usize = 70;
const REPO_URL: &str = "https://github.com/dbtlr/vard";

/// Abstracts over `FlagEntry` and `GlobalEntry` so the column layout and the
/// aligned line-writer can serve both without duplication.
trait LabelSource {
    fn short(&self) -> Option<char>;
    fn long(&self) -> Option<&str>;
    fn value_name(&self) -> Option<&str>;
    fn short_desc(&self) -> &str;
}

impl LabelSource for FlagEntry {
    fn short(&self) -> Option<char> {
        self.short
    }
    fn long(&self) -> Option<&str> {
        self.long.as_deref()
    }
    fn value_name(&self) -> Option<&str> {
        self.value_name.as_deref()
    }
    fn short_desc(&self) -> &str {
        &self.short_desc
    }
}

impl LabelSource for GlobalEntry {
    fn short(&self) -> Option<char> {
        self.short
    }
    fn long(&self) -> Option<&str> {
        self.long.as_deref()
    }
    fn value_name(&self) -> Option<&str> {
        self.value_name.as_deref()
    }
    fn short_desc(&self) -> &str {
        &self.short_desc
    }
}

fn label<T: LabelSource>(item: &T) -> String {
    let mut s = String::new();
    match (item.short(), item.long()) {
        (Some(short), Some(long)) => s.push_str(&format!("-{short}, --{long}")),
        (Some(short), None) => s.push_str(&format!("-{short}")),
        (None, Some(long)) => s.push_str(&format!("    --{long}")),
        (None, None) => {}
    }
    if let Some(vn) = item.value_name() {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("<{vn}>"));
    }
    s
}

/// Build the USAGE synopsis from the model: `path [OPTIONS]`, then each
/// positional (`<NAME>` required, `[NAME]` optional), then the subcommand slot
/// (`<COMMAND>` when required, `[COMMAND]` when a bare invocation is valid).
fn usage_synopsis(model: &HelpModel) -> String {
    let mut s = format!("{} [OPTIONS]", model.command_path);
    for p in &model.positionals {
        let name = p
            .value_name
            .as_deref()
            .or(p.long.as_deref())
            .unwrap_or("ARG");
        if p.required {
            s.push_str(&format!(" <{name}>"));
        } else {
            s.push_str(&format!(" [{name}]"));
        }
    }
    if !model.subcommands.is_empty() {
        if model.subcommand_required {
            s.push_str(" <COMMAND>");
        } else {
            s.push_str(" [COMMAND]");
        }
    }
    s
}

fn write_usage(out: &mut dyn Write, palette: &Palette, model: &HelpModel) -> io::Result<()> {
    write_section_header(out, palette, "USAGE")?;
    writeln!(
        out,
        "    {}{}{}",
        palette.fg.render(),
        usage_synopsis(model),
        palette.fg.render_reset()
    )?;
    writeln!(out)
}

/// Render the short (`-h`) form of `model` to `out`.
///
/// Flag lines in `-h` are one-liners; they align to a single global column and
/// do not wrap.
pub fn render_short(out: &mut dyn Write, model: &HelpModel, palette: &Palette) -> io::Result<()> {
    // Description line (dim).
    if !model.about.is_empty() {
        writeln!(
            out,
            "{}{}{}",
            palette.dim.render(),
            model.about,
            palette.dim.render_reset()
        )?;
        writeln!(out)?;
    }

    // USAGE line.
    write_usage(out, palette, model)?;

    // Positionals.
    if !model.positionals.is_empty() {
        write_section_header(out, palette, "ARGUMENTS")?;
        let refs: Vec<&FlagEntry> = model.positionals.iter().collect();
        let col = compute_column(&refs);
        for p in &model.positionals {
            write_aligned_line(out, palette, p, col, None)?;
        }
        writeln!(out)?;
    }

    // Flag groups — single column across ALL groups.
    let all_flags: Vec<&FlagEntry> = model.groups.iter().flat_map(|g| g.flags.iter()).collect();
    let col = compute_column(&all_flags);
    for group in &model.groups {
        write_section_header(out, palette, &group.heading.to_uppercase())?;
        for f in &group.flags {
            write_aligned_line(out, palette, f, col, None)?;
        }
        writeln!(out)?;
    }

    // Subcommands.
    if !model.subcommands.is_empty() {
        write_section_header(out, palette, "COMMANDS")?;
        write_subcommands(out, palette, &model.subcommands)?;
        writeln!(out)?;
    }

    // GLOBAL OPTIONS — full block, no collapse.
    write_globals(out, palette, model)?;

    // Footer: pointer to long form.
    writeln!(
        out,
        "{}For full help, run `{} --help`.{}",
        palette.dim.render(),
        model.command_path,
        palette.dim.render_reset()
    )?;

    Ok(())
}

pub(super) fn write_section_header(
    out: &mut dyn Write,
    palette: &Palette,
    heading: &str,
) -> io::Result<()> {
    writeln!(
        out,
        "{}{}{}",
        palette.section.render(),
        heading,
        palette.section.render_reset()
    )
}

fn write_subcommands(
    out: &mut dyn Write,
    palette: &Palette,
    subcommands: &[(String, String)],
) -> io::Result<()> {
    let max_name = subcommands.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, about) in subcommands {
        writeln!(
            out,
            "    {ts}{name:<width$}{te}  {ds}{about}{de}",
            ts = palette.accent.render(),
            name = name,
            width = max_name,
            te = palette.accent.render_reset(),
            ds = palette.dim.render(),
            about = about,
            de = palette.dim.render_reset(),
        )?;
    }
    Ok(())
}

/// `(longest "flag + placeholder") + 2 spaces`, over any [`LabelSource`].
fn compute_column<T: LabelSource>(items: &[&T]) -> usize {
    items.iter().map(|i| label(*i).len()).max().unwrap_or(0) + 2
}

/// Render the leading `-s, --long <PLACEHOLDER>` portion (without color).
pub(super) fn flag_label(f: &FlagEntry) -> String {
    label(f)
}

/// Truncate `s` to at most `max` display columns on a UTF-8 char boundary,
/// appending `…` when anything was cut. Byte slicing here would panic on a
/// multi-byte boundary; walking char boundaries keeps it safe.
fn truncate_desc(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let mut cut = max.saturating_sub(1).min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    Cow::Owned(format!("{}…", &s[..cut]))
}

/// Write one aligned `    <label><pad><desc>` line for any [`LabelSource`].
/// `desc_max`, when set, constrains the description (globals, per spec §2.2);
/// truncation is applied here as a pre-step so both flag and global lines share
/// this single writer.
fn write_aligned_line<T: LabelSource>(
    out: &mut dyn Write,
    palette: &Palette,
    item: &T,
    col: usize,
    desc_max: Option<usize>,
) -> io::Result<()> {
    let label = label(item);
    let (flag_part, placeholder_part) = split_flag_and_placeholder(&label);
    let pad = col.saturating_sub(label.len());
    let desc = match desc_max {
        Some(max) => truncate_desc(item.short_desc(), max),
        None => Cow::Borrowed(item.short_desc()),
    };
    writeln!(
        out,
        "    {fs}{flag}{fe}{ps}{ph}{pe}{spaces}{ds}{desc}{de}",
        fs = palette.accent.render(),
        flag = flag_part,
        fe = palette.accent.render_reset(),
        ps = palette.fg.render(),
        ph = placeholder_part,
        pe = palette.fg.render_reset(),
        spaces = " ".repeat(pad),
        ds = palette.dim.render(),
        desc = desc,
        de = palette.dim.render_reset(),
    )
}

/// Render the GLOBAL OPTIONS block (shared by short and long forms).
fn write_globals(out: &mut dyn Write, palette: &Palette, model: &HelpModel) -> io::Result<()> {
    if model.globals.is_empty() {
        return Ok(());
    }
    write_section_header(out, palette, "GLOBAL OPTIONS")?;
    let refs: Vec<&GlobalEntry> = model.globals.iter().collect();
    let col = compute_column(&refs);
    for g in &model.globals {
        write_aligned_line(out, palette, g, col, Some(GLOBAL_DESC_MAX))?;
    }
    writeln!(out)
}

pub(super) fn split_flag_and_placeholder(label: &str) -> (&str, &str) {
    if let Some(idx) = label.find(" <") {
        (&label[..idx], &label[idx..])
    } else {
        (label, "")
    }
}

/// Render the `EXAMPLES` section. Each entry is two lines: the command at
/// 4-space indent (per-token coloring), then the comment at 8-space indent in
/// `dim`. A blank line separates entries.
fn write_examples_block(
    out: &mut dyn Write,
    palette: &Palette,
    examples: &[(String, String)],
) -> io::Result<()> {
    if examples.is_empty() {
        return Ok(());
    }
    write_section_header(out, palette, "EXAMPLES")?;
    for (i, (cmd, comment)) in examples.iter().enumerate() {
        write_example_command(out, palette, cmd)?;
        writeln!(
            out,
            "        {ds}# {comment}{de}",
            ds = palette.dim.render(),
            comment = comment,
            de = palette.dim.render_reset(),
        )?;
        if i + 1 < examples.len() {
            writeln!(out)?;
        }
    }
    writeln!(out)?;
    Ok(())
}

/// Render a single example command line at 4-space indent with per-token
/// coloring: tokens starting with `-` render in `accent` (flags); everything
/// else in `fg` (including the literal `vard` prefix and value tokens).
fn write_example_command(out: &mut dyn Write, palette: &Palette, cmd: &str) -> io::Result<()> {
    write!(out, "    ")?;
    let mut first = true;
    for token in cmd.split_whitespace() {
        if !first {
            write!(out, " ")?;
        }
        first = false;
        if token.starts_with('-') {
            write!(
                out,
                "{ts}{token}{te}",
                ts = palette.accent.render(),
                te = palette.accent.render_reset(),
            )?;
        } else {
            write!(
                out,
                "{bs}{token}{be}",
                bs = palette.fg.render(),
                be = palette.fg.render_reset(),
            )?;
        }
    }
    writeln!(out)?;
    Ok(())
}

/// Render conceptual prose sections. One section per `(heading, body)` pair:
/// header in `dim` bold uppercase, body in default foreground at 4-space
/// indent. Paragraphs (split on `\n\n`) are separated by a blank line; every
/// line of a multi-line paragraph is indented. No-op when `sections` is empty.
fn write_conceptual_sections_block(
    out: &mut dyn Write,
    palette: &Palette,
    sections: &[(String, String)],
) -> io::Result<()> {
    for (heading, body) in sections {
        write_section_header(out, palette, &heading.to_uppercase())?;
        let paragraphs: Vec<&str> = body.split("\n\n").collect();
        for (i, paragraph) in paragraphs.iter().enumerate() {
            for line in paragraph.lines() {
                writeln!(out, "    {line}")?;
            }
            if i + 1 < paragraphs.len() {
                writeln!(out)?;
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Render the long (`--help`) form of `model` to `out`.
///
/// Hanging-indent style for flags: flag on its own line, descriptions/prose
/// indented 8 spaces beneath. Globals still use the aligned column.
pub fn render_long(out: &mut dyn Write, model: &HelpModel, palette: &Palette) -> io::Result<()> {
    // Description (one-line about).
    if !model.about.is_empty() {
        writeln!(
            out,
            "{}{}{}",
            palette.dim.render(),
            model.about,
            palette.dim.render_reset()
        )?;
        writeln!(out)?;
    }

    // Long about (multi-paragraph prose).
    if let Some(long) = &model.long_about {
        for paragraph in long.split("\n\n") {
            writeln!(
                out,
                "{}{}{}",
                palette.dim.render(),
                paragraph,
                palette.dim.render_reset()
            )?;
            writeln!(out)?;
        }
    }

    // USAGE.
    write_usage(out, palette, model)?;

    // Positionals — hanging indent.
    if !model.positionals.is_empty() {
        write_section_header(out, palette, "ARGUMENTS")?;
        for p in &model.positionals {
            write_flag_hanging(out, palette, p)?;
        }
    }

    // Flag groups — hanging indent.
    for group in &model.groups {
        write_section_header(out, palette, &group.heading.to_uppercase())?;
        for f in &group.flags {
            write_flag_hanging(out, palette, f)?;
        }
    }

    // Subcommands.
    if !model.subcommands.is_empty() {
        write_section_header(out, palette, "COMMANDS")?;
        write_subcommands(out, palette, &model.subcommands)?;
        writeln!(out)?;
    }

    // EXAMPLES — canned. Empty tables suppress the section.
    write_examples_block(out, palette, &model.extras.canned_examples)?;

    // Conceptual sections. Empty suppresses the block.
    write_conceptual_sections_block(out, palette, &model.extras.conceptual_sections)?;

    // GLOBAL OPTIONS — aligned column.
    write_globals(out, palette, model)?;

    // Footer: docs URL.
    writeln!(
        out,
        "{}Documentation: {}{}",
        palette.dim.render(),
        REPO_URL,
        palette.dim.render_reset()
    )?;

    Ok(())
}

fn write_flag_hanging(out: &mut dyn Write, palette: &Palette, f: &FlagEntry) -> io::Result<()> {
    // Flag line.
    let lbl = flag_label(f);
    let (flag_part, placeholder_part) = split_flag_and_placeholder(&lbl);
    writeln!(
        out,
        "    {fs}{flag}{fe}{ps}{ph}{pe}",
        fs = palette.accent.render(),
        flag = flag_part,
        fe = palette.accent.render_reset(),
        ps = palette.fg.render(),
        ph = placeholder_part,
        pe = palette.fg.render_reset(),
    )?;
    // Short description (always shown).
    if !f.short_desc.is_empty() {
        writeln!(
            out,
            "        {ds}{desc}{de}",
            ds = palette.dim.render(),
            desc = f.short_desc,
            de = palette.dim.render_reset(),
        )?;
    }
    // Long description (only when a flag earns one).
    if let Some(long) = &f.long_desc {
        for paragraph in long.split("\n\n") {
            writeln!(
                out,
                "        {ds}{p}{de}",
                ds = palette.dim.render(),
                p = paragraph,
                de = palette.dim.render_reset(),
            )?;
        }
    }
    // Possible enum values.
    if !f.possible_values.is_empty() {
        writeln!(
            out,
            "        {ds}Possible values: {vals}{de}",
            ds = palette.dim.render(),
            vals = f.possible_values.join(", "),
            de = palette.dim.render_reset(),
        )?;
    }
    // Default value(s), rendered `[default: …]` as the manpage does.
    if !f.default_values.is_empty() {
        writeln!(
            out,
            "        {ds}[default: {vals}]{de}",
            ds = palette.dim.render(),
            vals = f.default_values.join(", "),
            de = palette.dim.render_reset(),
        )?;
    }
    writeln!(out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::model::{FlagEntry, FlagGroup, GlobalEntry, HelpExtras, HelpModel};
    use crate::output::palette::Palette;

    fn sample_model() -> HelpModel {
        HelpModel {
            command_path: "vard run".to_string(),
            about: "Run the daemon".to_string(),
            long_about: None,
            positionals: vec![],
            groups: vec![FlagGroup {
                heading: "Run options".to_string(),
                flags: vec![
                    FlagEntry {
                        short: None,
                        long: Some("interval".to_string()),
                        value_name: Some("SECS".to_string()),
                        short_desc: "Debounce interval".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                        default_values: vec![],
                        required: false,
                    },
                    FlagEntry {
                        short: None,
                        long: Some("once".to_string()),
                        value_name: None,
                        short_desc: "Snapshot once then exit".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                        default_values: vec![],
                        required: false,
                    },
                ],
            }],
            globals: vec![GlobalEntry {
                short: None,
                long: Some("color".to_string()),
                value_name: Some("WHEN".to_string()),
                short_desc: "Color output: auto, always, or never".to_string(),
            }],
            subcommands: vec![],
            subcommand_required: false,
            extras: HelpExtras::default(),
        }
    }

    fn render_to_string(model: &HelpModel) -> String {
        let palette = Palette::off();
        let mut buf = Vec::new();
        render_short(&mut buf, model, &palette).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn renders_description_first() {
        let out = render_to_string(&sample_model());
        assert!(out.starts_with("Run the daemon\n"));
    }

    #[test]
    fn renders_usage_block() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("USAGE\n"));
        assert!(out.contains("vard run [OPTIONS]"));
    }

    #[test]
    fn usage_shows_optional_command_slot() {
        let mut model = sample_model();
        model.subcommands = vec![("run".to_string(), "Run the daemon".to_string())];
        model.subcommand_required = false;
        let out = render_to_string(&model);
        assert!(
            out.contains("[OPTIONS] [COMMAND]"),
            "optional subcommand must render [COMMAND]; got:\n{out}"
        );
        assert!(!out.contains("<COMMAND>"));
    }

    #[test]
    fn usage_shows_required_command_slot() {
        let mut model = sample_model();
        model.subcommands = vec![("run".to_string(), "Run the daemon".to_string())];
        model.subcommand_required = true;
        let out = render_to_string(&model);
        assert!(out.contains("[OPTIONS] <COMMAND>"));
    }

    #[test]
    fn usage_includes_positionals() {
        let mut model = sample_model();
        model.positionals = vec![
            FlagEntry {
                short: None,
                long: None,
                value_name: Some("PATH".to_string()),
                short_desc: "required path".to_string(),
                long_desc: None,
                possible_values: vec![],
                default_values: vec![],
                required: true,
            },
            FlagEntry {
                short: None,
                long: None,
                value_name: Some("EXTRA".to_string()),
                short_desc: "optional extra".to_string(),
                long_desc: None,
                possible_values: vec![],
                default_values: vec![],
                required: false,
            },
        ];
        let out = render_to_string(&model);
        assert!(
            out.contains("[OPTIONS] <PATH> [EXTRA]"),
            "positionals must render in USAGE; got:\n{out}"
        );
    }

    #[test]
    fn renders_group_heading_uppercased() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("RUN OPTIONS\n"));
        assert!(!out.contains("Run options\n"));
    }

    #[test]
    fn renders_flag_with_placeholder() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("--interval <SECS>"));
    }

    #[test]
    fn renders_globals_block_full() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("GLOBAL OPTIONS\n"));
        assert!(out.contains("--color <WHEN>"));
        assert!(out.contains("Color output"));
    }

    #[test]
    fn renders_short_form_footer_pointer() {
        let out = render_to_string(&sample_model());
        assert!(out.contains("For full help, run `vard run --help`."));
    }

    #[test]
    fn global_description_over_max_is_truncated() {
        let mut model = sample_model();
        model.globals[0].short_desc = "x".repeat(80);
        let out = render_to_string(&model);
        assert!(out.contains(&format!("{}…", "x".repeat(GLOBAL_DESC_MAX - 1))));
    }

    #[test]
    fn global_description_truncation_respects_char_boundary() {
        // A multi-byte character straddling the byte limit must not panic and
        // must truncate on a char boundary. `é` is two bytes; padding places one
        // astride the GLOBAL_DESC_MAX-1 cut point.
        let mut model = sample_model();
        model.globals[0].short_desc =
            format!("{}é{}", "a".repeat(GLOBAL_DESC_MAX - 2), "z".repeat(20));
        // Must not panic while rendering; the two-byte `é` straddles the cut, so
        // it is dropped whole and the prefix ends cleanly with the ellipsis.
        let out = render_to_string(&model);
        assert!(
            out.contains(&format!("{}…", "a".repeat(GLOBAL_DESC_MAX - 2))),
            "expected char-boundary truncation; got:\n{out}"
        );
    }

    #[test]
    fn aligned_column_uses_global_longest() {
        // Two groups with very different flag lengths — the column must align
        // to the longest across BOTH groups.
        let model = HelpModel {
            command_path: "vard run".to_string(),
            about: String::new(),
            long_about: None,
            positionals: vec![],
            groups: vec![
                FlagGroup {
                    heading: "A".to_string(),
                    flags: vec![FlagEntry {
                        short: None,
                        long: Some("x".to_string()),
                        value_name: None,
                        short_desc: "short".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                        default_values: vec![],
                        required: false,
                    }],
                },
                FlagGroup {
                    heading: "B".to_string(),
                    flags: vec![FlagEntry {
                        short: None,
                        long: Some("very-long-flag-name".to_string()),
                        value_name: Some("PLACEHOLDER".to_string()),
                        short_desc: "zebra".to_string(),
                        long_desc: None,
                        possible_values: vec![],
                        default_values: vec![],
                        required: false,
                    }],
                },
            ],
            globals: vec![],
            subcommands: vec![],
            subcommand_required: false,
            extras: HelpExtras::default(),
        };
        let out = render_to_string(&model);
        let lines: Vec<&str> = out.lines().collect();
        let short_line = lines.iter().find(|l| l.contains("short")).unwrap();
        let long_line = lines.iter().find(|l| l.contains("zebra")).unwrap();
        let short_pos = short_line.find("short").unwrap();
        let long_pos = long_line.find("zebra").unwrap();
        assert_eq!(short_pos, long_pos, "descriptions must align across groups");
    }

    fn render_long_to_string(model: &HelpModel) -> String {
        let palette = Palette::off();
        let mut buf = Vec::new();
        render_long(&mut buf, model, &palette).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn long_form_starts_with_about_then_long_about() {
        let mut model = sample_model();
        model.long_about = Some("Run the vard daemon.\n\nWatches and snapshots.".to_string());
        let out = render_long_to_string(&model);
        assert!(out.starts_with("Run the daemon\n"));
        assert!(out.contains("Run the vard daemon."));
        assert!(out.contains("Watches and snapshots."));
    }

    #[test]
    fn long_form_uses_hanging_indent_for_flags() {
        let mut model = sample_model();
        model.groups[0].flags[0].long_desc =
            Some("The debounce window before a snapshot fires.".to_string());
        let out = render_long_to_string(&model);
        let lines: Vec<&str> = out.lines().collect();
        let flag_idx = lines
            .iter()
            .position(|l| l.contains("--interval <SECS>"))
            .unwrap();
        let next = lines[flag_idx + 1];
        assert!(
            next.starts_with("        "),
            "hanging indent (8 spaces), got: {next:?}"
        );
        assert!(next.contains("Debounce interval"));
    }

    #[test]
    fn long_form_renders_long_desc_paragraphs_at_hanging_indent() {
        let mut model = sample_model();
        model.groups[0].flags[0].long_desc =
            Some("First paragraph of long_desc.\n\nSecond paragraph of long_desc.".to_string());
        let out = render_long_to_string(&model);
        let lines: Vec<&str> = out.lines().collect();
        let short_idx = lines
            .iter()
            .position(|l| l.contains("Debounce interval"))
            .expect("short_desc line");
        let first_para_idx = lines
            .iter()
            .position(|l| l.contains("First paragraph of long_desc."))
            .expect("first long_desc paragraph");
        let second_para_idx = lines
            .iter()
            .position(|l| l.contains("Second paragraph of long_desc."))
            .expect("second long_desc paragraph");
        assert!(short_idx < first_para_idx);
        assert!(first_para_idx < second_para_idx);
        assert!(lines[first_para_idx].starts_with("        "));
        assert!(lines[second_para_idx].starts_with("        "));
    }

    #[test]
    fn long_form_renders_default_values() {
        let mut model = sample_model();
        model.groups[0].flags[0].default_values = vec!["5".to_string()];
        let out = render_long_to_string(&model);
        assert!(
            out.contains("[default: 5]"),
            "expected default rendered like the manpage; got:\n{out}"
        );
    }

    #[test]
    fn long_form_renders_possible_values() {
        let mut model = sample_model();
        model.groups[0].flags[0].possible_values = vec![
            "records".to_string(),
            "json".to_string(),
            "jsonl".to_string(),
        ];
        let out = render_long_to_string(&model);
        assert!(out.contains("Possible values: records, json, jsonl"));
    }

    #[test]
    fn long_form_footer_is_docs_pointer() {
        let out = render_long_to_string(&sample_model());
        assert!(out.to_lowercase().contains("documentation"));
        assert!(out.contains("github.com"));
    }

    fn sample_model_with_examples() -> HelpModel {
        let mut m = sample_model();
        m.extras.canned_examples = vec![
            (
                "vard run".to_string(),
                "watch and snapshot until stopped".to_string(),
            ),
            (
                "vard --color never run".to_string(),
                "force plain output".to_string(),
            ),
        ];
        m
    }

    #[test]
    fn long_form_emits_examples_section_when_populated() {
        let out = render_long_to_string(&sample_model_with_examples());
        assert!(out.contains("EXAMPLES\n"));
        assert!(out.contains("vard run"));
        assert!(out.contains("# watch and snapshot until stopped"));
        assert!(out.contains("# force plain output"));
    }

    #[test]
    fn long_form_omits_examples_section_when_empty() {
        let out = render_long_to_string(&sample_model());
        assert!(
            !out.contains("EXAMPLES\n"),
            "empty canned_examples must not produce an EXAMPLES section; got:\n{out}"
        );
    }

    #[test]
    fn examples_section_positioned_before_global_options() {
        let out = render_long_to_string(&sample_model_with_examples());
        let ex_idx = out.find("EXAMPLES\n").expect("EXAMPLES section present");
        let go_idx = out
            .find("GLOBAL OPTIONS\n")
            .expect("GLOBAL OPTIONS section present");
        assert!(ex_idx < go_idx);
    }

    #[test]
    fn examples_indent_command_at_4_and_comment_at_8() {
        let out = render_long_to_string(&sample_model_with_examples());
        let lines: Vec<&str> = out.lines().collect();
        let cmd_line = lines
            .iter()
            .find(|l| l.trim() == "vard run")
            .expect("command line present");
        assert!(cmd_line.starts_with("    vard"));
        let comment_line = lines
            .iter()
            .find(|l| l.contains("# watch and snapshot"))
            .expect("comment line present");
        assert!(comment_line.starts_with("        #"));
    }

    #[test]
    fn short_form_never_emits_examples() {
        let out = render_to_string(&sample_model_with_examples());
        assert!(
            !out.contains("EXAMPLES\n"),
            "short form (-h) must not include EXAMPLES; got:\n{out}"
        );
    }

    fn sample_model_with_conceptual() -> HelpModel {
        let mut m = sample_model();
        m.extras.conceptual_sections = vec![(
            "How vard works".to_string(),
            "First paragraph of conceptual prose.\n\nSecond paragraph.".to_string(),
        )];
        m
    }

    #[test]
    fn long_form_emits_conceptual_section_header_uppercased() {
        let out = render_long_to_string(&sample_model_with_conceptual());
        assert!(out.contains("HOW VARD WORKS\n"));
        assert!(!out.contains("How vard works\n"));
    }

    #[test]
    fn long_form_emits_conceptual_section_body_paragraphs() {
        let out = render_long_to_string(&sample_model_with_conceptual());
        let lines: Vec<&str> = out.lines().collect();
        let first_idx = lines
            .iter()
            .position(|l| l.contains("First paragraph of conceptual prose."))
            .expect("first paragraph present");
        let second_idx = lines
            .iter()
            .position(|l| l.contains("Second paragraph."))
            .expect("second paragraph present");
        assert!(first_idx < second_idx);
        assert!(
            lines[first_idx + 1..second_idx]
                .iter()
                .any(|l| l.is_empty()),
            "expected blank line between paragraphs"
        );
    }

    #[test]
    fn long_form_conceptual_body_indented_at_4_spaces() {
        let out = render_long_to_string(&sample_model_with_conceptual());
        let para = out
            .lines()
            .find(|l| l.contains("First paragraph of conceptual prose."))
            .expect("paragraph present");
        assert!(para.starts_with("    "));
    }

    #[test]
    fn long_form_omits_conceptual_block_when_empty() {
        let out = render_long_to_string(&sample_model());
        assert!(!out.contains("HOW VARD WORKS"));
    }

    #[test]
    fn conceptual_sections_positioned_between_examples_and_globals() {
        let mut m = sample_model_with_conceptual();
        m.extras.canned_examples = vec![("vard run".to_string(), "start it".to_string())];
        let out = render_long_to_string(&m);
        let ex_idx = out.find("EXAMPLES\n").expect("EXAMPLES present");
        let concept_idx = out.find("HOW VARD WORKS").expect("conceptual present");
        let go_idx = out
            .find("GLOBAL OPTIONS\n")
            .expect("GLOBAL OPTIONS present");
        assert!(ex_idx < concept_idx);
        assert!(concept_idx < go_idx);
    }

    #[test]
    fn short_form_never_emits_conceptual_sections() {
        let out = render_to_string(&sample_model_with_conceptual());
        assert!(!out.contains("HOW VARD WORKS"));
    }

    #[test]
    fn long_form_indents_every_line_of_multiline_paragraph() {
        let mut m = sample_model();
        m.extras.conceptual_sections = vec![(
            "Startup".to_string(),
            "Startup runs:\n1. Lock.\n2. Load config.\n3. Watch.".to_string(),
        )];
        let out = render_long_to_string(&m);
        let lines: Vec<&str> = out.lines().collect();
        for needle in ["Startup runs:", "1. Lock.", "2. Load config.", "3. Watch."] {
            let line = lines
                .iter()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("expected line containing {needle:?}; got:\n{out}"));
            assert!(
                line.starts_with("    "),
                "line {needle:?} must be 4-space indented, got: {line:?}"
            );
        }
    }
}
