//! Machine-local workspace navigation document: which workspaces this
//! installation has opened recently.
//!
//! The document lives beside `preferences.json` under the per-user data
//! directory, never inside a workspace, because switching directories must not
//! travel with a synced or shared workspace. It is versioned and written with
//! the same atomic temp-file-and-rename path as every other Posthaste document.
//!
//! Loading is tolerant: a missing, unreadable, or malformed document yields an
//! empty list and never blocks startup. A document written by a newer build is
//! left untouched for the rest of the session. Only an explicit workspace
//! action records a path; startup resolution reads the document and never
//! writes it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::SCHEMA_VERSION;
use crate::preferences::PreferencesStore;
use crate::storage::{
    StorageError, StorageIssue, Versioned, cleanup_temporary_files, normalize_directory_path,
    read_versioned, same_directory, write_document_atomic,
};

/// File name of the recents document inside its directory.
pub const RECENTS_FILE: &str = "workspaces.json";

/// Environment variable naming the preferences directory, which also holds
/// the recents document. Reusing it keeps the two machine-local documents
/// together.
pub const RECENTS_DIR_ENV: &str = crate::preferences::PREFERENCES_DIR_ENV;

/// How many recent workspaces the document keeps.
pub const MAX_RECENT_WORKSPACES: usize = 8;

const fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// The workspace navigation document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentWorkspaces {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The last workspace opened through a workspace action, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_opened: Option<PathBuf>,
    /// Up to [`MAX_RECENT_WORKSPACES`] workspaces, most recent first.
    #[serde(default)]
    pub recent: Vec<PathBuf>,
}

impl Default for RecentWorkspaces {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            last_opened: None,
            recent: Vec::new(),
        }
    }
}

impl Versioned for RecentWorkspaces {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl RecentWorkspaces {
    /// Records `path` as the last opened workspace and moves it to the front
    /// of the recent list.
    ///
    /// The path is stored absolute and canonicalized when the directory exists.
    /// A workspace already in the list is moved rather than added again, and
    /// the list is truncated to [`MAX_RECENT_WORKSPACES`].
    pub fn record(&mut self, path: &Path) {
        let path = normalize_directory_path(path);
        self.recent.retain(|entry| !same_directory(entry, &path));
        self.recent.insert(0, path.clone());
        self.recent.truncate(MAX_RECENT_WORKSPACES);
        self.last_opened = Some(path);
    }

    /// Removes every entry naming `path`, including the last-opened path.
    pub fn forget(&mut self, path: &Path) {
        self.recent.retain(|entry| !same_directory(entry, path));
        if self
            .last_opened
            .as_deref()
            .is_some_and(|last| same_directory(last, path))
        {
            self.last_opened = None;
        }
    }

    /// Returns the document with paths normalized, duplicates removed keeping
    /// the first occurrence, and the list capped.
    pub fn normalized(mut self) -> Self {
        let mut recent: Vec<PathBuf> = Vec::with_capacity(self.recent.len());
        for entry in self.recent {
            let entry = normalize_directory_path(&entry);
            if !recent.iter().any(|kept| same_directory(kept, &entry)) {
                recent.push(entry);
            }
        }
        recent.truncate(MAX_RECENT_WORKSPACES);
        self.recent = recent;
        self
    }
}

/// What loading the recents document produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRecents {
    /// The effective document: the loaded one normalized, or an empty one.
    pub document: RecentWorkspaces,
    /// A non-fatal problem with the file, for logging. A missing file and a
    /// newer schema are not issues.
    pub issue: Option<StorageIssue>,
}

impl LoadedRecents {
    fn empty() -> Self {
        Self {
            document: RecentWorkspaces::default(),
            issue: None,
        }
    }
}

/// The machine-local recents file.
///
/// A disabled store (no platform data directory and no override) loads an empty
/// document and ignores saves, which keeps tests and a profile-less app
/// independent of the user's real recents.
#[derive(Debug, Clone)]
pub struct RecentsStore {
    root: Option<PathBuf>,
    writable: bool,
}

impl RecentsStore {
    /// Resolves the document directory from `POSTHASTE_PREFERENCES`, or the
    /// platform per-user data directory beneath `Posthaste/preferences` — the
    /// directory that also holds `preferences.json`.
    pub fn resolve() -> Self {
        Self::resolve_from(
            crate::paths::env_value(RECENTS_DIR_ENV),
            crate::paths::home(),
        )
    }

