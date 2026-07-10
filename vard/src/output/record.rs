//! Structured records rendered to any of the three output shapes.
//!
//! A command builds a list of [`Record`]s once, then hands them to the shape the
//! global `--format` resolved to: [`render_records`] for the human TTY form (a
//! leading count line and per-record key/value blocks, drawn with the
//! [`primitives`] kit), or [`render_json`] /
//! [`render_jsonl`] for the stable machine contract. One record model, three
//! renderers — so the human and machine forms can never drift in *which* fields
//! they carry, only in how they are drawn.

use std::io::{self, Write};
use std::time::Duration;

use super::palette::Palette;
use super::primitives::{self, Field};

/// One field's value, typed so the machine forms can distinguish a string, a
/// boolean, and an absent value (JSON `null`).
#[derive(Debug, Clone)]
pub(crate) enum Cell {
    /// A string value.
    Str(String),
    /// A boolean value — `yes`/`no` in records, `true`/`false` in JSON.
    Bool(bool),
    /// No value: `—` in records, `null` in JSON.
    Absent,
}

/// One key/value field of a record.
#[derive(Debug, Clone)]
pub(crate) struct RecordField {
    /// The field's key. Doubles as the JSON object key and the records label.
    pub key: &'static str,
    /// The field's value.
    pub cell: Cell,
    /// Whether to draw the value with the accent color in the records form.
    /// Ignored by the machine forms.
    pub highlight: bool,
}

impl RecordField {
    /// A plain string field.
    pub(crate) fn str(key: &'static str, value: impl Into<String>) -> Self {
        RecordField {
            key,
            cell: Cell::Str(value.into()),
            highlight: false,
        }
    }

    /// A boolean field.
    pub(crate) fn bool(key: &'static str, value: bool) -> Self {
        RecordField {
            key,
            cell: Cell::Bool(value),
            highlight: false,
        }
    }

    /// An optional string field: `Absent` (JSON `null`) when `None`.
    pub(crate) fn opt(key: &'static str, value: Option<impl Into<String>>) -> Self {
        RecordField {
            key,
            cell: value.map(|v| Cell::Str(v.into())).unwrap_or(Cell::Absent),
            highlight: false,
        }
    }

    /// Sets the records-form highlight.
    pub(crate) fn highlighted(mut self, on: bool) -> Self {
        self.highlight = on;
        self
    }
}

/// A single record: an optional identity header plus ordered fields.
#[derive(Debug, Clone)]
pub(crate) struct Record {
    /// The record's identity line (the records-form header). The machine forms
    /// ignore it — every datum they emit is a field.
    pub header: Option<String>,
    /// The record's fields, in display order.
    pub fields: Vec<RecordField>,
}

/// Renders `records` in the human records form: a leading count line, then each
/// record's block separated by a rule.
pub(crate) fn render_records(
    out: &mut dyn Write,
    palette: &Palette,
    records: &[Record],
    noun: &str,
    term_width: usize,
) -> io::Result<()> {
    primitives::count_line(out, palette, records.len(), records.len(), 1, noun)?;
    for record in records {
        primitives::separator(out, palette, term_width)?;
        let displays: Vec<(String, bool)> = record
            .fields
            .iter()
            .map(|f| (cell_display(&f.cell), f.highlight))
            .collect();
        let fields: Vec<Field<'_>> = record
            .fields
            .iter()
            .zip(displays.iter())
            .map(|(f, (value, highlight))| Field {
                label: f.key,
                value,
                highlight: *highlight,
            })
            .collect();
        primitives::record_block(out, palette, record.header.as_deref(), &fields, term_width)?;
    }
    Ok(())
}

/// Renders `records` as a single JSON array document.
pub(crate) fn render_json(out: &mut dyn Write, records: &[Record]) -> io::Result<()> {
    out.write_all(b"[")?;
    for (i, record) in records.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        write_json_object(out, record)?;
    }
    out.write_all(b"]\n")
}

