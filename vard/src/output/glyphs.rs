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
    let locale = resolve_locale(
        std::env::var("LC_ALL").ok(),
        std::env::var("LC_CTYPE").ok(),
        std::env::var("LANG").ok(),
    );
    !locale.to_lowercase().contains("utf")
}

/// The effective locale under POSIX precedence: the first NON-EMPTY of
/// `LC_ALL`, `LC_CTYPE`, `LANG`. An empty value counts as unset, so it does not
/// mask a lower-precedence variable that is actually set.
fn resolve_locale(
    lc_all: Option<String>,
    lc_ctype: Option<String>,
    lang: Option<String>,
) -> String {
    [lc_all, lc_ctype, lang]
        .into_iter()
        .flatten()
        .find(|v| !v.is_empty())
        .unwrap_or_default()
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

    #[test]
    fn locale_uses_lc_ctype_when_lc_all_unset() {
        // LC_CTYPE alone drives the locale (LC_ALL unset, LANG unset).
        let locale = resolve_locale(None, Some("en_US.UTF-8".to_string()), None);
        assert_eq!(locale, "en_US.UTF-8");
    }

    #[test]
    fn locale_treats_empty_lc_all_as_unset() {
        // An empty LC_ALL must not mask a set LC_CTYPE.
        let locale = resolve_locale(
            Some(String::new()),
            Some("C.UTF-8".to_string()),
            Some("C".to_string()),
        );
        assert_eq!(locale, "C.UTF-8");
    }

    #[test]
    fn locale_prefers_lc_all_over_others() {
        let locale = resolve_locale(
            Some("C".to_string()),
            Some("en_US.UTF-8".to_string()),
            Some("en_US.UTF-8".to_string()),
        );
        assert_eq!(locale, "C");
    }

    #[test]
    fn locale_falls_back_to_lang() {
        let locale = resolve_locale(None, None, Some("en_US.UTF-8".to_string()));
        assert_eq!(locale, "en_US.UTF-8");
    }
}
