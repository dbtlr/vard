//! Composable line writers — the shared record/count/tally output kit.
//!
//! Ported from norn's output layer. Commands compose their stdout from these
//! writers using a resolved [`Palette`], so every command lands the same
//! spacing, wrapping, and color conventions. The count line leads the output;
//! record blocks carry an identity-line header over aligned key/value rows with
//! hanging-indent wrap; the separator draws a capped rule.
//!
//! The count line, record blocks, and separator are wired into `vard watch
//! list` (via [`record`](super::record)). The status headline and severity
//! tally have no consumer yet — they carry a targeted `allow(dead_code)` and
//! are exercised by the tests below until a status/check command lands.

use std::io::{self, Write};

use super::glyphs::{self, Glyph};
use super::palette::Palette;

/// Status headline: `{text}…` in `dim`. One trailing newline.
#[allow(dead_code)] // No consumer yet; a future status command will use it.
pub fn status_headline(out: &mut dyn Write, p: &Palette, text: &str) -> io::Result<()> {
    write!(out, "{}{text}…{}", p.dim.render(), p.dim.render_reset())?;
    writeln!(out)
}

/// Count line: leads a command's output with the total, and a window when the
/// result set is truncated.
/// - `total == returned` (or empty) → `"{total} {noun}\n"` (no window).
/// - `returned < total` → `"{total} {noun} · showing {starts_at}–{end}\n"`
///   where `end = starts_at + returned − 1`.
/// - Both numbers and separator emitted in `dim`.
pub fn count_line(
    out: &mut dyn Write,
    p: &Palette,
    total: usize,
    returned: usize,
    starts_at: usize,
    noun: &str,
) -> io::Result<()> {
    let sep = glyphs::render(Glyph::Sep, glyphs::use_ascii());
    write!(out, "{}{total} {noun}", p.dim.render())?;
    if returned > 0 && returned < total {
        let end = starts_at + returned - 1;
        write!(out, " {sep} showing {starts_at}–{end}")?;
    }
    write!(out, "{}", p.dim.render_reset())?;
    writeln!(out)
}

pub struct Field<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub highlight: bool,
}

