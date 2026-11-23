//! Machine-local session document: the open tab strip of one workspace.
//!
//! The document lives under the per-user data directory, never inside the
//! workspace, because it holds unsaved drafts verbatim — including
//! Authorization headers and tokens. The file name is a UUID v5 of the
//! workspace's canonical root, the same keying the cookie jar and the request
//! index use, and the document repeats the root so a copied or hand-edited file
//! is detected instead of restored.
//!
//! A completed response is referenced by the history entry that recorded the
//! run; no response bytes are copied here. Loading is tolerant: a missing,
//! unreadable, or malformed document means "no session", while a newer schema or
//! a foreign root protects the file from being overwritten for the rest of the
//! process.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::history::HistoryId;
use crate::model::{RequestId, RequestSpec, SCHEMA_VERSION};
use crate::storage::{
    StorageError, Versioned, cleanup_temporary_files, read_versioned, write_document_atomic,
};

/// Directory below the platform data directory, relative to the Posthaste root.
const SESSIONS_DIR: &str = "sessions";

const fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// How a tab's last run is presented, apart from a completed response.
///
/// A completed response is not stored here: [`SessionTab::recorded_response`]
/// names the history entry whose outcome backs the response pane, and `Idle` is
/// used in its place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionExecution {
    #[default]
    Idle,
    InFlight,
    Failed {
        message: String,
    },
    Cancelled,
}

/// One recorded tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTab {
    /// The draft exactly as the request pane held it.
    pub draft: RequestSpec,
    /// The saved request the tab is attached to, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_request: Option<RequestId>,
    /// The history entry the tab presents, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_history: Option<HistoryId>,
    /// The history entry whose stored outcome backs a completed response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_response: Option<HistoryId>,
    /// The number this tab showed while it held a blank default draft.
    #[serde(default)]
    pub blank_number: u32,
    /// The run's presentation when no response reference describes it.
    #[serde(default)]
    pub execution: SessionExecution,
}

/// The session document recorded for one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDocument {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The canonical root the document was written for.
    #[serde(default)]
    pub workspace_root: String,
    /// Index into [`Self::tabs`] of the active tab.
    #[serde(default)]
    pub active_tab: usize,
    /// The environment the workspace had selected, in three states: `None`
    /// when an earlier document never recorded one, `Some(None)` when the user
    /// cleared the selection, and `Some(Some(slug))` for a named environment.
    ///
    /// A slug that no longer names a loaded environment resolves to no
    /// environment at boot; the record stays until the next selection.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_active_environment"
    )]
    pub active_environment: Option<Option<String>>,
    #[serde(default)]
    pub tabs: Vec<SessionTab>,
}

/// Reads a present `active_environment` — including `null` — as `Some(..)`.
///
/// The derived `Option<Option<String>>` deserializer maps both `null` and an
/// absent field to `None`, which would erase the difference between "the user
/// cleared the selection" and "no document ever recorded one". The field's
/// `default` still covers the absent field, so a present value only has to keep
/// its inner option.
fn deserialize_active_environment<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

impl Versioned for SessionDocument {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

/// What loading a session document found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLoad {
    /// No document, or one that could not be read or parsed. The store stays
    /// writable, so the next write replaces it.
    Absent,
    /// A document for this workspace.
    Loaded(SessionDocument),
    /// A newer schema or another workspace's root. The document is left
    /// untouched for the rest of the process.
    Protected,
}

/// The machine-local session file for one workspace.
///
/// A disabled store has no path and treats every load as absent and every save
/// as a no-op, which keeps tests away from the real user profile.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: Option<PathBuf>,
    workspace_root: Option<String>,
    writable: bool,
}

impl SessionStore {
    /// Resolves the session document for `workspace_root` below the platform's
    /// per-user data directory.
    pub fn resolve(workspace_root: &Path) -> Self {
        Self::resolve_in(workspace_root, crate::paths::home())
    }

