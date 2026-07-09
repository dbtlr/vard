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
//! The schema is a curated allowlist, not a blanket-tolerant parse: unknown
//! keys and sections are rejected with an error naming the offender, so a typo
//! (`[default]`, `qiesce`) cannot silently change behavior. The known-future
//! surface from spec §12 is explicitly tolerated as opaque values until its
//! owning tasks land typed models: top-level `[ai]` and `[update]`;
//! `secret_scan`/`hook_timeout`/`hook_rate_limit` in `[defaults]`; and
//! `secret_scan`/`hooks` in `[[watch]]`. Top-level `[daemon]` is typed
//! (VRD-14; see [`DaemonConfig`]) since it is binary-level per ADR 0003. The
//! top-level `version` key remains the migration lever for real schema
//! breaks.
//!
//! Durations are humantime strings (`"15m"`), deserialized through
//! [`vard_core::parse_duration`] so the file layer and the SDK share one parser.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use vard_core::{TriggerMode, WatchSpec};

use crate::paths;

/// The only config schema version this binary understands.
const SUPPORTED_VERSION: i64 = 1;

/// Default `[daemon].log_level`, absent an explicit value.
///
/// `[daemon]` is a binary-level concern (ADR 0003), so — unlike the
/// `vard-core` `DEFAULT_*` constants `[defaults]` resolves against — this
/// default lives in the binary crate rather than in `vard-core`.
const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;

/// Default `[daemon].log_retention_days`, absent an explicit value.
const DEFAULT_LOG_RETENTION_DAYS: u32 = 14;

/// A parsed `config.toml`. Unknown keys are rejected; the known-future
/// sections are carried opaquely (see the [module docs](self)).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Schema version. Read via the pre-parse probe in
    /// [`from_toml_str`](Config::from_toml_str) rather than this field, which is
    /// retained so the schema round-trips and tests can assert it.
    #[allow(dead_code)]
    pub version: i64,
    /// The `[daemon]` section (spec §12). Absent entirely, or with only
    /// some fields set, defaults fill in the rest — see [`DaemonConfig`].
    #[serde(default)]
    pub daemon: DaemonConfig,
    /// Tolerated opaquely; a later task adds the typed `[ai]` section.
    #[allow(dead_code)]
    ai: Option<toml::Value>,
    /// Tolerated opaquely; a later task adds the typed `[update]` section.
    #[allow(dead_code)]
    update: Option<toml::Value>,
    #[serde(default)]
    pub defaults: Defaults,
    /// Watches, one per `[[watch]]` table.
    #[serde(default, rename = "watch")]
    pub watches: Vec<WatchConfig>,
}

/// The `[daemon]` section (spec §12): binary-level daemon behavior — the
/// daemon's own log verbosity and how long it retains its rotated logs.
/// Typed here rather than in `vard-core` because `[daemon]` is a binary
/// concern, not an engine one (ADR 0003).
///
/// Every field is optional in the file; a missing `[daemon]` section
/// entirely, or a section with only some fields set, defaults the rest via
/// the container-level `#[serde(default)]` below, which falls back to
/// [`Default for DaemonConfig`](DaemonConfig#impl-Default-for-DaemonConfig).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct DaemonConfig {
    pub log_level: LogLevel,
    pub log_retention_days: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            log_level: DEFAULT_LOG_LEVEL,
            log_retention_days: DEFAULT_LOG_RETENTION_DAYS,
        }
    }
}

/// The daemon's own log verbosity (`[daemon].log_level`).
///
/// A validated enum, not a free string: the file must spell one of the
/// lowercase variants below, or deserialization fails with a clean,
/// span-bearing serde error naming the offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// The `[defaults]` section: values inherited by any watch that does not set
/// the corresponding field. Every field is optional; an absent field falls
/// through to the core `DEFAULT_*` constant during resolution.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Defaults {
    pub trigger: Option<String>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub interval: Option<Duration>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub quiesce: Option<Duration>,
    pub sync: Option<bool>,
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub sync_interval: Option<Duration>,
    /// Tolerated opaquely for the known-future spec §12 surface.
    #[allow(dead_code)]
    secret_scan: Option<toml::Value>,
    /// Tolerated opaquely for the known-future spec §12 surface.
    #[allow(dead_code)]
    hook_timeout: Option<toml::Value>,
    /// Tolerated opaquely for the known-future spec §12 surface.
    #[allow(dead_code)]
    hook_rate_limit: Option<toml::Value>,
}

