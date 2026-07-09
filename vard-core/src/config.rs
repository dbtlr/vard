//! The value model for a watch: [`WatchSpec`], its [`builder`](WatchSpec::builder),
//! and the [`TriggerMode`] that selects how snapshots are triggered.
//!
//! This crate owns **correctness**, so the invariants of a watch live here, not
//! in any host. The engine and every embedder take watches as validated
//! [`WatchSpec`] values; there is no file I/O and no serde in this module (see
//! the [crate docs](crate)). The binary's file-config layer and any SDK
//! embedder both build [`WatchSpec`]s through the same [`builder`], so the
//! default *values* live here as public constants — one source of truth shared
//! across every host.
//!
//! # Relation to the spec's SDK sketch
//!
//! The spec (§2a) illustrates the SDK with `WatchSpec::new(..)` and
//! string-typed duration setters. The shipped API deliberately supersedes that
//! sketch: construction is `WatchSpec::builder(name, path)`, duration setters
//! take [`Duration`] values, and humantime strings are parsed explicitly via
//! the public [`parse_duration`]. Typed durations keep the setters infallible
//! and make the one fallible step (string parsing) explicit at the call site
//! instead of deferring hidden errors to `build()`.
//!
//! [`TriggerMode`] is deliberately not named `Trigger`: [`Trigger`](crate::Trigger)
//! is the event vocabulary describing *why* a snapshot happened, whereas
//! `TriggerMode` is the *configuration* selecting which triggers a watch arms.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// Default quiescence window: how long file activity must settle before a
/// change-triggered snapshot is taken.
pub const DEFAULT_QUIESCE: Duration = Duration::from_secs(10);

/// Default interval between periodic snapshots.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Default interval between background syncs to the remote.
pub const DEFAULT_SYNC_INTERVAL: Duration = Duration::from_secs(20 * 60);

/// Default for whether a watch syncs to a remote at all.
pub const DEFAULT_SYNC: bool = true;

/// Default trigger mode: arm both change and interval triggers.
pub const DEFAULT_TRIGGER: TriggerMode = TriggerMode::Both;

/// Default remote name a watch pushes to and pulls from.
pub const DEFAULT_REMOTE: &str = "origin";

/// Which *automatic* snapshot triggers a watch arms.
///
/// This mode governs only the two background causes — filesystem-event
/// snapshots and interval-timer snapshots. Manual snapshots
/// ([`Trigger::Manual`](crate::Trigger)) and protective snapshots taken before
/// a restore or sync ([`Trigger::PreRestore`](crate::Trigger),
/// [`Trigger::PreSync`](crate::Trigger)) always run regardless of mode; the
/// scheduler must not gate them on `TriggerMode`.
///
/// Distinct from [`Trigger`](crate::Trigger), which reports why a snapshot was
/// taken. This type is the *configuration* knob; `Trigger` is the *event*.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TriggerMode {
    /// Snapshot automatically only in response to observed filesystem changes.
    Events,
    /// Snapshot automatically only when the periodic interval elapses.
    Interval,
    /// Arm both change and interval triggers. The default.
    #[default]
    Both,
}

impl fmt::Display for TriggerMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TriggerMode::Events => "events",
            TriggerMode::Interval => "interval",
            TriggerMode::Both => "both",
        };
        f.write_str(s)
    }
}

impl FromStr for TriggerMode {
    type Err = ConfigError;

    /// Parses the canonical lowercase spellings (`events`, `interval`, `both`),
    /// case-insensitively. Any other value is a [`ConfigError::UnknownTriggerMode`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "events" => Ok(TriggerMode::Events),
            "interval" => Ok(TriggerMode::Interval),
            "both" => Ok(TriggerMode::Both),
            _ => Err(ConfigError::UnknownTriggerMode {
                value: s.to_string(),
            }),
        }
    }
}

