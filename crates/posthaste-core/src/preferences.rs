//! Machine-local preferences document: tunable behavior shared by every
//! workspace on this machine.
//!
//! The document lives under the per-user data directory beside the workspace,
//! never inside it, because preferences must not travel with a synced or shared
//! workspace. It is versioned like the workspace documents and written with the
//! same atomic temp-file-and-rename path.
//!
//! Loading is tolerant: a missing, unreadable, or malformed document yields the
//! documented defaults and never blocks startup. Values outside their supported
//! range are clamped when loaded, and unknown fields are ignored. A document
//! written by a newer build is left untouched for the rest of the session and
//! the defaults apply instead.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::http::{DEFAULT_CONNECT_TIMEOUT, HttpConfig};
use crate::model::SCHEMA_VERSION;
use crate::paths;
use crate::storage::{JsonStore, LoadOutcome, StorageError, StorageIssue, Versioned};

/// File name of the preferences document inside its directory.
pub const PREFERENCES_FILE: &str = "preferences.json";

/// Environment variable naming the preferences directory.
pub const PREFERENCES_DIR_ENV: &str = "POSTHASTE_PREFERENCES";

/// Default and supported request timeout, in seconds.
pub const DEFAULT_TIMEOUT_SECONDS: u32 = 30;
pub const MIN_TIMEOUT_SECONDS: u32 = 1;
pub const MAX_TIMEOUT_SECONDS: u32 = 600;

/// Default and supported redirect limit.
pub const DEFAULT_REDIRECT_LIMIT: u32 = 10;
pub const MIN_REDIRECT_LIMIT: u32 = 0;
pub const MAX_REDIRECT_LIMIT: u32 = 50;

/// Default and supported maximum stored response body, in MiB.
pub const DEFAULT_MAX_BODY_MIB: u32 = 20;
pub const MIN_MAX_BODY_MIB: u32 = 1;
pub const MAX_MAX_BODY_MIB: u32 = 100;

/// Default and supported editor font size, in logical pixels.
pub const DEFAULT_FONT_SIZE: u32 = 14;
pub const MIN_FONT_SIZE: u32 = 12;
pub const MAX_FONT_SIZE: u32 = 16;

/// Default and supported retained history count.
pub const DEFAULT_HISTORY_RETENTION: u32 = 500;
pub const MIN_HISTORY_RETENTION: u32 = 10;
pub const MAX_HISTORY_RETENTION: u32 = 5000;

const BYTES_PER_MIB: usize = 1024 * 1024;

const fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

const fn default_timeout_seconds() -> u32 {
    DEFAULT_TIMEOUT_SECONDS
}

const fn default_redirect_limit() -> u32 {
    DEFAULT_REDIRECT_LIMIT
}

const fn default_max_body_mib() -> u32 {
    DEFAULT_MAX_BODY_MIB
}

const fn default_autosave() -> bool {
    true
}

const fn default_font_size() -> u32 {
    DEFAULT_FONT_SIZE
}

const fn default_history_retention() -> u32 {
    DEFAULT_HISTORY_RETENTION
}

/// The chosen theme. `System` follows the operating system's appearance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
    System,
}

/// The interface language. English is the default; the value is stored in the
/// preferences document and never in a workspace, because it describes this
/// machine's user rather than the shared workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LanguageChoice {
    /// English, the default.
    #[default]
    #[serde(rename = "en")]
    English,
    /// Simplified Chinese.
    #[serde(rename = "zh")]
    Chinese,
}

/// Appearance settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppearancePreferences {
    /// The selected theme, dark by default.
    #[serde(default)]
    pub theme: ThemeChoice,
    /// Whether the Pretty and Raw response bodies wrap long lines.
    #[serde(default)]
    pub wrap_long_lines: bool,
}

/// Request defaults applied when a request does not override them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPreferences {
    /// Default request timeout, in seconds.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u32,
    /// Maximum redirects followed; zero disables redirect following.
    #[serde(default = "default_redirect_limit")]
    pub redirect_limit: u32,
    /// Maximum stored response body, in MiB.
    #[serde(default = "default_max_body_mib")]
    pub max_body_mib: u32,
}

impl Default for RequestPreferences {
    fn default() -> Self {
        Self {
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            redirect_limit: DEFAULT_REDIRECT_LIMIT,
            max_body_mib: DEFAULT_MAX_BODY_MIB,
        }
    }
}