    fn resolve_from(override_dir: Option<OsString>, data_home: Option<PathBuf>) -> Self {
        let root = PreferencesStore::resolve_from(override_dir, data_home)
            .root()
            .map(Path::to_path_buf);
        Self {
            root,
            writable: true,
        }
    }

    /// A store rooted at an explicit directory. The document is
    /// `workspaces.json` inside it.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            writable: true,
        }
    }

    /// A store that loads an empty document and writes nothing.
    pub fn disabled() -> Self {
        Self {
            root: None,
            writable: false,
        }
    }

    /// The document directory, when the store is enabled.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The document path, when the store is enabled.
    pub fn path(&self) -> Option<PathBuf> {
        self.root.as_deref().map(|root| root.join(RECENTS_FILE))
    }

    /// Whether [`Self::save`] may write. A document from a newer build clears
    /// this for the rest of the session.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Reads the document. A missing file, an unreadable file, and a malformed
    /// file all yield an empty document; the first is silent, the others report
    /// a logging issue. A newer schema yields an empty document and makes the
    /// store refuse writes.
    pub fn load(&mut self) -> LoadedRecents {
        if !self.writable {
            return LoadedRecents::empty();
        }
        let Some(path) = self.path() else {
            return LoadedRecents::empty();
        };
        if let Some(directory) = path.parent() {
            cleanup_temporary_files(directory);
        }
        if !path.is_file() {
            return LoadedRecents::empty();
        }
        match read_versioned::<RecentWorkspaces>(&path) {
            Ok(document) => LoadedRecents {
                document: document.normalized(),
                issue: None,
            },
            Err(StorageError::UnsupportedSchema { .. }) => {
                // Never overwrite a document a newer build wrote.
                self.writable = false;
                LoadedRecents::empty()
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "ignoring unreadable workspaces document; starting with no recents"
                );
                let issue = StorageIssue::from_error(&error);
                LoadedRecents {
                    document: RecentWorkspaces::default(),
                    issue,
                }
            }
        }
    }

    /// Replaces the document atomically. A disabled or write-protected store
    /// does nothing, and a failure leaves the previous file in place.
    pub fn save(&self, document: &RecentWorkspaces) -> Result<(), StorageError> {
        if !self.writable {
            return Ok(());
        }
        let Some(path) = self.path() else {
            return Ok(());
        };
        write_document_atomic(&path, document).map(|_| ())
    }

    /// Records `path` in `document` and writes the document.
    ///
    /// The list is updated even when the write fails, so the menu is correct
    /// for the rest of the session; the error is returned for logging.
    pub fn record(&self, document: &mut RecentWorkspaces, path: &Path) -> Result<(), StorageError> {
        document.record(path);
        self.save(document)
    }

    /// Forgets `path` in `document` and writes the document.
    pub fn forget(&self, document: &mut RecentWorkspaces, path: &Path) -> Result<(), StorageError> {
        document.forget(path);
        self.save(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store_in(dir: &tempfile::TempDir) -> RecentsStore {
        RecentsStore::at(dir.path())
    }

    #[test]
    fn a_missing_document_loads_empty_without_an_issue() {
        let dir = tempfile::tempdir().expect("dir");
        let mut store = store_in(&dir);

        let loaded = store.load();

        assert_eq!(loaded.document, RecentWorkspaces::default());
        assert_eq!(loaded.issue, None);
        assert!(store.is_writable());
    }

    #[test]
    fn the_document_round_trips_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(&dir);
        let payments = dir.path().join("payments-api");
        let scratch = dir.path().join("Scratch");
        fs::create_dir(&payments).expect("create payments");
        fs::create_dir(&scratch).expect("create scratch");
        let mut written = RecentWorkspaces::default();
        written.record(&payments);
        written.record(&scratch);

        store.save(&written).expect("save recents");

        let mut reader = store_in(&dir);
        assert_eq!(reader.load().document, written);
        // The directories the test recorded are siblings of the document, so
        // only the files matter here.
        let files: Vec<_> = fs::read_dir(dir.path())
            .expect("read recents dir")
            .map(|entry| entry.expect("entry"))
            .filter(|entry| entry.file_type().expect("file type").is_file())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(files.len(), 1, "only the recents document remains");
        assert_eq!(files[0], RECENTS_FILE);
    }

    #[test]
    fn a_corrupt_document_loads_empty_and_stays_writable() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(RECENTS_FILE);
        fs::write(&path, b"{ not json").expect("write malformed");
        let mut store = store_in(&dir);

        let loaded = store.load();

        assert_eq!(loaded.document, RecentWorkspaces::default());
        assert!(loaded.issue.is_some());
        assert!(store.is_writable());

        store
            .record(&mut RecentWorkspaces::default(), &dir.path().join("fresh"))
            .expect("a corrupt document is rewritten");
        assert!(path.is_file());
    }

    #[test]
    fn a_newer_schema_loads_empty_and_protects_the_document() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(RECENTS_FILE);
        let newer = format!(r#"{{"schema_version": {}}}"#, SCHEMA_VERSION + 1);
        fs::write(&path, &newer).expect("write newer");
        let mut store = store_in(&dir);

        let loaded = store.load();

        assert_eq!(loaded.document, RecentWorkspaces::default());
        assert!(!store.is_writable());
        store
            .save(&RecentWorkspaces::default())
            .expect("a protected save is a no-op");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            newer,
            "the newer document is untouched"
        );
    }

    #[test]
    fn a_disabled_store_loads_empty_and_writes_nothing() {
        let mut store = RecentsStore::disabled();

        assert_eq!(store.load().document, RecentWorkspaces::default());
        store
            .save(&RecentWorkspaces::default())
            .expect("disabled save is a no-op");
        assert!(!store.is_writable());
    }

    #[test]
    fn recording_is_most_recent_first_and_capped() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(&dir);
        let mut document = RecentWorkspaces::default();
        let mut paths = Vec::new();
        for index in 0..9 {
            let path = dir.path().join(format!("workspace-{index}"));
            fs::create_dir(&path).expect("create workspace dir");
            store.record(&mut document, &path).expect("record");
            paths.push(path);
        }

        assert_eq!(document.recent.len(), MAX_RECENT_WORKSPACES);
        // The document stores the directory as the platform reports it, which
        // resolves the temporary directory's symlinks and short names.
        let newest = normalize_directory_path(&paths[8]);
        assert_eq!(document.recent[0], newest);
        assert_eq!(document.last_opened.as_deref(), Some(newest.as_path()));
        let dropped = normalize_directory_path(&paths[0]);
        assert!(
            !document.recent.iter().any(|entry| entry == &dropped),
            "the earliest entry was dropped"
        );

        store.record(&mut document, &paths[1]).expect("reopen");
        let reopened = normalize_directory_path(&paths[1]);
        assert_eq!(document.recent[0], reopened);
        assert_eq!(
            document
                .recent
                .iter()
                .filter(|entry| same_directory(entry, &reopened))
                .count(),
            1,
            "a reopened workspace appears once"
        );
        assert_eq!(document.recent.len(), MAX_RECENT_WORKSPACES);
        assert_eq!(document.last_opened.as_deref(), Some(reopened.as_path()));
    }

    #[test]
    fn recording_the_same_directory_twice_appears_once() {
        let dir = tempfile::tempdir().expect("dir");
        let store = store_in(&dir);
        let workspace = dir.path().join("Payments");
        fs::create_dir(&workspace).expect("create workspace");
        let mut document = RecentWorkspaces::default();

        store.record(&mut document, &workspace).expect("record");
        store
            .record(&mut document, &workspace)
            .expect("record again");

        assert_eq!(document.recent.len(), 1);
    }

    #[test]
    fn forgetting_removes_the_entry_and_the_last_opened_path() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("Payments");
        fs::create_dir(&workspace).expect("create workspace");
        let other = dir.path().join("Scratch");
        fs::create_dir(&other).expect("create other");
        let mut document = RecentWorkspaces::default();
        document.record(&workspace);
        document.record(&other);
        document.record(&workspace);

        document.forget(&workspace);

        assert_eq!(document.recent.len(), 1);
        assert_eq!(document.recent[0], normalize_directory_path(&other));
        assert_eq!(document.last_opened, None);
    }

    #[test]
    fn resolve_honors_the_directory_override() {
        let override_dir = tempfile::tempdir().expect("override");
        let data_dir = tempfile::tempdir().expect("data");

        let store = RecentsStore::resolve_from(
            Some(override_dir.path().as_os_str().to_owned()),
            Some(data_dir.path().to_path_buf()),
        );

        assert_eq!(store.root(), Some(override_dir.path()));
        assert_eq!(store.path(), Some(override_dir.path().join(RECENTS_FILE)));
    }

    #[test]
    fn resolve_lives_beside_the_preferences_document() {
        let data_dir = tempfile::tempdir().expect("data");

        let store = RecentsStore::resolve_from(
            None,
            crate::paths::home_in(Some(data_dir.path().to_path_buf())),
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
}