/// A validated description of one watch: what to watch and how to snapshot it.
///
/// The only way to obtain a `WatchSpec` is through the validating
/// [`builder`](WatchSpec::builder), and fields are private with read accessors,
/// so a spec that exists is a spec that passed validation — it cannot be
/// mutated out of its invariants after `build()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchSpec {
    name: String,
    path: PathBuf,
    trigger: TriggerMode,
    quiesce: Duration,
    interval: Duration,
    sync: bool,
    sync_interval: Duration,
    exclude: Vec<String>,
    branch: Option<String>,
    remote: String,
}

impl WatchSpec {
    /// Starts building a watch named `name` over `path`, with every other field
    /// preset to its `DEFAULT_*` constant. Chain setters, then call
    /// [`build`](WatchSpecBuilder::build).
    pub fn builder(name: impl Into<String>, path: impl Into<PathBuf>) -> WatchSpecBuilder {
        WatchSpecBuilder::new(name, path)
    }

    /// Stable identity of the watch. Used to name state files, so its charset
    /// is restricted (see [`WatchSpecBuilder::build`]).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Directory the watch snapshots.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which automatic triggers arm snapshots.
    pub fn trigger(&self) -> TriggerMode {
        self.trigger
    }

    /// How long activity must settle before a change-triggered snapshot.
    pub fn quiesce(&self) -> Duration {
        self.quiesce
    }

    /// Interval between periodic snapshots.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Whether the watch syncs to a remote.
    pub fn sync(&self) -> bool {
        self.sync
    }

    /// Interval between background syncs.
    pub fn sync_interval(&self) -> Duration {
        self.sync_interval
    }

    /// Gitignore-style patterns excluded from snapshots.
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }

    /// Branch the watch commits to. `None` adopts HEAD's branch at registration.
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    /// Remote the watch pushes to and pulls from.
    pub fn remote(&self) -> &str {
        &self.remote
    }
}

/// A fluent builder for [`WatchSpec`]. Obtain one from [`WatchSpec::builder`].
///
/// Duration setters take [`Duration`] values; parse humantime strings with
/// [`parse_duration`] first (see the [module docs](self) for why the API is
/// shaped this way).
#[derive(Clone, Debug)]
pub struct WatchSpecBuilder {
    name: String,
    path: PathBuf,
    trigger: TriggerMode,
    quiesce: Duration,
    interval: Duration,
    sync: bool,
    sync_interval: Duration,
    exclude: Vec<String>,
    branch: Option<String>,
    remote: String,
}

impl WatchSpecBuilder {
    fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            trigger: DEFAULT_TRIGGER,
            quiesce: DEFAULT_QUIESCE,
            interval: DEFAULT_INTERVAL,
            sync: DEFAULT_SYNC,
            sync_interval: DEFAULT_SYNC_INTERVAL,
            exclude: Vec::new(),
            branch: None,
            remote: DEFAULT_REMOTE.to_string(),
        }
    }

    /// Sets which automatic triggers arm snapshots.
    pub fn trigger(mut self, trigger: TriggerMode) -> Self {
        self.trigger = trigger;
        self
    }

    /// Sets the quiescence window.
    pub fn quiesce(mut self, quiesce: Duration) -> Self {
        self.quiesce = quiesce;
        self
    }

    /// Sets the periodic snapshot interval.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Sets whether the watch syncs to a remote.
    pub fn sync(mut self, sync: bool) -> Self {
        self.sync = sync;
        self
    }

    /// Sets the background sync interval.
    pub fn sync_interval(mut self, sync_interval: Duration) -> Self {
        self.sync_interval = sync_interval;
        self
    }

    /// Replaces the exclude patterns.
    pub fn exclude(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exclude = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the branch to commit to. `None` adopts HEAD's branch at registration.
    pub fn branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = Some(branch.into());
        self
    }

    /// Sets the remote to push to and pull from.
    pub fn remote(mut self, remote: impl Into<String>) -> Self {
        self.remote = remote.into();
        self
    }

    /// Validates the accumulated fields and produces a [`WatchSpec`].
    ///
    /// Enforces: non-empty name; a name limited to ASCII alphanumerics and
    /// `-`, `_`, `.` and not the path-special `.` or `..` (safe for state-file
    /// names); non-empty path; and strictly positive `quiesce`, `interval`,
    /// and `sync_interval`.
    pub fn build(self) -> Result<WatchSpec, ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::EmptyName);
        }
        if self.name == "." || self.name == ".." {
            return Err(ConfigError::ReservedName { name: self.name });
        }
        if let Some(ch) = self
            .name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        {
            return Err(ConfigError::InvalidNameChar {
                name: self.name,
                ch,
            });
        }
        if self.path.as_os_str().is_empty() {
            return Err(ConfigError::EmptyPath);
        }
        if self.quiesce.is_zero() {
            return Err(ConfigError::ZeroDuration { field: "quiesce" });
        }
        if self.interval.is_zero() {
            return Err(ConfigError::ZeroDuration { field: "interval" });
        }
        if self.sync_interval.is_zero() {
            return Err(ConfigError::ZeroDuration {
                field: "sync_interval",
            });
        }

        Ok(WatchSpec {
            name: self.name,
            path: self.path,
            trigger: self.trigger,
            quiesce: self.quiesce,
            interval: self.interval,
            sync: self.sync,
            sync_interval: self.sync_interval,
            exclude: self.exclude,
            branch: self.branch,
            remote: self.remote,
        })
    }
}