impl RequestPreferences {
    /// The default request timeout as a duration.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(u64::from(self.timeout_seconds))
    }

    /// The redirect limit in the form the HTTP layer uses.
    pub fn redirects(&self) -> usize {
        self.redirect_limit as usize
    }

    /// The response body cap in bytes.
    pub fn body_limit(&self) -> usize {
        self.max_body_mib as usize * BYTES_PER_MIB
    }

    /// Builds the HTTP configuration these defaults describe. The connect
    /// timeout is not user-visible and stays fixed.
    pub fn http_config(&self) -> HttpConfig {
        HttpConfig {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            default_timeout: self.timeout(),
            redirect_limit: self.redirects(),
            body_limit: self.body_limit(),
        }
    }
}

/// Editing settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditingPreferences {
    /// Whether request-field edits are written automatically.
    #[serde(default = "default_autosave")]
    pub autosave: bool,
    /// Editor font size for code-bearing text, in logical pixels.
    #[serde(default = "default_font_size")]
    pub font_size: u32,
}

impl Default for EditingPreferences {
    fn default() -> Self {
        Self {
            autosave: true,
            font_size: DEFAULT_FONT_SIZE,
        }
    }
}

/// History settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPreferences {
    /// Number of entries retained; the oldest beyond it are deleted.
    #[serde(default = "default_history_retention")]
    pub retention: u32,
}

impl Default for HistoryPreferences {
    fn default() -> Self {
        Self {
            retention: DEFAULT_HISTORY_RETENTION,
        }
    }
}

impl HistoryPreferences {
    /// The retained count in the form the history store uses.
    pub fn entries(&self) -> usize {
        self.retention as usize
    }
}

/// A window frame recorded at close, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Whether the window was maximized. The four bounds then describe the
    /// restore state.
    #[serde(default)]
    pub maximized: bool,
}

impl WindowFrame {
    /// Whether every coordinate is finite and the size is positive.
    pub fn is_usable(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.
            && self.height > 0.
    }
}

/// Remembered window layout. Every field is optional so a missing layout
/// section means "no remembered frame".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LayoutPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_ratio: Option<f32>,
}

/// Reads the interface language, treating a value this build does not know —
/// or a value of the wrong type — as English, so one unknown token cannot reset
/// the rest of the document.
fn deserialize_language<'de, D>(deserializer: D) -> Result<LanguageChoice, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value.as_str() {
        Some("zh") => LanguageChoice::Chinese,
        _ => LanguageChoice::English,
    })
}

/// The complete preferences document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The interface language, English by default.
    #[serde(default, deserialize_with = "deserialize_language")]
    pub language: LanguageChoice,
    #[serde(default)]
    pub appearance: AppearancePreferences,
    #[serde(default)]
    pub requests: RequestPreferences,
    #[serde(default)]
    pub editing: EditingPreferences,
    #[serde(default)]
    pub history: HistoryPreferences,
    #[serde(default)]
    pub layout: LayoutPreferences,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            language: LanguageChoice::default(),
            appearance: AppearancePreferences::default(),
            requests: RequestPreferences::default(),
            editing: EditingPreferences::default(),
            history: HistoryPreferences::default(),
            layout: LayoutPreferences::default(),
        }
    }
}

impl Versioned for Preferences {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl Preferences {
    /// Clamps every setting to its supported range and drops unusable layout
    /// values. Called on every load; the settings dialog clamps its own
    /// controls, so this is the safety net for hand-edited files.
    pub fn clamp(&mut self) {
        self.requests.timeout_seconds = self
            .requests
            .timeout_seconds
            .clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS);
        self.requests.redirect_limit = self
            .requests
            .redirect_limit
            .clamp(MIN_REDIRECT_LIMIT, MAX_REDIRECT_LIMIT);
        self.requests.max_body_mib = self
            .requests
            .max_body_mib
            .clamp(MIN_MAX_BODY_MIB, MAX_MAX_BODY_MIB);
        self.editing.font_size = self.editing.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.history.retention = self
            .history
            .retention
            .clamp(MIN_HISTORY_RETENTION, MAX_HISTORY_RETENTION);

        if let Some(window) = self.layout.window
            && !window.is_usable()
        {
            self.layout.window = None;
        }
        self.layout.split_ratio = self
            .layout
            .split_ratio
            .filter(|ratio| ratio.is_finite())
            .map(|ratio| ratio.clamp(0., 1.));
    }

    /// Returns the document with every setting clamped.
    pub fn normalized(mut self) -> Self {
        self.clamp();
        self
    }
}

/// What loading the preferences document produced.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPreferences {
    /// The effective preferences: the loaded document clamped, or the defaults.
    pub preferences: Preferences,
    /// A non-fatal problem with the file, for logging. A missing file and a
    /// newer schema are not issues.
    pub issue: Option<StorageIssue>,
}

