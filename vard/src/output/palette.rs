//! Semantic color palette for `vard` output.
//!
//! [`Palette`] maps a small set of semantic tokens (foreground, accent,
//! severities, muted) to `anstyle::Style` values. Use [`resolve`] to get a
//! palette calibrated to the current environment: TTY detection plus the
//! `NO_COLOR` and `CLICOLOR_FORCE` conventions.
//!
//! The token set and ANSI-256 hues are borrowed from norn's proven output
//! layer; they are reasonable defaults and can be rebranded later without
//! changing the resolution logic or the primitives that consume them.

use std::env;
use std::io::IsTerminal;

use anstyle::{Ansi256Color, Color, Style};

use crate::cli::ColorWhen;

/// Semantic color palette.
///
/// Every field is an `anstyle::Style`. When color is disabled (`enabled ==
/// false`) every style is `Style::new()` (a no-op). When color is enabled the
/// styles carry ANSI-256 color codes.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Default foreground — a near-white ANSI-256 tone, distinct from `dim`.
    pub fg: Style,
    /// Accent / interactive tokens (ANSI 256 color 67, muted blue).
    pub accent: Style,
    /// Success (ANSI 256 color 108, muted green).
    pub success: Style,
    /// Warning (ANSI 256 color 178, amber).
    pub warning: Style,
    /// Error (ANSI 256 color 167, muted red).
    pub error: Style,
    /// Muted secondary text — ANSI 256 #244 (medium gray).
    pub dim: Style,
    /// Field labels (= `dim`).
    pub label: Style,
    /// Record-block header — foreground, bold.
    pub header: Style,
    /// Section headers (= `dim` bold).
    pub section: Style,
    /// Whether color output is enabled. Lets a caller branch on color state to
    /// append textual signals that color would otherwise carry. Consumed by the
    /// read/list commands (VRD-15+); unread until they land, so `allow`ed here
    /// alongside [`Palette::is_off`].
    #[allow(dead_code)]
    pub enabled: bool,
}

const fn ansi256(n: u8) -> Style {
    Style::new().fg_color(Some(Color::Ansi256(Ansi256Color(n))))
}

impl Palette {
    /// Returns a palette with no styling — every field is `Style::new()`.
    pub const fn off() -> Self {
        Self {
            fg: Style::new(),
            accent: Style::new(),
            success: Style::new(),
            warning: Style::new(),
            error: Style::new(),
            dim: Style::new(),
            label: Style::new(),
            header: Style::new(),
            section: Style::new(),
            enabled: false,
        }
    }

    /// Returns `true` when this palette has all styles disabled (no-color path).
    /// Color-state accessor for the read/list commands (VRD-15+); unused until
    /// they land (see [`Palette::enabled`]).
    #[allow(dead_code)]
    pub const fn is_off(&self) -> bool {
        !self.enabled
    }

    /// Returns the full semantic palette with ANSI-256 colors applied.
    pub const fn on() -> Self {
        // `dim` and `fg` ship as explicit ANSI-256 colors, NOT as SGR effects,
        // because many terminals (macOS Terminal.app default profile, several
        // tmux configs) silently ignore SGR 2 ("faint") and render the text as
        // the terminal default — defeating the visual distinction between the
        // foreground and muted tones. Explicit 256-color codes are portable.
        let dim = ansi256(244);
        let fg = ansi256(253);
        Self {
            fg,
            accent: ansi256(67),
            success: ansi256(108),
            warning: ansi256(178),
            error: ansi256(167),
            dim,
            label: dim,
            header: fg.bold(),
            section: dim.bold(),
            enabled: true,
        }
    }
}

/// Resolve a [`Palette`] for the given `ColorWhen` setting.
///
/// Reads `NO_COLOR` and `CLICOLOR_FORCE` from the environment and detects
/// whether stdout is a TTY, then delegates to [`resolve_inner`].
/// Convenience wrapper that queries the terminal itself. The help emitter uses
/// [`resolve_with_tty`] to avoid a second isatty probe; the record/list commands
/// (VRD-15+) are the intended callers here, so it is `allow`ed until they land.
#[allow(dead_code)]
pub fn resolve(when: ColorWhen) -> Palette {
    let is_tty = std::io::stdout().is_terminal();
    resolve_with_tty(when, is_tty)
}

/// Like [`resolve`], but takes a pre-resolved `is_tty` so a caller that has
/// already queried the terminal once (e.g. the help emitter) does not probe it
/// again.
pub fn resolve_with_tty(when: ColorWhen, is_tty: bool) -> Palette {
    let no_color = env::var_os("NO_COLOR").is_some();
    let force = env::var_os("CLICOLOR_FORCE").is_some();
    resolve_inner(when, no_color, force, is_tty)
}

/// Inner resolution logic — separated for testability.
///
/// `no_color`: `NO_COLOR` env var is set.
/// `force`: `CLICOLOR_FORCE` env var is set.
/// `is_tty`: stdout is a terminal.
pub(crate) fn resolve_inner(when: ColorWhen, no_color: bool, force: bool, is_tty: bool) -> Palette {
    // NO_COLOR takes precedence over everything, including `--color always`.
    // See https://no-color.org/
    if no_color {
        return Palette::off();
    }
    match when {
        ColorWhen::Always => Palette::on(),
        ColorWhen::Never => Palette::off(),
        ColorWhen::Auto => {
            if force || is_tty {
                Palette::on()
            } else {
                Palette::off()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_has_zero_styles_and_disabled_flag() {
        let p = Palette::off();
        assert!(!p.enabled);
        assert_eq!(format!("{}", p.accent.render()), "");
        assert_eq!(format!("{}", p.error.render()), "");
    }

    #[test]
    fn on_severity_styles_render_ansi() {
        let p = Palette::on();
        assert!(p.enabled);
        assert_ne!(format!("{}", p.success.render()), "");
        assert_ne!(format!("{}", p.warning.render()), "");
        assert_ne!(format!("{}", p.error.render()), "");
        assert_ne!(format!("{}", p.accent.render()), "");
    }

    #[test]
    fn resolve_always_without_no_color_returns_on() {
        assert!(resolve_inner(ColorWhen::Always, false, false, false).enabled);
    }

    #[test]
    fn resolve_always_with_no_color_returns_off() {
        // NO_COLOR takes precedence over `--color always` per https://no-color.org/
        assert!(!resolve_inner(ColorWhen::Always, true, false, false).enabled);
    }

    #[test]
    fn resolve_never_returns_off() {
        assert!(!resolve_inner(ColorWhen::Never, false, false, true).enabled);
    }

    #[test]
    fn resolve_inner_no_color_env_forces_off() {
        assert!(!resolve_inner(ColorWhen::Auto, true, false, true).enabled);
    }

    #[test]
    fn resolve_inner_clicolor_force_overrides_no_tty() {
        assert!(resolve_inner(ColorWhen::Auto, false, true, false).enabled);
    }

    #[test]
    fn resolve_inner_auto_with_tty_returns_on() {
        assert!(resolve_inner(ColorWhen::Auto, false, false, true).enabled);
    }

    #[test]
    fn resolve_inner_auto_without_tty_returns_off() {
        assert!(!resolve_inner(ColorWhen::Auto, false, false, false).enabled);
    }
}