/// Everything that can go wrong building a [`WatchSpec`] or parsing its inputs.
///
/// Implements [`std::error::Error`] and carries no dependency on any error
/// crate, in keeping with the crate's dependency-light contract.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// The watch name was empty.
    EmptyName,
    /// The watch name was `.` or `..`, which are path-special and unsafe as
    /// state-file names.
    ReservedName {
        /// The reserved name.
        name: String,
    },
    /// The watch name contained a character outside the state-file-safe set.
    InvalidNameChar {
        /// The offending name.
        name: String,
        /// The first disallowed character found.
        ch: char,
    },
    /// The watch path was empty.
    EmptyPath,
    /// A duration field that must be positive was zero.
    ZeroDuration {
        /// Which field was zero.
        field: &'static str,
    },
    /// A humantime duration string could not be parsed.
    InvalidDuration {
        /// The input that failed to parse.
        value: String,
        /// Why it failed.
        reason: String,
    },
    /// A trigger-mode string did not name a known mode.
    UnknownTriggerMode {
        /// The unrecognized value.
        value: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::EmptyName => f.write_str("watch name must not be empty"),
            ConfigError::ReservedName { name } => {
                write!(f, "watch name {name:?} is reserved; pick another name")
            }
            ConfigError::InvalidNameChar { name, ch } => write!(
                f,
                "watch name {name:?} contains invalid character {ch:?}; \
                 only ASCII alphanumerics and '-', '_', '.' are allowed"
            ),
            ConfigError::EmptyPath => f.write_str("watch path must not be empty"),
            ConfigError::ZeroDuration { field } => {
                write!(f, "{field} must be greater than zero")
            }
            ConfigError::InvalidDuration { value, reason } => {
                write!(f, "invalid duration {value:?}: {reason}")
            }
            ConfigError::UnknownTriggerMode { value } => write!(
                f,
                "unknown trigger mode {value:?}; expected one of: events, interval, both"
            ),
        }
    }
}

impl Error for ConfigError {}