impl LoadedPreferences {
    fn defaults() -> Self {
        Self {
            preferences: Preferences::default(),
            issue: None,
        }
    }
}

/// The machine-local preferences file.
///
/// A disabled store (no platform data directory and no override) loads the
/// defaults and ignores saves, which keeps the app usable without a profile.
#[derive(Debug, Clone)]
pub struct PreferencesStore {
    store: Option<JsonStore>,
    writable: bool,
}

impl PreferencesStore {
    /// Resolves the preferences directory from `POSTHASTE_PREFERENCES`, or the
    /// platform per-user data directory beneath `Posthaste/preferences`.
    pub fn resolve() -> Self {
        Self::resolve_from(paths::env_value(PREFERENCES_DIR_ENV), paths::home())
    }

    /// The injectable form of [`Self::resolve`], taking the already-resolved
    /// product directory under the platform data directory.
    pub(crate) fn resolve_from(override_dir: Option<OsString>, data_home: Option<PathBuf>) -> Self {
        let root = if let Some(value) = override_dir
            && !value.is_empty()
        {
            Some(PathBuf::from(value))
        } else {
            data_home.map(|data_home| data_home.join("preferences"))
        };
        Self {
            store: root.map(|root| JsonStore::in_dir(root, PREFERENCES_FILE)),
            writable: true,
        }
    }