    /// The injectable form of [`Self::resolve`], taking the already-resolved
    /// product directory under the platform data directory.
    fn resolve_in(workspace_root: &Path, data_home: Option<PathBuf>) -> Self {
        let key = canonical_root(workspace_root);
        let Some(data_home) = data_home else {
            return Self::disabled();
        };
        let name = Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes());
        Self {
            path: Some(
                data_home
                    .join(SESSIONS_DIR)
                    .join(format!("workspace-{name}.json")),
            ),
            workspace_root: Some(key),
            writable: true,
        }
    }

    /// A store for an explicit path, without a root to validate against.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            workspace_root: None,
            writable: true,
        }
    }

    /// A store that neither loads nor writes anything.
    pub fn disabled() -> Self {
        Self {
            path: None,
            workspace_root: None,
            writable: false,
        }
    }

    /// The resolved file, when the store is enabled.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The canonical root this store was resolved for, when it has one.
    pub fn workspace_root(&self) -> Option<&str> {
        self.workspace_root.as_deref()
    }

    /// Whether [`Self::save`] may write. A protected document clears this.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// The root to record in a document built by this store.
    pub fn document_root(&self) -> String {
        self.workspace_root.clone().unwrap_or_default()
    }

    /// Reads the document, lowering the store's writability when the file must
    /// not be replaced.
    pub fn load(&mut self) -> SessionLoad {
        if !self.writable {
            return SessionLoad::Absent;
        }
        let Some(path) = self.path.as_deref() else {
            return SessionLoad::Absent;
        };
        // Residue from an interrupted write would otherwise accumulate beside
        // the document forever.
        if let Some(directory) = path.parent() {
            cleanup_temporary_files(directory);
        }
        // A directory or an unreadable path is treated like a missing document;
        // the next write replaces it.
        if !path.is_file() {
            return SessionLoad::Absent;
        }
        match read_versioned::<SessionDocument>(path) {
            Ok(document) => {
                let matches = self
                    .workspace_root
                    .as_deref()
                    .is_none_or(|root| roots_match(&document.workspace_root, root));
                if !matches {
                    self.writable = false;
                    return SessionLoad::Protected;
                }
                SessionLoad::Loaded(document)
            }
            Err(StorageError::UnsupportedSchema { .. }) => {
                // Never overwrite a document a newer build wrote.
                self.writable = false;
                SessionLoad::Protected
            }
            Err(StorageError::Parse { .. }) => {
                tracing::warn!(
                    path = %path.display(),
                    "ignoring malformed session document; the next write replaces it"
                );
                SessionLoad::Absent
            }
            // An unreadable file is convenience state, not workspace data.
            Err(_) => SessionLoad::Absent,
        }
    }

    /// Replaces the document atomically. A disabled or protected store does
    /// nothing, and a failure leaves the previous file in place.
    pub fn save(&self, document: &SessionDocument) -> Result<(), StorageError> {
        if !self.writable {
            return Ok(());
        }
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        write_document_atomic(path, document).map(|_| ())
    }
}

