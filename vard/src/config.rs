//! The binary's file-config layer: parse `config.toml`, apply defaults
//! inheritance, and hand the engine validated [`WatchSpec`] values.
//!
//! Presentation and file I/O are host concerns, so they live here rather than
//! in `vard-core`. This module reads and parses the TOML schema (spec §12) and
//! resolves it against the core default constants; the engine never sees a
//! file. Correctness of each resolved watch is still owned by core — resolution
//! funnels every watch through [`WatchSpec::builder`], which validates.
//!
//! # Forward compatibility
//!
//! Deserialization tolerates unknown keys and sections (no `deny_unknown_fields`).
//! Future typed sections (`[ai]`, `[update]`, per-watch `[watch.hooks]`, …) land
//! in later tasks and must parse against today's binary without error; the
//! top-level `version` key is the migration lever if a breaking change is ever
//! required.
//!
//! Durations are humantime strings (`"15m"`), deserialized through
//! [`vard_core::parse_duration`] so the file layer and the SDK share one parser.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use vard_core::{TriggerMode, WatchSpec};

use crate::paths;

/// The only config schema version this binary understands.
const SUPPORTED_VERSION: u32 = 1;

/// A parsed `config.toml`. Unknown keys and sections are ignored (see the
/// [module docs](self)); only the fields modeled below are read.
#[derive(Debug, Deserialize)]
pub(crate) struct Config {
    /// Schema version. Must equal [`SUPPORTED_VERSION`]; checked in
    /// [`from_toml_str`](Config::from_toml_str).
    pub version: u32,
    #[serde(default)]
    pub daemon: Daemon,
    #[serde(default)]
    pub defaults: Defaults,
    /// Watches, one per `[[watch]]` table.
    #[serde(default, rename = "watch")]
    pub watches: Vec<WatchConfig>,
}

/// The `[daemon]` section.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub(crate) struct Daemon {
    pub log_level: String,
    pub log_retention_days: u32,
}

impl Default for Daemon {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            log_retention_days: 14,
        }
    }
}

/// The `[defaults]` section: values inherited by any watch that does not set
/// the corresponding field. Every field is optional; an absent field falls
/// through to the core `DEFAULT_*` constant during resolution.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Defaults {
    pub trigger: Option<String>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub interval: Option<Duration>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub quiesce: Option<Duration>,
    pub sync: Option<bool>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub sync_interval: Option<Duration>,
}

/// One `[[watch]]` table. `name` and `path` are required; every other field is
/// optional and inherits from `[defaults]` then the core constants.
#[derive(Debug, Deserialize)]
pub(crate) struct WatchConfig {
    pub name: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub remote: Option<String>,
    pub trigger: Option<String>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub interval: Option<Duration>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub quiesce: Option<Duration>,
    pub sync: Option<bool>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub sync_interval: Option<Duration>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Config {
    /// The default config path, `$XDG_CONFIG_HOME/vard/config.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        paths::config_file().map_err(|e| ConfigError::Path(e.to_string()))
    }

    /// Reads and parses the config file at `path`. Does not watch for changes
    /// or hot-reload — that is VRD-14.
    pub fn load(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::from_toml_str(&text)
    }

    /// Parses `text` as TOML and rejects an unsupported schema version.
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if config.version != SUPPORTED_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: config.version,
            });
        }
        Ok(config)
    }

    /// Resolves the config into validated [`WatchSpec`]s.
    ///
    /// Each field is resolved watch value > `[defaults]` > core constant, then
    /// the watch is built through [`WatchSpec::builder`], which enforces core's
    /// invariants. Duplicate watch names are rejected. Any error names the
    /// offending watch.
    pub fn resolve(&self) -> Result<Vec<WatchSpec>, ConfigError> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut specs = Vec::with_capacity(self.watches.len());

        for watch in &self.watches {
            if !seen.insert(watch.name.as_str()) {
                return Err(ConfigError::DuplicateWatch {
                    name: watch.name.clone(),
                });
            }

            let mut builder = WatchSpec::builder(&watch.name, &watch.path);

            // trigger: watch > defaults > core default (the builder's preset).
            if let Some(raw) = watch
                .trigger
                .as_deref()
                .or(self.defaults.trigger.as_deref())
            {
                let mode = raw.parse::<TriggerMode>().map_err(|e| ConfigError::Watch {
                    name: watch.name.clone(),
                    source: e,
                })?;
                builder = builder.trigger(mode);
            }
            if let Some(quiesce) = watch.quiesce.or(self.defaults.quiesce) {
                builder = builder.quiesce(quiesce);
            }
            if let Some(interval) = watch.interval.or(self.defaults.interval) {
                builder = builder.interval(interval);
            }
            if let Some(sync) = watch.sync.or(self.defaults.sync) {
                builder = builder.sync(sync);
            }
            if let Some(sync_interval) = watch.sync_interval.or(self.defaults.sync_interval) {
                builder = builder.sync_interval(sync_interval);
            }
            if let Some(branch) = &watch.branch {
                builder = builder.branch(branch);
            }
            if let Some(remote) = &watch.remote {
                builder = builder.remote(remote);
            }
            if !watch.exclude.is_empty() {
                builder = builder.exclude(watch.exclude.clone());
            }

            let spec = builder.build().map_err(|e| ConfigError::Watch {
                name: watch.name.clone(),
                source: e,
            })?;
            specs.push(spec);
        }

        Ok(specs)
    }
}

