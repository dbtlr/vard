//! XDG base-directory resolution for vard's config, state, data, and log
//! locations. The layout is identical on macOS and Linux — vard uses XDG
//! paths on both platforms rather than `~/Library` on macOS.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// `$XDG_CONFIG_HOME/vard/config.toml`, default `~/.config/vard/config.toml`.
pub fn config_file() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("config.toml")
}

/// `$XDG_STATE_HOME/vard`, default `~/.local/state/vard` — health file,
/// request queue, locks.
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// `$XDG_DATA_HOME/vard`, default `~/.local/share/vard` — update staging,
/// metadata.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// `<state_dir>/logs` — vard's own rotated logfile.
pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}

fn xdg_dir(var: &str, default_rel: &str) -> PathBuf {
    resolve(env::var_os(var), &home(), default_rel)
}

/// Pure resolution core: an absolute `$var` value wins; a relative or unset
/// value falls back to `<home>/<default_rel>`, per the XDG spec's rule that
/// relative base-directory values must be ignored.
fn resolve(var_value: Option<OsString>, home: &Path, default_rel: &str) -> PathBuf {
    var_value
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(default_rel))
        .join("vard")
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME must be set")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn absolute_xdg_value_wins() {
        let dir = resolve(os("/custom/xdg"), Path::new("/home/u"), ".config");
        assert_eq!(dir, PathBuf::from("/custom/xdg/vard"));
    }

    #[test]
    fn unset_falls_back_to_home_default() {
        let dir = resolve(None, Path::new("/home/u"), ".local/state");
        assert_eq!(dir, PathBuf::from("/home/u/.local/state/vard"));
    }

    #[test]
    fn relative_xdg_value_is_ignored() {
        let dir = resolve(os("relative/xdg"), Path::new("/home/u"), ".config");
        assert_eq!(dir, PathBuf::from("/home/u/.config/vard"));
    }

    #[test]
    fn empty_xdg_value_is_ignored() {
        let dir = resolve(os(""), Path::new("/home/u"), ".config");
        assert_eq!(dir, PathBuf::from("/home/u/.config/vard"));
    }
}
