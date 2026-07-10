//! XDG base-directory resolution for vard's config, state, data, and log
//! locations. The layout is identical on macOS and Linux — vard uses XDG
//! paths on both platforms rather than `~/Library` on macOS.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// HOME is not set to an absolute path and no XDG base directory override
/// covers the requested location.
#[derive(Debug)]
pub struct HomeNotFound;

impl fmt::Display for HomeNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HOME is not set to an absolute path and no XDG base directory override is set"
        )
    }
}

impl std::error::Error for HomeNotFound {}

/// `$XDG_CONFIG_HOME/vard/config.toml`, default `~/.config/vard/config.toml`.
pub fn config_file() -> Result<PathBuf, HomeNotFound> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("config.toml"))
}

/// `$XDG_STATE_HOME/vard`, default `~/.local/state/vard` — health file,
/// request queue, locks.
pub fn state_dir() -> Result<PathBuf, HomeNotFound> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// `$XDG_DATA_HOME/vard`, default `~/.local/share/vard` — update staging,
/// metadata.
// Consumed by the self-update flow (VRD-25); resolved and tested here now.
#[allow(dead_code)]
pub fn data_dir() -> Result<PathBuf, HomeNotFound> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// `<state_dir>/logs` — vard's own rotated logfile.
// Consumed when file-based log rotation lands (VRD-23); resolved and tested here now.
#[allow(dead_code)]
pub fn log_dir() -> Result<PathBuf, HomeNotFound> {
    Ok(state_dir()?.join("logs"))
}

/// `<state_dir>/vard.lock` — the flock target that enforces a single daemon
/// instance per state directory.
pub fn lock_file() -> Result<PathBuf, HomeNotFound> {
    Ok(state_dir()?.join("vard.lock"))
}

/// `<state_dir>/health` — the small structured document the daemon rewrites on
/// every watch state change and `vard notify` reads. Deliberately in the state
/// directory (not config): it is derived runtime state, not user input.
pub fn health_file() -> Result<PathBuf, HomeNotFound> {
    Ok(state_dir()?.join("health"))
}

/// `<state_dir>/requests` — the request-file queue the CLI drops into and the
/// daemon drains.
pub fn request_dir() -> Result<PathBuf, HomeNotFound> {
    Ok(state_dir()?.join("requests"))
}

/// `<state_dir>/journal` — per-watch operation journals.
pub fn journal_dir() -> Result<PathBuf, HomeNotFound> {
    Ok(state_dir()?.join("journal"))
}

fn xdg_dir(var: &str, default_rel: &str) -> Result<PathBuf, HomeNotFound> {
    resolve(env::var_os(var), home().as_deref(), default_rel).ok_or(HomeNotFound)
}

/// Pure resolution core: an absolute `$var` value wins; else an absolute
/// `home` joins `default_rel`, per the XDG spec's rule that relative
/// base-directory values must be ignored. Empty or relative values are
/// rejected on both inputs; `None` when neither is usable.
fn resolve(var_value: Option<OsString>, home: Option<&Path>, default_rel: &str) -> Option<PathBuf> {
    let xdg = var_value.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = xdg.or_else(|| {
        home.filter(|h| h.is_absolute())
            .map(|h| h.join(default_rel))
    })?;
    Some(base.join("vard"))
}

fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn absolute_xdg_value_wins() {
        let dir = resolve(os("/custom/xdg"), Some(Path::new("/home/u")), ".config");
        assert_eq!(dir, Some(PathBuf::from("/custom/xdg/vard")));
    }

    #[test]
    fn unset_falls_back_to_home_default() {
        let dir = resolve(None, Some(Path::new("/home/u")), ".local/state");
        assert_eq!(dir, Some(PathBuf::from("/home/u/.local/state/vard")));
    }

    #[test]
    fn relative_xdg_value_is_ignored() {
        let dir = resolve(os("relative/xdg"), Some(Path::new("/home/u")), ".config");
        assert_eq!(dir, Some(PathBuf::from("/home/u/.config/vard")));
    }

    #[test]
    fn empty_xdg_value_is_ignored() {
        let dir = resolve(os(""), Some(Path::new("/home/u")), ".config");
        assert_eq!(dir, Some(PathBuf::from("/home/u/.config/vard")));
    }

    #[test]
    fn unset_home_with_no_xdg_override_is_none() {
        let dir = resolve(None, None, ".local/state");
        assert_eq!(dir, None);
    }

    #[test]
    fn empty_home_with_no_xdg_override_is_none() {
        // Finding 2: an empty HOME with no XDG override must not silently
        // resolve to a CWD-relative path like ".local/state/vard".
        let dir = resolve(None, Some(Path::new("")), ".local/state");
        assert_eq!(dir, None);
    }

    #[test]
    fn relative_home_with_no_xdg_override_is_none() {
        let dir = resolve(None, Some(Path::new("relative/home")), ".local/state");
        assert_eq!(dir, None);
    }

    #[test]
    fn absolute_xdg_override_works_even_when_home_is_none() {
        let dir = resolve(os("/custom/xdg"), None, ".config");
        assert_eq!(dir, Some(PathBuf::from("/custom/xdg/vard")));
    }
}