/// Serde helpers for humantime duration fields, delegating to
/// [`vard_core::parse_duration`] so parsing has one source of truth.
mod de {
    use super::Duration;
    use serde::{Deserialize, Deserializer, de::Error};

    /// Deserializes an optional humantime string into an optional [`Duration`].
    /// An absent key yields `None` (via `#[serde(default)]` on the field); a
    /// present but unparseable value is a deserialization error.
    pub(super) fn opt_duration<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Option::<String>::deserialize(deserializer)?;
        raw.map(|s| vard_core::parse_duration(&s).map_err(D::Error::custom))
            .transpose()
    }
}

/// Everything that can go wrong loading or resolving the file config.
///
/// Wraps the underlying `std::io` and `toml` errors as strings to keep a small,
/// stable surface. Per-watch failures carry the watch name and the underlying
/// [`vard_core::ConfigError`].
#[derive(Debug)]
pub(crate) enum ConfigError {
    /// Reading the config file failed.
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The config file could not be parsed as the expected TOML schema.
    Parse(String),
    /// The config path could not be resolved (see [`paths`]).
    Path(String),
    /// The `version` key names a schema this binary does not support.
    UnsupportedVersion {
        /// The version found in the file.
        found: u32,
    },
    /// Two watches share the same name.
    DuplicateWatch {
        /// The duplicated name.
        name: String,
    },
    /// A watch failed core validation during resolution.
    Watch {
        /// The offending watch's name.
        name: String,
        /// The core error explaining the failure.
        source: vard_core::ConfigError,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(f, "reading config {}: {source}", path.display())
            }
            ConfigError::Parse(msg) => write!(f, "parsing config: {msg}"),
            ConfigError::Path(msg) => write!(f, "resolving config path: {msg}"),
            ConfigError::UnsupportedVersion { found } => write!(
                f,
                "unsupported config version {found}; this build supports version {SUPPORTED_VERSION}"
            ),
            ConfigError::DuplicateWatch { name } => {
                write!(f, "duplicate watch name {name:?}")
            }
            ConfigError::Watch { name, source } => {
                write!(f, "watch {name:?}: {source}")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            ConfigError::Watch { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vard_core::{
        DEFAULT_INTERVAL, DEFAULT_QUIESCE, DEFAULT_REMOTE, DEFAULT_SYNC, DEFAULT_SYNC_INTERVAL,
        DEFAULT_TRIGGER,
    };

    /// The spec §12 example config, including the future `[ai]`, `[update]`,
    /// and per-watch `[watch.hooks]` sections that this build does not yet
    /// model — their presence proves unknown-section tolerance.
    const SPEC_EXAMPLE: &str = r#"
version = 1

[daemon]
log_level = "debug"
log_retention_days = 30

[defaults]
trigger = "both"
interval = "15m"
quiesce = "10s"
sync = true
sync_interval = "20m"

[[watch]]
name = "notes"
path = "/home/u/notes"

[[watch]]
name = "project"
path = "/home/u/project"
trigger = "events"
interval = "5m"
quiesce = "3s"
sync = false
sync_interval = "1h"
branch = "vard-backup"
remote = "backup"
exclude = ["target", "*.log"]

[watch.hooks]
post_snapshot = "notify-send snapshot taken"

[ai]
enabled = true
model = "claude"

[update]
channel = "stable"
"#;

    #[test]
    fn parses_spec_example_including_unknown_sections() {
        let config = Config::from_toml_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.daemon.log_level, "debug");
        assert_eq!(config.daemon.log_retention_days, 30);
        assert_eq!(config.watches.len(), 2);
        // Resolution succeeds despite [ai], [update], and [watch.hooks].
        let specs = config.resolve().unwrap();
        assert_eq!(specs.len(), 2);
    }

    #[test]
    fn daemon_defaults_apply_when_section_absent() {
        let config = Config::from_toml_str("version = 1\n").unwrap();
        assert_eq!(config.daemon.log_level, "info");
        assert_eq!(config.daemon.log_retention_days, 14);
    }

    #[test]
    fn watch_without_overrides_inherits_defaults_section() {
        // [defaults] values chosen to differ from the core constants so the
        // inheritance layer is distinguishable from the constant layer.
        let config = Config::from_toml_str(
            r#"
version = 1

[defaults]
trigger = "interval"
interval = "30m"
quiesce = "45s"
sync = false
sync_interval = "2h"

[[watch]]
name = "plain"
path = "/data/plain"
"#,
        )
        .unwrap();
        let specs = config.resolve().unwrap();
        let spec = &specs[0];
        assert_eq!(spec.trigger, TriggerMode::Interval);
        assert_eq!(spec.interval, Duration::from_secs(30 * 60));
        assert_eq!(spec.quiesce, Duration::from_secs(45));
        assert!(!spec.sync);
        assert_eq!(spec.sync_interval, Duration::from_secs(2 * 3600));
        // Fields with no [defaults] entry still fall to core values.
        assert_eq!(spec.remote, DEFAULT_REMOTE);
        assert_eq!(spec.branch, None);
        assert!(spec.exclude.is_empty());
    }

    #[test]
    fn watch_override_wins_over_defaults() {
        let config = Config::from_toml_str(
            r#"
version = 1

[defaults]
trigger = "interval"
interval = "30m"
quiesce = "45s"
sync = false
sync_interval = "2h"

[[watch]]
name = "custom"
path = "/data/custom"
trigger = "events"
interval = "5m"
quiesce = "3s"
sync = true
sync_interval = "90m"
branch = "backup"
remote = "mirror"
exclude = ["target"]
"#,
        )
        .unwrap();
        let spec = &config.resolve().unwrap()[0];
        assert_eq!(spec.trigger, TriggerMode::Events);
        assert_eq!(spec.interval, Duration::from_secs(5 * 60));
        assert_eq!(spec.quiesce, Duration::from_secs(3));
        assert!(spec.sync);
        assert_eq!(spec.sync_interval, Duration::from_secs(90 * 60));
        assert_eq!(spec.branch.as_deref(), Some("backup"));
        assert_eq!(spec.remote, "mirror");
        assert_eq!(spec.exclude, vec!["target".to_string()]);
    }

    #[test]
    fn watch_falls_back_to_core_constants_without_defaults() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "bare"
path = "/data/bare"
"#,
        )
        .unwrap();
        let spec = &config.resolve().unwrap()[0];
        assert_eq!(spec.trigger, DEFAULT_TRIGGER);
        assert_eq!(spec.interval, DEFAULT_INTERVAL);
        assert_eq!(spec.quiesce, DEFAULT_QUIESCE);
        assert_eq!(spec.sync, DEFAULT_SYNC);
        assert_eq!(spec.sync_interval, DEFAULT_SYNC_INTERVAL);
        assert_eq!(spec.remote, DEFAULT_REMOTE);
    }

    #[test]
    fn sync_false_watch_resolves_with_sync_off() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "local-only"