/// One `[[watch]]` table. `name` and `path` are required. Of the optional
/// fields, exactly five inherit from `[defaults]` before falling back to the
/// core constants: `trigger`, `interval`, `quiesce`, `sync`, and
/// `sync_interval`. `branch`, `remote`, `exclude`, and `poll_interval` have no
/// `[defaults]` home and fall back to the core defaults directly.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Forces the filesystem watcher to poll at this period instead of using
    /// native events. Deliberately per-watch with no `[defaults]` counterpart:
    /// polling compensates for one path's filesystem (a network mount, a
    /// container bind), not a fleet-wide preference.
    #[serde(default, deserialize_with = "de::opt_duration")]
    pub poll_interval: Option<Duration>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Tolerated opaquely for the known-future spec §12 surface.
    #[allow(dead_code)]
    secret_scan: Option<toml::Value>,
    /// Tolerated opaquely; a later task adds typed `[watch.hooks]`.
    #[allow(dead_code)]
    hooks: Option<toml::Value>,
}

impl Config {
    /// The default config path, `$XDG_CONFIG_HOME/vard/config.toml`.
    // The daemon resolves paths via `DaemonPaths`; kept for future CLI callers
    // (e.g. `vard config path`).
    #[allow(dead_code)]
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

    /// Parses `text` as TOML, gating on the schema version *before* the full
    /// schema is applied.
    ///
    /// The version is probed from a generic [`toml::Value`] first, so a config
    /// written for a future schema fails with a clear version error rather
    /// than an incidental "missing field" from today's schema; a missing
    /// `version` key tells the user to add `version = 1`. Only then is the
    /// full schema deserialized (a second, span-preserving parse of the same
    /// small text — deserializing from the probed `Value` would lose TOML
    /// line/column error spans).
    pub fn from_toml_str(text: &str) -> Result<Config, ConfigError> {
        let probe: toml::Value =
            toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let version = probe.get("version").ok_or(ConfigError::MissingVersion)?;
        let version = version
            .as_integer()
            .ok_or_else(|| ConfigError::Parse("`version` must be an integer".to_string()))?;
        if version != SUPPORTED_VERSION {
            return Err(ConfigError::UnsupportedVersion { found: version });
        }

        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Resolves the config into validated [`WatchSpec`]s.
    ///
    /// Each of `trigger`, `interval`, `quiesce`, `sync`, and `sync_interval`
    /// is resolved watch value > `[defaults]` > core constant, then the watch
    /// is built through [`WatchSpec::builder`], which enforces core's
    /// invariants. A leading `~` in a watch path is expanded against `$HOME`.
    /// Duplicate watch names (compared case-insensitively — state files
    /// collide on case-insensitive filesystems) and duplicate expanded paths
    /// are rejected.
    ///
    /// Resolution-stage errors name the offending watch (or `[defaults]`);
    /// malformed durations and type errors surface earlier, at parse time,
    /// with TOML line/column spans instead.
    pub fn resolve(&self) -> Result<Vec<WatchSpec>, ConfigError> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.resolve_with_home(home.as_deref())
    }

