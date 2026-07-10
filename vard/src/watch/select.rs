//! Resolving a `<name|path>` selector to a watch (spec §12: path identity).
//!
//! `vard watch remove|pause|resume` accept either a watch's stable name or the
//! directory it covers. Names match case-insensitively. Paths match by
//! *canonical* identity — symlinks resolved — so `./notes`, `~/notes`, and a
//! symlink into it all select the same watch. When the directory has been moved
//! or deleted, canonicalization is impossible, so a name match (which needs no
//! filesystem) still resolves the watch — the moved-directory case the spec
//! calls out.

use std::path::{Path, PathBuf};

use crate::config::{Config, expand_tilde};

/// Why a selector did not resolve to exactly one watch.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SelectError {
    /// No watch matched the selector by name or by path.
    NotFound {
        /// The selector as typed.
        selector: String,
    },
    /// The selector matched more than one watch by path — an ambiguous request.
    Ambiguous {
        /// The selector as typed.
        selector: String,
        /// The names of the watches it matched.
        names: Vec<String>,
    },
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectError::NotFound { selector } => {
                write!(f, "no watch named or rooted at {selector:?}")
            }
            SelectError::Ambiguous { selector, names } => {
                write!(
                    f,
                    "selector {selector:?} matches multiple watches ({}); select by name",
                    names.join(", ")
                )
            }
        }
    }
}

/// Resolves `selector` to the index of the matching watch in `config.watches`.
///
/// A name match wins outright (names are unique in a valid config). Failing
/// that, the selector is treated as a path and compared by canonical identity,
/// with a textual fallback for paths that cannot be canonicalized.
pub(crate) fn select_watch(config: &Config, selector: &str) -> Result<usize, SelectError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    select_watch_with_home(config, selector, home.as_deref())
}

/// [`select_watch`] with an explicit home directory for tilde expansion, so
/// tests need not read the process environment.
pub(crate) fn select_watch_with_home(
    config: &Config,
    selector: &str,
    home: Option<&Path>,
) -> Result<usize, SelectError> {
    // 1. Name match (case-insensitive). No filesystem access, so it works even
    //    for a watch whose directory has moved or been removed.
    if let Some(i) = config
        .watches
        .iter()
        .position(|w| w.name.eq_ignore_ascii_case(selector))
    {
        return Ok(i);
    }

    // 2. Path match by canonical identity, with a textual fallback.
    let selector_path = Path::new(selector);
    let selector_canon = std::fs::canonicalize(selector_path).ok();

    let matches: Vec<usize> = config
        .watches
        .iter()
        .enumerate()
        .filter(|(_, w)| {
            let expanded = expand_tilde(&w.path, home).unwrap_or_else(|| w.path.clone());
            path_matches(&expanded, selector_path, selector_canon.as_deref())
        })
        .map(|(i, _)| i)
        .collect();

    match matches.as_slice() {
        [i] => Ok(*i),
        [] => Err(SelectError::NotFound {
            selector: selector.to_string(),
        }),
        many => Err(SelectError::Ambiguous {
            selector: selector.to_string(),
            names: many
                .iter()
                .map(|&i| config.watches[i].name.clone())
                .collect(),
        }),
    }
}

/// Whether a watch's (tilde-expanded) configured path identifies the same
/// directory as the selector: equal canonical paths when both canonicalize,
/// else a textual equality fallback for paths that no longer exist on disk.
fn path_matches(watch_path: &Path, selector: &Path, selector_canon: Option<&Path>) -> bool {
    match (std::fs::canonicalize(watch_path).ok(), selector_canon) {
        (Some(a), Some(b)) => a == b,
        // One side (or both) cannot be canonicalized — a moved/removed dir.
        // Fall back to exact textual equality of the expanded paths.
        _ => watch_path == selector,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(watches: &[(&str, &str)]) -> Config {
        let mut toml = String::from("version = 1\n");
        for (name, path) in watches {
            toml.push_str(&format!(
                "\n[[watch]]\nname = \"{name}\"\npath = \"{path}\"\n"
            ));
        }
        Config::from_toml_str(&toml).unwrap()
    }

    #[test]
    fn selects_by_exact_name() {
        let config = config_with(&[("notes", "/data/notes"), ("proj", "/data/proj")]);
        assert_eq!(select_watch_with_home(&config, "proj", None), Ok(1));
    }

    #[test]
    fn selects_by_name_case_insensitively() {
        let config = config_with(&[("Notes", "/data/notes")]);
        assert_eq!(select_watch_with_home(&config, "notes", None), Ok(0));
    }

    #[test]
    fn selects_by_textual_path_when_dir_is_gone() {
        // Neither path exists on disk, so canonicalization fails and the textual
        // fallback resolves the watch — the moved/removed directory case.
        let config = config_with(&[("a", "/nonexistent/aaa"), ("b", "/nonexistent/bbb")]);
        assert_eq!(
            select_watch_with_home(&config, "/nonexistent/bbb", None),
            Ok(1)
        );
    }

    #[test]
    fn selects_by_canonical_path_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Watch stored under the canonical (real) path; selector via the symlink.
        let real_str = real.to_string_lossy();
        let config = config_with(&[("w", &real_str)]);
        assert_eq!(
            select_watch_with_home(&config, &link.to_string_lossy(), None),
            Ok(0)
        );
    }

    #[test]
    fn tilde_selector_expands_against_home() {
        let home = Path::new("/nonexistent/home");
        let config = config_with(&[("dots", "~/dotfiles")]);
        // Both expand to /nonexistent/home/dotfiles; neither exists, so the
        // textual fallback matches.
        assert_eq!(
            select_watch_with_home(&config, "/nonexistent/home/dotfiles", Some(home)),
            Ok(0)
        );
    }

    #[test]
    fn unknown_selector_is_not_found() {
        let config = config_with(&[("notes", "/data/notes")]);
        match select_watch_with_home(&config, "ghost", None) {
            Err(SelectError::NotFound { selector }) => assert_eq!(selector, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