/// The canonical root a machine-local document is keyed by.
///
/// A workspace that does not exist yet is keyed by its resolved path, matching
/// the request index. Windows case is folded so the same directory cannot mint
/// two documents. History uses the same key, so one workspace keeps one
/// directory under `Posthaste/` per document kind.
pub(crate) fn canonical_root(root: &Path) -> String {
    let canonical =
        fs::canonicalize(root).unwrap_or_else(|_| match (root.parent(), root.file_name()) {
            (Some(parent), Some(name)) => fs::canonicalize(parent)
                .map(|parent| parent.join(name))
                .unwrap_or_else(|_| root.to_path_buf()),
            _ => root.to_path_buf(),
        });
    let key = canonical.to_string_lossy().into_owned();
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// Windows path comparison is case-insensitive; other platforms compare bytes.
fn roots_match(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.to_lowercase() == right.to_lowercase()
    } else {
        left == right
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiKeyLocation, HttpMethod, KeyValue, RequestAuth, RequestBody};

    fn draft() -> RequestSpec {
        let mut spec = RequestSpec::new("Secret draft");
        spec.method = HttpMethod::Post;
        spec.url = "https://api.example.com/items".into();
        spec.headers = vec![KeyValue::new("Authorization", "Bearer top-secret")];
        spec.query = vec![KeyValue::new("page", "2")];
        spec.body = RequestBody::Json(r#"{"token":"abc"}"#.into());
        spec.timeout_ms = Some(5_000);
        spec.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".into(),
            value: "another-secret".into(),
            location: ApiKeyLocation::Header,
        };
        spec
    }

    fn document(tabs: Vec<SessionTab>) -> SessionDocument {
        SessionDocument {
            schema_version: SCHEMA_VERSION,
            workspace_root: "C:\\workspace".into(),
            active_tab: 0,
            active_environment: Some(Some("defaults".into())),
            tabs,
        }
    }

    fn tab() -> SessionTab {
        SessionTab {
            draft: draft(),
            opened_request: None,
            selected_history: None,
            recorded_response: None,
            blank_number: 0,
            execution: SessionExecution::Idle,
        }
    }

    #[test]
    fn a_draft_round_trips_verbatim_and_absent_references_are_omitted() {
        let document = document(vec![tab()]);
        let json = serde_json::to_string(&document).expect("serialize session");

        assert!(!json.contains("opened_request"));
        assert!(!json.contains("selected_history"));
        assert!(!json.contains("recorded_response"));

        let parsed: SessionDocument = serde_json::from_str(&json).expect("deserialize session");
        assert_eq!(parsed, document);
        let parsed_draft = &parsed.tabs[0].draft;
        assert_eq!(parsed_draft.headers, draft().headers);
        assert_eq!(parsed_draft.body, draft().body);
        assert_eq!(parsed_draft.auth, draft().auth);
    }

    #[test]
    fn execution_variants_round_trip() {
        for execution in [
            SessionExecution::Idle,
            SessionExecution::InFlight,
            SessionExecution::Failed {
                message: "connection refused".into(),
            },
            SessionExecution::Cancelled,
        ] {
            let mut recorded = tab();
            recorded.execution = execution.clone();
            let json = serde_json::to_string(&recorded).expect("serialize tab");
            let parsed: SessionTab = serde_json::from_str(&json).expect("deserialize tab");
            assert_eq!(parsed.execution, execution);
        }
    }

    #[test]
    fn the_active_environment_round_trips_in_all_three_states() {
        for state in [None, Some(None), Some(Some("production".to_owned()))] {
            let mut recorded = document(vec![tab()]);
            recorded.active_environment = state.clone();
            let json = serde_json::to_string(&recorded).expect("serialize session");
            if state.is_none() {
                assert!(
                    !json.contains("active_environment"),
                    "an absent record is omitted: {json}"
                );
            }
            let parsed: SessionDocument = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed.active_environment, state);
        }
    }

    #[test]
    fn an_omitted_active_environment_is_distinct_from_an_explicit_null() {
        let omitted = format!(r#"{{"schema_version": {}, "tabs": []}}"#, SCHEMA_VERSION);
        let explicit = format!(
            r#"{{"schema_version": {}, "active_environment": null, "tabs": []}}"#,
            SCHEMA_VERSION
        );

        let omitted: SessionDocument = serde_json::from_str(&omitted).expect("deserialize");
        let explicit: SessionDocument = serde_json::from_str(&explicit).expect("deserialize");

        assert_eq!(omitted.active_environment, None);
        assert_eq!(
            explicit.active_environment,
            Some(None),
            "a cleared selection must not read as never recorded"
        );
    }

    #[test]
    fn the_same_root_maps_to_the_same_path_and_different_roots_do_not() {
        let first = tempfile::tempdir().expect("first root");
        let second = tempfile::tempdir().expect("second root");
        let data = tempfile::tempdir().expect("data dir");

        let store = SessionStore::resolve_in(
            first.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        );
        let again = SessionStore::resolve_in(
            first.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        );
        let other = SessionStore::resolve_in(
            second.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        );

        assert_eq!(
            store.path(),
            again.path(),
            "one workspace keeps one session"
        );
        assert_ne!(store.path(), other.path(), "another root gets its own file");
        assert_eq!(
            store
                .path()
                .expect("path")
                .parent()
                .and_then(|dir| dir.file_name())
                .map(|name| name.to_string_lossy().into_owned()),
            Some("sessions".to_owned()),
            "the session lives in the shared sessions directory"
        );
        assert!(
            store
                .path()
                .expect("path")
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .starts_with("workspace-")
        );
    }

    #[test]
    fn a_document_naming_another_root_is_protected() {
        let root = tempfile::tempdir().expect("root");
        let data = tempfile::tempdir().expect("data dir");
        let path = SessionStore::resolve_in(
            root.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        )
        .path()
        .expect("path")
        .to_path_buf();
        let mut recorded = document(vec![tab()]);
        recorded.workspace_root = "D:\\elsewhere".into();
        fs::create_dir_all(path.parent().expect("parent")).expect("session dir");
        fs::write(
            &path,
            serde_json::to_vec(&recorded).expect("encode document"),
        )
        .expect("write document");

        let mut store = SessionStore::resolve_in(
            root.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        );
        assert_eq!(store.load(), SessionLoad::Protected);
        assert!(!store.is_writable());
        assert!(store.save(&document(vec![tab()])).is_ok());
        let saved: SessionDocument =
            serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
        assert_eq!(
            saved.workspace_root, "D:\\elsewhere",
            "a protected document is left untouched"
        );
    }

    #[test]
    fn a_missing_document_loads_as_absent_and_stays_writable() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = SessionStore::at(dir.path().join("session.json"));

        assert_eq!(store.load(), SessionLoad::Absent);
        assert!(store.is_writable());
    }

    #[test]
    fn a_malformed_document_loads_as_absent_and_the_next_save_replaces_it() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("session.json");
        fs::write(&path, b"{ not json").expect("write malformed");
        let mut store = SessionStore::at(&path);

        assert_eq!(store.load(), SessionLoad::Absent);
        assert!(store.is_writable());

        let replacement = document(vec![tab()]);
        store
            .save(&replacement)
            .expect("replace malformed document");
        let parsed: SessionDocument = serde_json::from_slice(&fs::read(&path).expect("read"))
            .expect("the malformed file was replaced");
        assert_eq!(parsed, replacement);
    }

    #[test]
    fn a_newer_schema_is_protected_and_never_written() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("session.json");
        let newer = format!(
            r#"{{"schema_version": {}, "workspace_root": "", "tabs": []}}"#,
            SCHEMA_VERSION + 1
        );
        fs::write(&path, newer.as_bytes()).expect("write newer document");
        let mut store = SessionStore::at(&path);

        assert_eq!(store.load(), SessionLoad::Protected);
        assert!(!store.is_writable());
        store
            .save(&document(vec![tab()]))
            .expect("a protected save is a no-op");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            newer,
            "the newer document is untouched"
        );
    }

    #[test]
    fn an_unreadable_document_loads_as_absent_and_stays_writable() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("session.json");
        fs::create_dir(&path).expect("replace the file with a directory");
        let mut store = SessionStore::at(&path);

        assert_eq!(store.load(), SessionLoad::Absent);
        assert!(store.is_writable());
    }

    #[test]
    fn save_round_trips_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("dir");
        let session_dir = dir.path().join("sessions");
        let path = session_dir.join("workspace-test.json");
        let store = SessionStore::at(&path);
        let recorded = document(vec![tab()]);

        store.save(&recorded).expect("save session");

        let mut reader = SessionStore::at(&path);
        assert_eq!(reader.load(), SessionLoad::Loaded(recorded));
        let files: Vec<_> = fs::read_dir(&session_dir)
            .expect("read session dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(files.len(), 1, "only the session document remains");
    }

    #[test]
    fn loading_removes_leftover_temporary_files_from_the_session_directory() {
        let dir = tempfile::tempdir().expect("dir");
        let session_dir = dir.path().join("sessions");
        fs::create_dir_all(&session_dir).expect("session dir");
        let path = session_dir.join("workspace-test.json");
        SessionStore::at(&path)
            .save(&document(vec![tab()]))
            .expect("save session");
        let residue = session_dir.join(".posthaste-abc123.posthaste-tmp");
        fs::write(&residue, "{").expect("write residue");
        let unrelated = session_dir.join(".posthaste-notes.txt");
        fs::write(&unrelated, "keep me").expect("write unrelated file");

        let mut reader = SessionStore::at(&path);
        assert!(matches!(reader.load(), SessionLoad::Loaded(_)));

        assert!(
            !residue.exists(),
            "residue from an interrupted write is removed"
        );
        assert_eq!(
            fs::read_to_string(&unrelated).expect("unrelated file"),
            "keep me",
            "only the temporary suffix is removed"
        );
    }

    #[test]
    fn a_disabled_store_neither_reads_nor_writes() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = SessionStore::disabled();

        assert_eq!(store.load(), SessionLoad::Absent);
        store
            .save(&document(vec![tab()]))
            .expect("disabled save is a no-op");
        assert!(fs::read_dir(dir.path()).expect("read").next().is_none());
    }

    // Keeps the unused helper import honest while the draft fixture is shared.
    #[test]
    fn history_ids_round_trip_by_reference() {
        let mut recorded = tab();
        recorded.selected_history = Some(HistoryId::new());
        recorded.recorded_response = Some(HistoryId::new());
        recorded.opened_request = Some(RequestId::new());
        let json = serde_json::to_string(&recorded).expect("serialize");
        assert!(json.contains("selected_history"));
        assert!(json.contains("recorded_response"));
        assert!(json.contains("opened_request"));
        let parsed: SessionTab = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, recorded);
    }
}