    /// [`resolve`](Self::resolve) with an explicit home directory, so tests
    /// need not mutate the process environment.
    fn resolve_with_home(&self, home: Option<&Path>) -> Result<Vec<WatchSpec>, ConfigError> {
        // Parse [defaults].trigger once, up front: an error there belongs to
        // [defaults], not to whichever watch happens to inherit it first.
        let default_trigger = self
            .defaults
            .trigger
            .as_deref()
            .map(str::parse::<TriggerMode>)
            .transpose()
            .map_err(|e| ConfigError::Defaults { source: e })?;

        let mut seen_names: HashSet<String> = HashSet::new();
        // Expanded path -> name of the first watch using it. Textual equality
        // only: catching canonicalization-level collisions (symlinks, case
        // folding) is the daemon's job at registration time.
        let mut seen_paths: HashMap<PathBuf, String> = HashMap::new();
        let mut specs = Vec::with_capacity(self.watches.len());

        for watch in &self.watches {
            if !seen_names.insert(watch.name.to_lowercase()) {
                return Err(ConfigError::DuplicateWatch {
                    name: watch.name.clone(),
                });
            }

            let path = expand_tilde(&watch.path, home).ok_or_else(|| ConfigError::HomeUnset {
                name: watch.name.clone(),
            })?;
            if let Some(other) = seen_paths.insert(path.clone(), watch.name.clone()) {
                return Err(ConfigError::DuplicatePath {
                    name: watch.name.clone(),
                    other,
                    path,
                });
            }

            let mut builder = WatchSpec::builder(&watch.name, &path);

            // trigger: watch > defaults > core default (the builder's preset).
            let trigger = match watch.trigger.as_deref() {
                Some(raw) => Some(raw.parse::<TriggerMode>().map_err(|e| ConfigError::Watch {
                    name: watch.name.clone(),
                    source: e,
                })?),
                None => default_trigger,
            };
            if let Some(mode) = trigger {
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
            if let Some(poll_interval) = watch.poll_interval {
                builder = builder.poll_interval(poll_interval);
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

/// Expands a leading `~/` (or a bare `~`) against `home`. Any other path —
/// including `~user` forms and non-UTF-8 paths — passes through unchanged.
/// Returns `None` only when expansion is *needed* but `home` is unset.
///
/// Only textual expansion happens here; canonicalization and symlink
/// resolution stay a registration/daemon concern.
fn expand_tilde(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let Some(s) = path.to_str() else {
        return Some(path.to_path_buf());
    };
    if s == "~" {
        home.map(Path::to_path_buf)
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.map(|h| h.join(rest))
    } else {
        Some(path.to_path_buf())
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
    /// The config has no `version` key.
    MissingVersion,
    /// The `version` key names a schema this binary does not support.
    UnsupportedVersion {
        /// The version found in the file.
        found: i64,
    },
    /// Two watches share the same name (compared case-insensitively).
    DuplicateWatch {
        /// The duplicated name.
        name: String,
    },
    /// Two watches resolve to the same expanded path.
    DuplicatePath {
        /// The later watch.
        name: String,
        /// The earlier watch already using the path.
        other: String,
        /// The shared expanded path.
        path: PathBuf,
    },
    /// A watch path needs `~` expansion but `HOME` is unset.
    HomeUnset {
        /// The watch whose path needs expansion.
        name: String,
    },
    /// The `[defaults]` section failed core validation.
    Defaults {
        /// The core error explaining the failure.
        source: vard_core::ConfigError,
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
            ConfigError::MissingVersion => f.write_str(
                "config is missing the `version` key; add `version = 1` at the top of the file",
            ),
            ConfigError::UnsupportedVersion { found } => write!(
                f,
                "unsupported config version {found}; this build supports version {SUPPORTED_VERSION}"
            ),
            ConfigError::DuplicateWatch { name } => {
                write!(
                    f,
                    "duplicate watch name {name:?} (names are compared case-insensitively)"
                )
            }
            ConfigError::DuplicatePath { name, other, path } => write!(
                f,
                "watch {name:?} watches the same path as watch {other:?}: {}",
                path.display()
            ),
            ConfigError::HomeUnset { name } => write!(
                f,
                "watch {name:?}: path needs `~` expansion but HOME is not set"
            ),
            ConfigError::Defaults { source } => {
                write!(f, "[defaults]: {source}")
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
            ConfigError::Defaults { source } => Some(source),
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
    /// `[daemon]`, and per-watch `[watch.hooks]` sections that this build
    /// carries opaquely — their presence proves known-future tolerance. Paths
    /// use `~` like the real spec example.
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
path = "~/notes"

[[watch]]
name = "project"
path = "~/project"
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
    fn parses_spec_example_including_future_sections() {
        let config = Config::from_toml_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.daemon.log_level, LogLevel::Debug);
        assert_eq!(config.daemon.log_retention_days, 30);
        assert_eq!(config.watches.len(), 2);
        // Resolution succeeds despite [ai], [update], [watch.hooks].
        let specs = config
            .resolve_with_home(Some(Path::new("/home/u")))
            .unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].path(), Path::new("/home/u/notes"));
    }

    #[test]
    fn missing_daemon_section_applies_defaults() {
        let config = Config::from_toml_str(
            r#"
version = 1
"#,
        )
        .unwrap();
        assert_eq!(config.daemon.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(config.daemon.log_retention_days, DEFAULT_LOG_RETENTION_DAYS);
    }

    #[test]
    fn partial_daemon_section_defaults_unspecified_fields() {
        let config = Config::from_toml_str(
            r#"
version = 1

[daemon]
log_level = "warn"
"#,
        )
        .unwrap();
        assert_eq!(config.daemon.log_level, LogLevel::Warn);
        assert_eq!(config.daemon.log_retention_days, DEFAULT_LOG_RETENTION_DAYS);
    }

    #[test]
    fn invalid_log_level_is_a_clean_parse_error() {
        let err = Config::from_toml_str(
            r#"
version = 1

[daemon]
log_level = "verbose"
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::Parse(msg) => {
                assert!(msg.contains("log_level"), "got: {msg}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_key_in_daemon_is_rejected() {
        let err = Config::from_toml_str(
            r#"
version = 1

[daemon]
log_leveel = "warn"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("log_leveel"), "got: {err}");
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
        assert_eq!(spec.trigger(), TriggerMode::Interval);
        assert_eq!(spec.interval(), Duration::from_secs(30 * 60));
        assert_eq!(spec.quiesce(), Duration::from_secs(45));
        assert!(!spec.sync());
        assert_eq!(spec.sync_interval(), Duration::from_secs(2 * 3600));
        // Fields with no [defaults] entry still fall to core values.
        assert_eq!(spec.remote(), DEFAULT_REMOTE);
        assert_eq!(spec.branch(), None);
        assert!(spec.exclude().is_empty());
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
        let specs = config.resolve().unwrap();
        let spec = &specs[0];
        assert_eq!(spec.trigger(), TriggerMode::Events);
        assert_eq!(spec.interval(), Duration::from_secs(5 * 60));
        assert_eq!(spec.quiesce(), Duration::from_secs(3));
        assert!(spec.sync());
        assert_eq!(spec.sync_interval(), Duration::from_secs(90 * 60));
        assert_eq!(spec.branch(), Some("backup"));
        assert_eq!(spec.remote(), "mirror");
        assert_eq!(spec.exclude(), ["target".to_string()]);
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
        let specs = config.resolve().unwrap();
        let spec = &specs[0];
        assert_eq!(spec.trigger(), DEFAULT_TRIGGER);
        assert_eq!(spec.interval(), DEFAULT_INTERVAL);
        assert_eq!(spec.quiesce(), DEFAULT_QUIESCE);
        assert_eq!(spec.sync(), DEFAULT_SYNC);
        assert_eq!(spec.sync_interval(), DEFAULT_SYNC_INTERVAL);
        assert_eq!(spec.remote(), DEFAULT_REMOTE);
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
        let specs = config.resolve().unwrap();
        let spec = &specs[0];
        assert!(!spec.sync());
        // sync_interval is still validated (> 0) even with sync off.
        assert_eq!(spec.sync_interval(), DEFAULT_SYNC_INTERVAL);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        match Config::from_toml_str("version = 2\n") {
            Err(ConfigError::UnsupportedVersion { found }) => assert_eq!(found, 2),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn version_gate_precedes_schema_errors() {
        // A future config with a restructured schema must fail on its version,
        // not on some incidental "missing field" from today's schema.
        let err = Config::from_toml_str(
            r#"
version = 2

[[watch]]
id = "restructured-schema-without-name-or-path"
"#,
        )
        .unwrap_err();
        match err {
            ConfigError::UnsupportedVersion { found } => assert_eq!(found, 2),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn missing_version_key_gives_actionable_error() {
        let err = Config::from_toml_str("[[watch]]\nname = \"w\"\npath = \"/p\"\n").unwrap_err();
        assert!(
            err.to_string().contains("version = 1"),
            "error should tell the user to add version = 1, got: {err}"
        );
    }

    #[test]
    fn unknown_key_in_defaults_is_rejected() {
        // A stray key in [defaults] (e.g. `remote`, which has no defaults
        // home) must error, not be silently ignored.
        let err = Config::from_toml_str(
            r#"
version = 1

[defaults]
remote = "evil"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("remote"), "got: {err}");
    }

    #[test]
    fn misspelled_top_level_section_is_rejected() {
        // `[default]` (singular) silently disabling sync is the footgun.
        let err = Config::from_toml_str(
            r#"
version = 1

[default]
sync = false
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("default"), "got: {err}");
    }

    #[test]
    fn unknown_key_in_watch_is_rejected() {
        let err = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "w"
path = "/p"
qiesce = "5s"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("qiesce"), "got: {err}");
    }

    #[test]
    fn defaults_trigger_error_is_attributed_to_defaults() {
        let config = Config::from_toml_str(
            r#"
version = 1

[defaults]
trigger = "bogus"

[[watch]]
name = "notes"
path = "/data/notes"
"#,
        )
        .unwrap();
        let err = config.resolve().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("[defaults]"), "got: {msg}");
        assert!(!msg.contains("notes"), "got: {msg}");
    }

    #[test]
    fn watch_trigger_error_keeps_watch_attribution() {
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
    fn tilde_paths_expand_against_home() {
        // The spec's own example uses `path = "~/dotfiles"`.
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "dots"
path = "~/dotfiles"
"#,
        )
        .unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME set in test env"));
        let specs = config.resolve().unwrap();
        assert_eq!(specs[0].path(), home.join("dotfiles"));
    }

    #[test]
    fn tilde_path_without_home_is_a_clear_error() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "dots"
path = "~/dotfiles"
"#,
        )
        .unwrap();
        match config.resolve_with_home(None) {
            Err(ConfigError::HomeUnset { name }) => assert_eq!(name, "dots"),
            other => panic!("expected HomeUnset, got {other:?}"),
        }
    }

    #[test]
    fn non_tilde_paths_pass_through_unchanged() {
        assert_eq!(
            expand_tilde(Path::new("/abs/path"), Some(Path::new("/home/u"))),
            Some(PathBuf::from("/abs/path"))
        );
        // `~user` forms are not expanded, only `~/` and bare `~`.
        assert_eq!(
            expand_tilde(Path::new("~other/x"), Some(Path::new("/home/u"))),
            Some(PathBuf::from("~other/x"))
        );
        assert_eq!(
            expand_tilde(Path::new("~"), Some(Path::new("/home/u"))),
            Some(PathBuf::from("/home/u"))
        );
        // Absolute paths never need HOME.
        assert_eq!(
            expand_tilde(Path::new("/abs"), None),
            Some(PathBuf::from("/abs"))
        );
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
    fn duplicate_names_differing_only_in_case_are_rejected() {
        // State files collide on case-insensitive filesystems (APFS, Windows).
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "notes"
path = "/a"

[[watch]]
name = "Notes"
path = "/b"
"#,
        )
        .unwrap();
        assert!(
            matches!(config.resolve(), Err(ConfigError::DuplicateWatch { .. })),
            "case-insensitive duplicate names must be rejected"
        );
    }

    #[test]
    fn duplicate_expanded_paths_are_rejected() {
        // Two watches over the same directory would fight over one repo.
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "a"
path = "~/same"

[[watch]]
name = "b"
path = "~/same"
"#,
        )
        .unwrap();
        match config.resolve_with_home(Some(Path::new("/home/u"))) {
            Err(ConfigError::DuplicatePath { name, other, path }) => {
                assert_eq!(name, "b");
                assert_eq!(other, "a");
                assert_eq!(path, PathBuf::from("/home/u/same"));
            }
            other => panic!("expected DuplicatePath, got {other:?}"),
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
    fn load_reads_and_parses_a_file() {
        let dir = std::env::temp_dir().join(format!("vard-cfg-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, SPEC_EXAMPLE).unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.version, 1);
        let specs = config
            .resolve_with_home(Some(Path::new("/home/u")))
            .unwrap();
        assert_eq!(specs.len(), 2);

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
    fn poll_interval_parses_and_reaches_the_spec() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "netfs"
path = "/mnt/share"
poll_interval = "45s"
"#,
        )
        .unwrap();
        let specs = config.resolve().unwrap();
        assert_eq!(specs[0].poll_interval(), Some(Duration::from_secs(45)));
    }

    #[test]
    fn absent_poll_interval_stays_native() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "local"
path = "/data/local"
"#,
        )
        .unwrap();
        let specs = config.resolve().unwrap();
        assert_eq!(specs[0].poll_interval(), None);
    }

    #[test]
    fn zero_poll_interval_is_rejected_with_watch_attribution() {
        let config = Config::from_toml_str(
            r#"
version = 1

[[watch]]
name = "netfs"
path = "/mnt/share"
poll_interval = "0s"
"#,
        )
        .unwrap();
        match config.resolve() {
            Err(ConfigError::Watch { name, source }) => {
                assert_eq!(name, "netfs");
                assert!(
                    source.to_string().contains("poll_interval"),
                    "got: {source}"
                );
            }
            other => panic!("expected Watch error, got {other:?}"),
        }
    }

    #[test]
    fn poll_interval_has_no_defaults_home() {
        // Polling is a property of one watch's filesystem, not a fleet-wide
        // default; a [defaults] entry must be rejected like any unknown key.
        let err = Config::from_toml_str(
            r#"
version = 1

[defaults]
poll_interval = "45s"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("poll_interval"), "got: {err}");
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