path = "/data/local"
sync = false
"#,
        )
        .unwrap();
        let spec = &config.resolve().unwrap()[0];
        assert!(!spec.sync);
        // sync_interval is still validated (> 0) even with sync off.
        assert_eq!(spec.sync_interval, DEFAULT_SYNC_INTERVAL);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        match Config::from_toml_str("version = 2\n") {
            Err(ConfigError::UnsupportedVersion { found }) => assert_eq!(found, 2),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_watch_names_are_rejected() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "dup"
path = "/a"

[[watch]]
name = "dup"
path = "/b"
"#,
        )
        .unwrap();
        match config.resolve() {
            Err(ConfigError::DuplicateWatch { name }) => assert_eq!(name, "dup"),
            other => panic!("expected DuplicateWatch, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_fields_are_rejected_with_useful_errors() {
        // Missing path.
        let err = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "no-path"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
        assert!(err.to_string().contains("path"), "got: {err}");

        // Missing name.
        let err = Config::from_toml_str(
            r#"
version = 1

[[watch]]
path = "/somewhere"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
        assert!(err.to_string().contains("name"), "got: {err}");
    }

    #[test]
    fn unknown_trigger_is_rejected_naming_the_watch() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "weird"
path = "/data"
trigger = "sometimes"
"#,
        )
        .unwrap();
        match config.resolve() {
            Err(ConfigError::Watch { name, .. }) => assert_eq!(name, "weird"),
            other => panic!("expected Watch error, got {other:?}"),
        }
    }

    #[test]
    fn load_reads_and_parses_a_file() {
        let dir = std::env::temp_dir().join(format!("vard-cfg-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, SPEC_EXAMPLE).unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.resolve().unwrap().len(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_is_an_io_error() {
        let path = std::env::temp_dir().join("vard-cfg-does-not-exist-xyz.toml");
        match Config::load(&path) {
            Err(ConfigError::Io { .. }) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn invalid_duration_string_is_a_parse_error() {
        let err = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "w"
path = "/data"
interval = "soon"
"#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got: {err:?}");
    }
}