/// Record block: an optional identity-line header at column 0 (pass `None` for
/// header-less records where every datum is a field row), then 2-indent field
/// rows. Label column width = `max(label.len()) + 2`. Long values wrap to the
/// value column on continuation lines (no label repeated). Values containing
/// words longer than the value column are force-broken at the column boundary
/// so they stay cell-shaped. `highlight: true` renders the value in `accent`;
/// otherwise `fg`.
pub fn record_block(
    out: &mut dyn Write,
    p: &Palette,
    header: Option<&str>,
    fields: &[Field<'_>],
    term_width: usize,
) -> io::Result<()> {
    if let Some(h) = header {
        let h = sanitize_controls(h);
        writeln!(out, "{}{h}{}", p.header.render(), p.header.render_reset())?;
    }
    if fields.is_empty() {
        return Ok(());
    }
    let label_w = fields.iter().map(|f| f.label.len()).max().unwrap_or(0) + 2;
    let value_w = term_width.saturating_sub(2 + label_w).max(20);

    for f in fields {
        let val_style = if f.highlight { &p.accent } else { &p.fg };
        let value = sanitize_controls(f.value);
        let wrapped = wrap_value(&value, value_w);
        for (i, line) in wrapped.iter().enumerate() {
            if i == 0 {
                writeln!(
                    out,
                    "  {l_start}{label:<label_w$}{l_end}{v_start}{line}{v_end}",
                    l_start = p.label.render(),
                    label = f.label,
                    l_end = p.label.render_reset(),
                    v_start = val_style.render(),
                    v_end = val_style.render_reset(),
                )?;
            } else {
                writeln!(
                    out,
                    "  {pad:<label_w$}{v_start}{line}{v_end}",
                    pad = "",
                    v_start = val_style.render(),
                    v_end = val_style.render_reset(),
                )?;
            }
        }
    }
    Ok(())
}

/// Replaces C0 control characters — except tab and newline, which the wrapper
/// handles as layout — and DEL (`0x7f`) with the Unicode replacement character
/// `U+FFFD`, so a value carrying a raw `ESC` (or other control) cannot inject
/// terminal escape sequences into records/TTY output. Applied at the primitives
/// layer, every records consumer inherits it. The JSON path escapes separately
/// (see [`super::record`]) and is left untouched.
///
/// Exposed to the crate so a command building a one-off human line by hand (e.g.
/// `vard notify`) can apply the same protection the record primitives apply
/// automatically.
pub(crate) fn sanitize_controls(s: &str) -> String {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            if (cp < 0x20 && c != '\n' && c != '\t') || cp == 0x7f {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// Collapses a possibly multi-line string into one line: each non-empty line is
/// trimmed and joined with `"; "`, then any internal whitespace run collapses to
/// a single space. So a multi-line git error renders as one prompt/status line.
pub(crate) fn flatten_ws(s: &str) -> String {
    let joined = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    joined.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Flattens whitespace to a single line ([`flatten_ws`]) and strips control
/// characters ([`sanitize_controls`]), so a crafted watch name or a multi-line
/// health summary cannot break or inject into a terminal line. Shared by `notify`
/// and `status` so a multi-line summary renders identically in both.
pub(crate) fn clean_line(s: &str) -> String {
    sanitize_controls(&flatten_ws(s))
}

fn wrap_value(value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        if word.chars().count() > width {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.extend(chunk_str(word, width));
            continue;
        }
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn chunk_str(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in s.chars() {
        current.push(c);
        count += 1;
        if count >= width {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Rule separator: a `─` bar capped at 60 columns, in `dim`.
pub fn separator(out: &mut dyn Write, p: &Palette, term_width: usize) -> io::Result<()> {
    let width = term_width.min(60);
    let bar: String = "─".repeat(width);
    writeln!(out, "{}{}{}", p.dim.render(), bar, p.dim.render_reset())
}

#[allow(dead_code)] // Consumed by a future status/notes command.
#[derive(Debug, Clone, Copy)]
pub enum NoteLabel {
    Note,
    Tip,
}

/// Note line: `{label}: {body}` — label in `accent`, body in `dim`.
#[allow(dead_code)] // No consumer yet; a future status/notes command will use it.
pub fn note_line(out: &mut dyn Write, p: &Palette, label: NoteLabel, body: &str) -> io::Result<()> {
    let label_str = match label {
        NoteLabel::Note => "note",
        NoteLabel::Tip => "tip",
    };
    writeln!(
        out,
        "{l_start}{label_str}:{l_end} {b_start}{body}{b_end}",
        l_start = p.accent.render(),
        l_end = p.accent.render_reset(),
        b_start = p.dim.render(),
        b_end = p.dim.render_reset(),
    )
}

/// Severity tally: a three-line block (pass / warn / err) with zero rows
/// elided and right-aligned counts. If all three are zero, emits a single
/// "0 {noun} pass" row so the caller still has a visible "the command ran"
/// signal.
#[allow(dead_code)] // No consumer yet; a future check/status command will use it.
pub fn severity_tally(
    out: &mut dyn Write,
    p: &Palette,
    pass: usize,
    warn: usize,
    err: usize,
    noun: &str,
) -> io::Result<()> {
    let ascii = glyphs::use_ascii();
    let max_count = pass.max(warn).max(err);
    let w = max_count.to_string().len();

    let emit_pass = pass > 0 || (warn == 0 && err == 0);
    if emit_pass {
        let g = glyphs::render(Glyph::Pass, ascii);
        writeln!(
            out,
            "  {}{g}{}  {pass:>w$} {noun} pass",
            p.success.render(),
            p.success.render_reset(),
        )?;
    }
    if warn > 0 {
        let g = glyphs::render(Glyph::Warn, ascii);
        let label = if warn == 1 { "warning" } else { "warnings" };
        writeln!(
            out,
            "  {}{g}{}  {warn:>w$} {label}",
            p.warning.render(),
            p.warning.render_reset(),
        )?;
    }
    if err > 0 {
        let g = glyphs::render(Glyph::Err, ascii);
        let label = if err == 1 { "error" } else { "errors" };
        writeln!(
            out,
            "  {}{g}{}  {err:>w$} {label}",
            p.error.render(),
            p.error.render_reset(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_headline_writes_text_then_ellipsis_and_newline() {
        let mut out = Vec::new();
        status_headline(&mut out, &Palette::off(), "loading config").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "loading config…\n");
    }

    #[test]
    fn status_headline_on_palette_wraps_with_dim_ansi() {
        let mut out = Vec::new();
        status_headline(&mut out, &Palette::on(), "x").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b["), "expected ANSI: {s:?}");
        assert!(s.contains("x…"));
    }

    #[test]
    fn count_line_full_set_omits_window() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 3, 3, 1, "watches").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "3 watches\n");
    }

    #[test]
    fn count_line_windowed_shows_range() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 23, 10, 1, "watches").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "23 watches · showing 1–10\n"
        );
    }

    #[test]
    fn count_line_starts_at_offset() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 23, 10, 11, "watches").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "23 watches · showing 11–20\n"
        );
    }

    #[test]
    fn count_line_empty_set() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 0, 0, 1, "watches").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "0 watches\n");
    }

    #[test]
    fn count_line_no_ansi_when_palette_off() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::off(), 23, 10, 1, "watches").unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("\x1b["));
    }

    #[test]
    fn count_line_ansi_when_palette_on() {
        let mut out = Vec::new();
        count_line(&mut out, &Palette::on(), 23, 10, 1, "watches").unwrap();
        assert!(String::from_utf8(out).unwrap().contains("\x1b["));
    }

    #[test]
    fn severity_tally_pure_pass_shows_only_check_row() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 100, 0, 0, "watches").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("✓"));
        assert!(s.contains("100 watches pass"));
        assert!(!s.contains("warnings"));
        assert!(!s.contains("errors"));
    }

    #[test]
    fn severity_tally_mixed_shows_all_nonzero_rows_in_order() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 698, 71, 11, "watches").unwrap();
        let s = String::from_utf8(out).unwrap();
        let pass_pos = s.find("698 watches pass").unwrap();
        let warn_pos = s.find("71 warnings").unwrap();
        let err_pos = s.find("11 errors").unwrap();
        assert!(
            pass_pos < warn_pos && warn_pos < err_pos,
            "order pass→warn→err"
        );
    }

    #[test]
    fn severity_tally_elides_zero_rows() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 698, 0, 11, "watches").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("698 watches pass"));
        assert!(!s.contains("warnings"));
        assert!(s.contains("11 errors"));
    }

    #[test]
    fn severity_tally_all_zero_emits_zero_pass_row() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 0, 0, 0, "watches").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("0 watches pass"));
    }

    #[test]
    fn severity_tally_singular_warning_and_error_nouns() {
        let mut out = Vec::new();
        severity_tally(&mut out, &Palette::off(), 100, 1, 1, "watches").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("1 warning"));
        assert!(!s.contains("1 warnings"));
        assert!(s.contains("1 error"));
        assert!(!s.contains("1 errors"));
    }

    #[test]
    fn record_block_emits_header_then_2_indent_fields() {
        let mut out = Vec::new();
        let fields = [
            Field {
                label: "path",
                value: "~/notes",
                highlight: false,
            },
            Field {
                label: "status",
                value: "watching",
                highlight: false,
            },
        ];
        record_block(&mut out, &Palette::off(), Some("vault"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "vault");
        // Label column width = max("path", "status") + 2 = 8 → "path    " / "status  ".
        assert_eq!(lines[1], "  path    ~/notes");
        assert_eq!(lines[2], "  status  watching");
    }

    #[test]
    fn record_block_sanitizes_control_characters_in_values_and_headers() {
        // A value carrying a raw ESC (as a malicious watch name/path might) must
        // not reach the terminal as an escape sequence — it renders as U+FFFD.
        let mut out = Vec::new();
        let fields = [Field {
            label: "name",
            value: "evil\x1b[31mred\x07",
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("hd\x1br"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains('\x1b'), "raw ESC must not survive: {s:?}");
        assert!(!s.contains('\x07'), "raw BEL must not survive: {s:?}");
        assert!(s.contains('\u{fffd}'), "expected replacement char: {s:?}");
        // Tab is layout whitespace, not a dangerous control: it must NOT be
        // sanitized to U+FFFD (the wrapper collapses it as whitespace instead).
        let mut out2 = Vec::new();
        let tabbed = [Field {
            label: "k",
            value: "a\tb",
            highlight: false,
        }];
        record_block(&mut out2, &Palette::off(), None, &tabbed, 80).unwrap();
        let s2 = String::from_utf8(out2).unwrap();
        assert!(
            !s2.contains('\u{fffd}'),
            "tab must not be sanitized: {s2:?}"
        );
        assert!(s2.contains("a b") || s2.contains("a\tb"), "got: {s2:?}");
    }

    #[test]
    fn record_block_wraps_long_value_across_multiple_lines() {
        let mut out = Vec::new();
        let long = "the quick brown fox jumps over the lazy dog one more time";
        let fields = [Field {
            label: "k",
            value: long,
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("h"), &fields, 30).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // Header + at least 2 wrapped lines under "k".
        assert!(lines.len() >= 3, "expected wrap, got {:?}", lines);
        assert_eq!(lines[0], "h");
        // Continuation lines align past the label column.
        assert!(lines[2].starts_with("    "));
    }

    #[test]
    fn record_block_force_breaks_long_unbreakable_word() {
        // UUIDs and similar no-whitespace tokens must force-break at the value
        // column instead of soft-wrapping into the key column.
        let mut out = Vec::new();
        let id = "a1b2c3d4-5e6f-7a8b-9c0d-1e2f3a4b5c6d";
        let fields = [Field {
            label: "id",
            value: id,
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("h"), &fields, 30).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines.len() >= 3,
            "expected force-break wrap into multiple lines: {lines:?}"
        );
        assert!(
            lines[2].starts_with("    "),
            "continuation should be indented past key column: {lines:?}"
        );
    }

    #[test]
    fn record_block_highlight_uses_accent_ansi_when_palette_on() {
        let mut out = Vec::new();
        let fields = [Field {
            label: "k",
            value: "v",
            highlight: true,
        }];
        record_block(&mut out, &Palette::on(), Some("h"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        // accent = ANSI 256 color 67 → escape "\x1b[38;5;67m"
        assert!(s.contains("\x1b[38;5;67m"), "expected accent ansi: {s:?}");
    }

    #[test]
    fn record_block_no_highlight_does_not_use_accent() {
        let mut out = Vec::new();
        let fields = [Field {
            label: "k",
            value: "v",
            highlight: false,
        }];
        record_block(&mut out, &Palette::on(), Some("h"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("\x1b[38;5;67m"),
            "unexpected accent ansi: {s:?}"
        );
    }

    #[test]
    fn record_block_single_field_emits_2_indent() {
        let mut out = Vec::new();
        let fields = [Field {
            label: "k",
            value: "v",
            highlight: false,
        }];
        record_block(&mut out, &Palette::off(), Some("h"), &fields, 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // Single label "k": label_w = 1 + 2 = 3 → "k  v".
        assert_eq!(lines[1], "  k  v");
    }

    #[test]
    fn separator_caps_at_60_when_term_wider() {
        let mut out = Vec::new();
        separator(&mut out, &Palette::off(), 200).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().filter(|c| *c == '─').count(), 60);
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn separator_respects_narrower_term() {
        let mut out = Vec::new();
        separator(&mut out, &Palette::off(), 40).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.chars().filter(|c| *c == '─').count(), 40);
    }

    #[test]
    fn note_line_with_note_label() {
        let mut out = Vec::new();
        note_line(
            &mut out,
            &Palette::off(),
            NoteLabel::Note,
            "config reloaded",
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "note: config reloaded\n");
    }

    #[test]
    fn note_line_with_tip_label() {
        let mut out = Vec::new();
        note_line(
            &mut out,
            &Palette::off(),
            NoteLabel::Tip,
            "run vard run to start",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "tip: run vard run to start\n"
        );
    }

    #[test]
    fn note_line_on_palette_emits_ansi() {
        let mut out = Vec::new();
        note_line(&mut out, &Palette::on(), NoteLabel::Tip, "body").unwrap();
        assert!(String::from_utf8(out).unwrap().contains("\x1b["));
    }
}