    /// A store rooted at an explicit directory. The document is
    /// `preferences.json` inside it.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            store: Some(JsonStore::in_dir(root, PREFERENCES_FILE)),
            writable: true,
        }
    }

    /// A store that loads defaults and writes nothing.
    pub fn disabled() -> Self {
        Self {
            store: None,
            writable: false,
        }
    }

    /// The preferences directory, when the store is enabled.
    pub fn root(&self) -> Option<&Path> {
        self.store.as_ref().and_then(|store| store.path().parent())
    }

    /// The document path, when the store is enabled.
    pub fn path(&self) -> Option<PathBuf> {
        self.store.as_ref().map(|store| store.path().to_path_buf())
    }

    /// Whether [`Self::save`] may write. A document from a newer build clears
    /// this for the rest of the session.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Reads the document and clamps it. A missing file, an unreadable file,
    /// and a malformed file all yield the defaults; the first two are silent,
    /// the last reports a logging issue. A newer schema yields the defaults and
    /// makes the store refuse writes.
    pub fn load(&mut self) -> LoadedPreferences {
        if !self.writable {
            return LoadedPreferences::defaults();
        }
        let Some(store) = self.store.as_ref() else {
            return LoadedPreferences::defaults();
        };
        match store.load::<Preferences>() {
            Ok(LoadOutcome::Absent) => LoadedPreferences::defaults(),
            Ok(LoadOutcome::Loaded(preferences)) => LoadedPreferences {
                preferences: preferences.normalized(),
                issue: None,
            },
            Err(StorageError::UnsupportedSchema { .. }) => {
                // Never overwrite a document a newer build wrote.
                self.writable = false;
                LoadedPreferences::defaults()
            }
            Err(error) => {
                tracing::warn!(
                    path = %store.path().display(),
                    error = %error,
                    "ignoring unreadable preferences document; the defaults apply"
                );
                let issue = StorageIssue::from_error(&error);
                LoadedPreferences {
                    preferences: Preferences::default(),
                    issue,
                }
            }
        }
    }

    /// Replaces the document atomically. A disabled or write-protected store
    /// does nothing, and a failure leaves the previous file in place.
    pub fn save(&self, preferences: &Preferences) -> Result<(), StorageError> {
        if !self.writable {
            return Ok(());
        }
        let Some(store) = self.store.as_ref() else {
            return Ok(());
        };
        store.write(preferences)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store_in(dir: &tempfile::TempDir) -> PreferencesStore {
        PreferencesStore::at(dir.path())
    }

    #[test]
    fn defaults_match_the_documented_behavior() {
        let preferences = Preferences::default();

        assert_eq!(preferences.schema_version, SCHEMA_VERSION);
        assert_eq!(preferences.language, LanguageChoice::English);
        assert_eq!(preferences.appearance.theme, ThemeChoice::Dark);
        assert!(!preferences.appearance.wrap_long_lines);
        assert_eq!(preferences.requests.timeout_seconds, 30);
        assert_eq!(preferences.requests.redirect_limit, 10);
        assert_eq!(preferences.requests.max_body_mib, 20);
        assert!(preferences.editing.autosave);
        assert_eq!(preferences.editing.font_size, 14);
        assert_eq!(preferences.history.retention, 500);
        assert_eq!(preferences.layout.window, None);
        assert_eq!(preferences.layout.split_ratio, None);
    }

    #[test]
    fn the_language_round_trips_and_unknown_values_read_english() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(PREFERENCES_FILE);
        let preferences = Preferences {
            language: LanguageChoice::Chinese,
            ..Preferences::default()
        };
        let store = store_in(&dir);
        store.save(&preferences).expect("save");
        let json = fs::read_to_string(&path).expect("read preferences");
        assert!(json.contains("\"language\": \"zh\""), "{json}");

        let reloaded = store_in(&dir).load();
        assert_eq!(reloaded.preferences.language, LanguageChoice::Chinese);

        let missing = Preferences {
            language: LanguageChoice::Chinese,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&missing).expect("encode");
        let without_field = json.replace("\"language\":\"zh\",", "");
        assert!(!without_field.contains("language"), "{without_field}");
        let parsed: Preferences = serde_json::from_str(&without_field).expect("parse");
        assert_eq!(
            parsed.language,
            LanguageChoice::English,
            "an absent field is English"
        );

        for unknown in [r#""fr""#, "7", "null", r#"{"code": "zh"}"#] {
            let json = format!(r#"{{"schema_version": 1, "language": {unknown}}}"#);
            let parsed: Preferences =
                serde_json::from_str(&json).expect("an unknown language parses");
            assert_eq!(parsed.language, LanguageChoice::English, "{unknown}");
        }
    }

    #[test]
    fn a_missing_file_loads_the_defaults_without_an_issue() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = store_in(&dir);

        let loaded = store.load();

        assert_eq!(loaded.preferences, Preferences::default());
        assert_eq!(loaded.issue, None);
        assert!(store.is_writable());
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(PREFERENCES_FILE);
        fs::write(
            &path,
            r#"{
                "requests": {"timeout_seconds": 900, "redirect_limit": 99, "max_body_mib": 0},
                "editing": {"font_size": 40},
                "history": {"retention": 2},
                "layout": {"split_ratio": 3.0}
            }"#,
        )
        .expect("write preferences");

        let loaded = store_in(&dir).load();

        assert_eq!(loaded.preferences.requests.timeout_seconds, 600);
        assert_eq!(loaded.preferences.requests.redirect_limit, 50);
        assert_eq!(loaded.preferences.requests.max_body_mib, 1);
        assert_eq!(loaded.preferences.editing.font_size, 16);
        assert_eq!(loaded.preferences.history.retention, 10);
        assert_eq!(loaded.preferences.layout.split_ratio, Some(1.));
        assert_eq!(loaded.issue, None);
    }

    #[test]
    fn values_below_the_range_are_clamped() {
        let dir = tempfile::tempdir().expect("dir");
        fs::write(
            dir.path().join(PREFERENCES_FILE),
            r#"{
                "requests": {"timeout_seconds": 0, "redirect_limit": 0, "max_body_mib": 0},
                "editing": {"font_size": 1},
                "history": {"retention": 1}
            }"#,
        )
        .expect("write preferences");

        let loaded = store_in(&dir).load();

        assert_eq!(loaded.preferences.requests.timeout_seconds, 1);
        assert_eq!(loaded.preferences.requests.redirect_limit, 0);
        assert_eq!(loaded.preferences.requests.max_body_mib, 1);
        assert_eq!(loaded.preferences.editing.font_size, 12);
        assert_eq!(loaded.preferences.history.retention, 10);
    }

    #[test]
    fn missing_sections_and_unknown_fields_fall_back_to_defaults() {
        let dir = tempfile::tempdir().expect("dir");
        fs::write(
            dir.path().join(PREFERENCES_FILE),
            r#"{
                "mystery": true,
                "appearance": {"theme": "light", "sparkle": 4},
                "requests": {"timeout_seconds": 45, "future_field": "ignored"}
            }"#,
        )
        .expect("write preferences");

        let loaded = store_in(&dir).load();

        assert_eq!(loaded.preferences.appearance.theme, ThemeChoice::Light);
        assert_eq!(loaded.preferences.requests.timeout_seconds, 45);
        assert_eq!(loaded.preferences.requests.redirect_limit, 10);
        assert_eq!(loaded.preferences.editing.font_size, 14);
        assert_eq!(loaded.preferences.history.retention, 500);
        assert_eq!(loaded.issue, None);
    }

    #[test]
    fn malformed_json_loads_defaults_with_an_issue_and_stays_writable() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(PREFERENCES_FILE);
        fs::write(&path, b"{ not json").expect("write malformed");
        let mut store = store_in(&dir);

        let loaded = store.load();

        assert_eq!(loaded.preferences, Preferences::default());
        assert!(loaded.issue.is_some());
        assert!(store.is_writable());
    }

    #[test]
    fn a_newer_schema_loads_defaults_and_protects_the_document() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(PREFERENCES_FILE);
        let newer = format!(r#"{{"schema_version": {}}}"#, SCHEMA_VERSION + 1);
        fs::write(&path, &newer).expect("write newer");
        let mut store = store_in(&dir);

        let loaded = store.load();

        assert_eq!(loaded.preferences, Preferences::default());
        assert!(!store.is_writable());
        store
            .save(&Preferences::default())
            .expect("a protected save is a no-op");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            newer,
            "the newer document is untouched"
        );
    }

    #[test]
    fn the_document_round_trips_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(&dir);
        let written = Preferences {
            appearance: AppearancePreferences {
                theme: ThemeChoice::System,
                wrap_long_lines: true,
            },
            requests: RequestPreferences {
                timeout_seconds: 45,
                redirect_limit: 0,
                max_body_mib: 5,
            },
            editing: EditingPreferences {
                autosave: false,
                font_size: 16,
            },
            history: HistoryPreferences { retention: 100 },
            layout: LayoutPreferences {
                window: Some(WindowFrame {
                    x: 120.,
                    y: 80.,
                    width: 1440.,
                    height: 900.,
                    maximized: true,
                }),
                split_ratio: Some(0.6),
            },
            ..Preferences::default()
        };

        store.save(&written).expect("save preferences");

        let mut reader = store_in(&dir);
        assert_eq!(reader.load().preferences, written);
        let files: Vec<_> = fs::read_dir(dir.path())
            .expect("read preferences dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(files.len(), 1, "only the preferences document remains");
        assert_eq!(files[0], PREFERENCES_FILE);
    }

    #[test]
    fn resolve_honors_the_directory_override() {
        let override_dir = tempfile::tempdir().expect("override");
        let data_dir = tempfile::tempdir().expect("data");

        let store = PreferencesStore::resolve_from(
            Some(override_dir.path().as_os_str().to_owned()),
            Some(data_dir.path().to_path_buf()),
        );

        assert_eq!(store.root(), Some(override_dir.path()));
        assert_eq!(
            store.path(),
            Some(override_dir.path().join(PREFERENCES_FILE))
        );
    }

    #[test]
    fn resolve_falls_back_to_the_platform_data_directory() {
        let data_dir = tempfile::tempdir().expect("data");

        let store = PreferencesStore::resolve_from(
            None,
            paths::home_in(Some(data_dir.path().to_path_buf())),
        );

        assert_eq!(
            store.root(),
            Some(
                data_dir
                    .path()
                    .join("Posthaste")
                    .join("preferences")
                    .as_path()
            )
        );
    }

    #[test]
    fn loading_removes_temporary_residue_in_the_preferences_directory() {
        let dir = tempfile::tempdir().expect("dir");
        let stale = dir.path().join(".posthaste-abc.posthaste-tmp");
        let unrelated = dir.path().join("notes.txt");
        fs::write(&stale, "{").expect("write residue");
        fs::write(&unrelated, "keep me").expect("write unrelated file");
        let mut store = PreferencesStore::at(dir.path());

        let _ = store.load();

        assert!(
            !stale.exists(),
            "residue from an interrupted write is removed"
        );
        assert_eq!(
            fs::read_to_string(&unrelated).expect("unrelated file"),
            "keep me"
        );
    }

    #[test]
    fn a_disabled_store_loads_defaults_and_writes_nothing() {
        let mut store = PreferencesStore::disabled();

        assert_eq!(store.load().preferences, Preferences::default());
        store
            .save(&Preferences::default())
            .expect("disabled save is a no-op");
        assert!(!store.is_writable());
    }

    #[test]
    fn request_helpers_follow_the_settings() {
        let preferences = Preferences {
            requests: RequestPreferences {
                timeout_seconds: 45,
                redirect_limit: 0,
                max_body_mib: 5,
            },
            ..Preferences::default()
        };

        assert_eq!(preferences.requests.timeout(), Duration::from_secs(45));
        assert_eq!(preferences.requests.redirects(), 0);
        assert_eq!(preferences.requests.body_limit(), 5 * 1024 * 1024);
        let config = preferences.requests.http_config();
        assert_eq!(config.connect_timeout, DEFAULT_CONNECT_TIMEOUT);
        assert_eq!(config.default_timeout, Duration::from_secs(45));
        assert_eq!(config.redirect_limit, 0);
        assert_eq!(config.body_limit, 5 * 1024 * 1024);
    }
}
