//! Glyph rendering — UTF-8 symbols with ASCII fallbacks.
//!
//! Call [`render`] to get the appropriate string for a glyph, and [`use_ascii`]
//! to probe the environment for the caller's preferred mode.
//!
//! Ported from norn's output layer as the shared foundation for the record and
//! tally primitives. Those primitives are not yet wired to a `vard` command
//! (the read/list commands land in VRD-15+), so this module carries an
//! `allow(dead_code)`: it is exercised by its own and the primitives' tests and
//! becomes live when those commands consume it.
#![allow(dead_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Pass,
    Warn,
    Err,
    Sep,
}

pub fn render(g: Glyph, ascii: bool) -> &'static str {
    match (g, ascii) {
        (Glyph::Pass, false) => "✓",
        (Glyph::Pass, true) => "[ok]",
        (Glyph::Warn, false) => "⚠",
        (Glyph::Warn, true) => "[warn]",
        (Glyph::Err, false) => "✗",
        (Glyph::Err, true) => "[err]",
        (Glyph::Sep, false) => "·",
        (Glyph::Sep, true) => ".",
    }
}

/// Whether to prefer ASCII fallbacks. True when `VARD_ASCII` is set or the
/// active locale is not UTF-8.
pub fn use_ascii() -> bool {
    if std::env::var_os("VARD_ASCII").is_some() {
        return true;
    }
    let locale =
        std::env::var("LC_ALL").unwrap_or_else(|_| std::env::var("LANG").unwrap_or_default());
    !locale.to_lowercase().contains("utf")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_utf_and_ascii() {
        assert_eq!(render(Glyph::Pass, false), "✓");
        assert_eq!(render(Glyph::Pass, true), "[ok]");
    }

    #[test]
    fn warn_utf_and_ascii() {
        assert_eq!(render(Glyph::Warn, false), "⚠");
        assert_eq!(render(Glyph::Warn, true), "[warn]");
    }

    #[test]
    fn err_utf_and_ascii() {
        assert_eq!(render(Glyph::Err, false), "✗");
        assert_eq!(render(Glyph::Err, true), "[err]");
    }

    #[test]
    fn sep_utf_and_ascii() {
        assert_eq!(render(Glyph::Sep, false), "·");
        assert_eq!(render(Glyph::Sep, true), ".");
    }
}