/// Parses a humantime-style duration string into a [`Duration`].
///
/// Supports whitespace-separated segments of an integer followed by a unit,
/// summed together (e.g. `"1h30m"`, `"90 s"`). Segments must run from the
/// largest unit down, each unit at most once — `"1h30m"` is valid, `"30m1h"`
/// and `"1s1s"` are not, matching the humantime grammar this subsets.
/// Recognized units:
///
/// - `ns`
/// - `us`
/// - `ms`
/// - `s`, `sec`, `secs`, `second`, `seconds`
/// - `m`, `min`, `mins`, `minute`, `minutes`
/// - `h`, `hr`, `hrs`, `hour`, `hours`
/// - `d`, `day`, `days`
///
/// This is a deliberately small subset of the `humantime` crate's grammar (no
/// fractional values, no microsecond `µs` spelling): the engine stays
/// dependency-light, so the parser it needs lives here rather than pulling a
/// crate. The binary's TOML layer routes through this same function, keeping
/// one source of truth for duration parsing.
pub fn parse_duration(input: &str) -> Result<Duration, ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidDuration {
        value: input.to_string(),
        reason: reason.to_string(),
    };

    let bytes = input.as_bytes();
    let mut i = 0;
    let mut total = Duration::ZERO;
    let mut segments = 0usize;
    // Magnitude rank of the previous segment's unit; each segment must rank
    // strictly below it (largest unit first, no repeats).
    let mut prev_rank: Option<u8> = None;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        let num_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == num_start {
            return Err(invalid("expected a number"));
        }
        let num: u64 = input[num_start..i]
            .parse()
            .map_err(|_| invalid("number out of range"))?;

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &input[unit_start..i];
        if unit.is_empty() {
            return Err(invalid("missing unit"));
        }

        let (rank, seg) = match unit {
            "ns" => (0, Duration::from_nanos(num)),
            "us" => (1, Duration::from_micros(num)),
            "ms" => (2, Duration::from_millis(num)),
            "s" | "sec" | "secs" | "second" | "seconds" => (3, Duration::from_secs(num)),
            "m" | "min" | "mins" | "minute" | "minutes" => (4, secs(num, 60, invalid)?),
            "h" | "hr" | "hrs" | "hour" | "hours" => (5, secs(num, 3600, invalid)?),
            "d" | "day" | "days" => (6, secs(num, 86_400, invalid)?),
            other => return Err(invalid(&format!("unknown unit {other:?}"))),
        };

        if let Some(prev) = prev_rank
            && rank >= prev
        {
            return Err(invalid(
                "units must run from largest to smallest without repeats",
            ));
        }
        prev_rank = Some(rank);

        total = total
            .checked_add(seg)
            .ok_or_else(|| invalid("duration overflow"))?;
        segments += 1;
    }

    if segments == 0 {
        return Err(invalid("empty duration"));
    }
    Ok(total)
}