/// Renders `records` as newline-delimited JSON: one object per line.
pub(crate) fn render_jsonl(out: &mut dyn Write, records: &[Record]) -> io::Result<()> {
    for record in records {
        write_json_object(out, record)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Writes one record as a compact JSON object.
pub(crate) fn write_json_object(out: &mut dyn Write, record: &Record) -> io::Result<()> {
    out.write_all(b"{")?;
    for (i, field) in record.fields.iter().enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        write_json_string(out, field.key)?;
        out.write_all(b":")?;
        match &field.cell {
            Cell::Str(s) => write_json_string(out, s)?,
            Cell::Bool(b) => out.write_all(if *b { b"true" } else { b"false" })?,
            Cell::Absent => out.write_all(b"null")?,
        }
    }
    out.write_all(b"}")
}

/// The records-form display string for a cell.
fn cell_display(cell: &Cell) -> String {
    match cell {
        Cell::Str(s) => s.clone(),
        Cell::Bool(true) => "yes".to_string(),
        Cell::Bool(false) => "no".to_string(),
        Cell::Absent => "—".to_string(),
    }
}

/// Writes `s` as a JSON string literal with the mandatory escapes (RFC 8259):
/// `"` and `\`, the named control escapes, and `\u00XX` for the rest below
/// U+0020.
fn write_json_string(out: &mut dyn Write, s: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    for c in s.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            '\u{08}' => out.write_all(b"\\b")?,
            '\u{0c}' => out.write_all(b"\\f")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => {
                let mut buf = [0u8; 4];
                out.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    out.write_all(b"\"")
}

/// Formats a [`Duration`] as a compact humantime string (`15m`, `1h30m`,
/// `10s`), the same grammar [`vard_core::parse_duration`] accepts, so a value
/// rendered here round-trips back through the config. Sub-second remainders are
/// appended as `ms`; a zero duration renders as `0s`.
pub(crate) fn format_duration(d: Duration) -> String {
    let mut secs = d.as_secs();
    let millis = d.subsec_millis();
    let mut parts = String::new();

    let days = secs / 86_400;
    secs %= 86_400;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    secs %= 60;

    if days > 0 {
        parts.push_str(&format!("{days}d"));
    }
    if hours > 0 {
        parts.push_str(&format!("{hours}h"));
    }
    if mins > 0 {
        parts.push_str(&format!("{mins}m"));
    }
    if secs > 0 {
        parts.push_str(&format!("{secs}s"));
    }
    if millis > 0 {
        parts.push_str(&format!("{millis}ms"));
    }
    if parts.is_empty() {
        parts.push_str("0s");
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            header: Some("notes".to_string()),
            fields: vec![
                RecordField::str("path", "/home/u/notes"),
                RecordField::opt("branch", Some("main")),
                RecordField::opt("remote", None::<String>),
                RecordField::bool("sync", true),
                RecordField::bool("paused", false),
            ],
        }
    }

    #[test]
    fn json_object_shapes_types_correctly() {
        let mut out = Vec::new();
        write_json_object(&mut out, &record()).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            r#"{"path":"/home/u/notes","branch":"main","remote":null,"sync":true,"paused":false}"#
        );
    }

    #[test]
    fn json_array_joins_records() {
        let mut out = Vec::new();
        render_json(&mut out, &[record(), record()]).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with('['));
        assert!(s.ends_with("]\n"));
        assert_eq!(s.matches("\"path\"").count(), 2);
    }

    #[test]
    fn jsonl_is_one_object_per_line() {
        let mut out = Vec::new();
        render_jsonl(&mut out, &[record(), record()]).unwrap();
        let s = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = s.trim_end().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('{') && lines[0].ends_with('}'));
    }

    #[test]
    fn json_string_escapes_quotes_backslashes_and_controls() {
        let mut out = Vec::new();
        write_json_string(&mut out, "a\"b\\c\n\td\u{01}").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#""a\"b\\c\n\td\u0001""#);
    }

    #[test]
    fn json_string_preserves_non_ascii() {
        let mut out = Vec::new();
        write_json_string(&mut out, "café—notes").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\"café—notes\"");
    }

    #[test]
    fn records_form_leads_with_count_line() {
        let mut out = Vec::new();
        render_records(&mut out, &Palette::off(), &[record()], "watches", 80).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("1 watches\n"), "got: {s}");
        assert!(s.contains("notes"));
        assert!(s.contains("path"));
        // Absent renders as an em dash, bool false as "no".
        assert!(s.contains("—"));
        assert!(s.contains("no"));
    }

    #[test]
    fn empty_collection_records_is_just_the_count_line() {
        let mut out = Vec::new();
        render_records(&mut out, &Palette::off(), &[], "watches", 80).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "0 watches\n");
    }

    #[test]
    fn empty_collection_json_is_empty_array() {
        let mut out = Vec::new();
        render_json(&mut out, &[]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[]\n");
    }

    #[test]
    fn format_duration_matches_humantime_grammar() {
        assert_eq!(format_duration(Duration::from_secs(10)), "10s");
        assert_eq!(format_duration(Duration::from_secs(900)), "15m");
        assert_eq!(format_duration(Duration::from_secs(1200)), "20m");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(3600 + 1800)), "1h30m");
        assert_eq!(format_duration(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn format_duration_round_trips_through_the_parser() {
        for secs in [10u64, 900, 1200, 3600, 5400, 86_400] {
            let text = format_duration(Duration::from_secs(secs));
            let parsed = vard_core::parse_duration(&text).unwrap();
            assert_eq!(parsed, Duration::from_secs(secs), "round-trip {text}");
        }
    }
}