/// Multiplies `num` by `factor` seconds with overflow checking.
fn secs(
    num: u64,
    factor: u64,
    invalid: impl Fn(&str) -> ConfigError,
) -> Result<Duration, ConfigError> {
    num.checked_mul(factor)
        .map(Duration::from_secs)
        .ok_or_else(|| invalid("duration overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_defaults_match_core_constants() {
        let spec = WatchSpec::builder("notes", "/home/u/notes")
            .build()
            .unwrap();
        assert_eq!(spec.name(), "notes");
        assert_eq!(spec.path(), Path::new("/home/u/notes"));
        assert_eq!(spec.trigger(), DEFAULT_TRIGGER);
        assert_eq!(spec.quiesce(), DEFAULT_QUIESCE);
        assert_eq!(spec.interval(), DEFAULT_INTERVAL);
        assert_eq!(spec.sync(), DEFAULT_SYNC);
        assert_eq!(spec.sync_interval(), DEFAULT_SYNC_INTERVAL);
        assert!(spec.exclude().is_empty());
        assert_eq!(spec.branch(), None);
        assert_eq!(spec.remote(), DEFAULT_REMOTE);
    }

    #[test]
    fn default_trigger_mode_is_both() {
        assert_eq!(TriggerMode::default(), TriggerMode::Both);
        assert_eq!(DEFAULT_TRIGGER, TriggerMode::Both);
    }

    #[test]
    fn builder_setters_override_every_field() {
        let spec = WatchSpec::builder("proj", "/tmp/proj")
            .trigger(TriggerMode::Events)
            .quiesce(Duration::from_secs(3))
            .interval(Duration::from_secs(300))
            .sync(false)
            .sync_interval(Duration::from_secs(3600))
            .exclude(["target", "*.log"])
            .branch("backup")
            .remote("origin2")
            .build()
            .unwrap();
        assert_eq!(spec.trigger(), TriggerMode::Events);
        assert_eq!(spec.quiesce(), Duration::from_secs(3));
        assert_eq!(spec.interval(), Duration::from_secs(300));
        assert!(!spec.sync());
        assert_eq!(spec.sync_interval(), Duration::from_secs(3600));
        assert_eq!(spec.exclude(), ["target".to_string(), "*.log".to_string()]);
        assert_eq!(spec.branch(), Some("backup"));
        assert_eq!(spec.remote(), "origin2");
    }

    #[test]
    fn build_rejects_empty_name() {
        assert_eq!(
            WatchSpec::builder("", "/p").build(),
            Err(ConfigError::EmptyName)
        );
    }

    #[test]
    fn build_rejects_dot_and_dotdot_names() {
        // "." and ".." pass the charset check but are path-special; a state
        // file named after them would escape or collide with its directory.
        for name in [".", ".."] {
            assert_eq!(
                WatchSpec::builder(name, "/p").build(),
                Err(ConfigError::ReservedName {
                    name: name.to_string()
                }),
                "expected reserved name {name:?} to be rejected"
            );
        }
        // Names merely containing dots stay valid.
        assert!(WatchSpec::builder("a.b", "/p").build().is_ok());
        assert!(WatchSpec::builder(".hidden", "/p").build().is_ok());
    }

    #[test]
    fn build_rejects_unsafe_name_charset() {
        match WatchSpec::builder("bad/name", "/p").build() {
            Err(ConfigError::InvalidNameChar { name, ch }) => {
                assert_eq!(name, "bad/name");
                assert_eq!(ch, '/');
            }
            other => panic!("expected InvalidNameChar, got {other:?}"),
        }
        // The safe set is accepted.
        assert!(WatchSpec::builder("a-b_c.9", "/p").build().is_ok());
    }

    #[test]
    fn build_rejects_empty_path() {
        assert_eq!(
            WatchSpec::builder("n", "").build(),
            Err(ConfigError::EmptyPath)
        );
    }

    #[test]
    fn build_rejects_zero_durations() {
        assert_eq!(
            WatchSpec::builder("n", "/p")
                .quiesce(Duration::ZERO)
                .build(),
            Err(ConfigError::ZeroDuration { field: "quiesce" })
        );
        assert_eq!(
            WatchSpec::builder("n", "/p")
                .interval(Duration::ZERO)
                .build(),
            Err(ConfigError::ZeroDuration { field: "interval" })
        );
        assert_eq!(
            WatchSpec::builder("n", "/p")
                .sync_interval(Duration::ZERO)
                .build(),
            Err(ConfigError::ZeroDuration {
                field: "sync_interval"
            })
        );
    }

    #[test]
    fn parse_duration_accepts_spec_examples_and_units() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("20m").unwrap(), Duration::from_secs(1200));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_duration("1h30m").unwrap(),
            Duration::from_secs(3600 + 1800)
        );
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn parse_duration_requires_strictly_descending_unique_units() {
        // The humantime grammar this parser subsets orders segments from the
        // largest unit down, with no repeats.
        assert!(parse_duration("1h30m").is_ok());
        assert!(parse_duration("1h30m10s").is_ok());
        for bad in ["1s1s", "30m1h", "5m5m5m"] {
            assert!(
                parse_duration(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        for bad in ["", "   ", "15", "abc", "10x", "s", "10 20"] {
            assert!(
                parse_duration(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn trigger_mode_roundtrips_through_string() {
        for mode in [
            TriggerMode::Events,
            TriggerMode::Interval,
            TriggerMode::Both,
        ] {
            assert_eq!(mode.to_string().parse::<TriggerMode>().unwrap(), mode);
        }
        assert_eq!("BOTH".parse::<TriggerMode>().unwrap(), TriggerMode::Both);
    }

    #[test]
    fn trigger_mode_rejects_unknown() {
        match "sometimes".parse::<TriggerMode>() {
            Err(ConfigError::UnknownTriggerMode { value }) => assert_eq!(value, "sometimes"),
            other => panic!("expected UnknownTriggerMode, got {other:?}"),
        }
    }
}
