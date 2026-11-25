//! File-based workspace storage.
//!
//! A workspace is a directory of plain JSON files:
//!
//! ```text
//! <workspace>/
//!   workspace.json
//!   collections/
//!     <slug>/
//!       collection.json          # metadata + flat folder list
//!       requests/
//!         <uuid>.json            # optional `folder_id` reference
//!   environments/
//!     <slug>.json
//!   files/
//!     <uuid>[.<ext>]           # picked file assets, copied in on first import
//!   .gitattributes
//!   .gitignore
//! ```
//!
//! The layout is meant to be committed: files a request references live inside
//! the workspace, and the two dotfiles keep a checkout byte-stable across
//! platforms. Bootstrap writes a dotfile only when it is absent and never runs
//! git.
//!
//! A collection's folders are stored as a flat list inside `collection.json`.
//! Each entry has a stable UUID identifier, a display name, and an optional
//! parent identifier; a request document records the identifier of the folder
//! it belongs to, or omits the field at the collection root. Renaming or moving
//! a folder therefore rewrites one small document, and no file or directory is
//! ever renamed because of a folder or request move. A collection with no
//! folders omits the list entirely, so its file is byte-identical to a
//! pre-folder document.
//!
//! Reads are tolerant: a file that cannot be parsed, or that declares a newer
//! `schema_version`, is reported through [`StorageIssue`] and skipped without
//! being modified. The root `workspace.json` is the exception: it is renamed to
//! `workspace.json.corrupt-<unix-millis>` before a default workspace replaces
//! it, so the damaged bytes are never overwritten.
//!
//! Writes are atomic against process interruption: the document is serialized
//! first, written to a temporary file in the destination directory, then renamed
//! over the destination. An interrupted write therefore leaves either the
//! previous file or the new file, never partial content.
//!
//! Atomicity here is rename-based only. Nothing is flushed to the device and no
//! directory is synced, so a power loss or an operating-system crash can still
//! lose or truncate the newest write. Adding that barrier would cost a
//! `sync_all` per autosave, and this storage model does not need it: a missing
//! or truncated document loads tolerantly and is re-written by the next save.
//! Power-loss durability is therefore not claimed, and no documentation or API
//! here should promise it.
//!
//! Request metadata used by the sidebar is cached outside the workspace, at
//! `<platform cache>/Posthaste/index/<workspace-key>/<collection-id>.json`, where
//! `workspace-key` is a UUID v5 of the canonicalized workspace root. The cache
//! never contains user-authored data that is not also in a request document:
//! deleting it only makes the next startup read every request file.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    Collection, Environment, FileFingerprint, Folder, FolderId, HttpMethod, RequestId,
    RequestProtocol, RequestSpec, RequestSummary, SCHEMA_VERSION, Workspace,
};
use crate::paths;
use crate::url_query::join_url_query;

/// Directory name used when no workspace name can be derived from a file.
pub const DEFAULT_WORKSPACE_NAME: &str = "My Workspace";
/// Maximum number of characters in a generated collection slug.
pub const MAX_SLUG_CHARS: usize = 48;

const WORKSPACE_FILE: &str = "workspace.json";
/// Marker inserted before the timestamp of a preserved damaged document.
const QUARANTINE_MARKER: &str = ".corrupt-";
const COLLECTION_FILE: &str = "collection.json";
const COLLECTIONS_DIR: &str = "collections";
const REQUESTS_DIR: &str = "requests";
const ENVIRONMENTS_DIR: &str = "environments";
/// Directory holding files the user picked into a request body or part. They
/// live inside the workspace so a shared checkout carries its own assets.
const FILES_DIR: &str = "files";

/// Root file that pins line endings for every checkout of a versioned
/// workspace.
const GITATTRIBUTES_FILE: &str = ".gitattributes";
/// Root file that hides residue and operating-system metadata from git.
const GITIGNORE_FILE: &str = ".gitignore";

/// Every document is written with `\n`; this keeps a checkout on a platform
/// with another line-ending default from rewriting them.
pub const GITATTRIBUTES_CONTENT: &str = "* text=auto eol=lf\n";

/// The app's own transients plus metadata files the platforms drop in.
pub const GITIGNORE_CONTENT: &str = ".posthaste-tmp\nhistory/\nThumbs.db\n.DS_Store\n";

/// Extension of every workspace document written by this build.
pub(crate) const DOCUMENT_EXTENSION: &str = "json";

/// Version of the derived request index. Independent of the document schema.
///
/// Version 2 stores the composed URL, so searching a query parameter works
/// before the request file is read. Version 1 entries hold the base URL only;
/// they are rebuilt once after the upgrade.
const INDEX_VERSION: u32 = 2;
/// Directory below the platform cache directory used for derived indexes.
const CACHE_INDEX_DIR: &str = "index";
/// Namespace for the UUID v5 workspace key.
const WORKSPACE_KEY_NAMESPACE: Uuid = Uuid::from_u128(0x6ba7b810_9dad_11d1_80b4_00c04fd430c8);

/// A workspace-level failure that prevents loading or saving.
#[derive(Debug, Error)]
pub enum StorageError {
    /// No platform data directory is available and no override was set.
    #[error(
        "cannot determine the user data directory; set POSTHASTE_WORKSPACE to a workspace path"
    )]
    NoDataDir,
    /// A filesystem operation failed.
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    /// A file could not be parsed in its container format.
    #[error("{path}: invalid {format}: {message}")]
    Parse {
        path: PathBuf,
        format: &'static str,
        message: String,
    },
    /// A document was written by a newer version of Posthaste.
    #[error("{path}: schema_version {found} is newer than supported version {supported}")]
    UnsupportedSchema {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    /// A document or path is structurally invalid.
    #[error("{path}: {reason}")]
    InvalidEntry { path: PathBuf, reason: String },
    /// A document could not be serialized.
    #[error("{path}: cannot serialize JSON: {message}")]
    Serialize { path: PathBuf, message: String },
}

impl StorageError {
    /// The file or directory this error refers to, when there is one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::NoDataDir => None,
            Self::Io { path, .. }
            | Self::Parse { path, .. }
            | Self::UnsupportedSchema { path, .. }
            | Self::InvalidEntry { path, .. }
            | Self::Serialize { path, .. } => Some(path),
        }
    }

    /// The error detail without the path prefix.
    pub fn detail(&self) -> String {
        match self {
            Self::NoDataDir => {
                "cannot determine the user data directory; set POSTHASTE_WORKSPACE to a workspace path"
                    .into()
            }
            Self::Io { message, .. } => message.clone(),
            Self::Parse {
                format, message, ..
            } => format!("invalid {format}: {message}"),
            Self::UnsupportedSchema {
                found, supported, ..
            } => format!("schema_version {found} is newer than supported version {supported}"),
            Self::InvalidEntry { reason, .. } => reason.clone(),
            Self::Serialize { message, .. } => format!("cannot serialize JSON: {message}"),
        }
    }
}

/// A non-fatal problem with one file, reported while loading a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageIssue {
    /// Path of the affected file or directory.
    pub path: PathBuf,
    /// Human-readable explanation.
    pub message: String,
}

/// The outcome of writing one or more documents.
///
/// A save names every document it wrote and every document it could not write.
/// A non-empty [`Self::failures`] list is a partial failure; callers decide how
/// visible it should be. There is no separate channel that is always empty.
#[derive(Debug, Default)]
pub struct SaveReport {
    /// Documents replaced on disk, in the order they were written.
    pub written: Vec<PathBuf>,
    /// Documents that were not written, with the reason.
    pub failures: Vec<SaveFailure>,
}

/// One document a save could not write.
#[derive(Debug)]
pub struct SaveFailure {
    /// Path of the document that was not written.
    pub path: PathBuf,
    /// Why the write failed.
    pub error: StorageError,
}

impl SaveReport {
    /// A report for one document that was written.
    pub fn written(path: PathBuf) -> Self {
        Self {
            written: vec![path],
            failures: Vec::new(),
        }
    }

    /// Records the outcome of another save in this report.
    pub fn append(&mut self, other: Self) {
        self.written.extend(other.written);
        self.failures.extend(other.failures);
    }

    /// Records one document's outcome, naming `fallback` when the error does
    /// not carry a path of its own.
    fn absorb(&mut self, result: Result<Self, StorageError>, fallback: PathBuf) {
        match result {
            Ok(report) => self.append(report),
            Err(error) => {
                let path = error.path().unwrap_or(&fallback).to_path_buf();
                self.failures.push(SaveFailure { path, error });
            }
        }
    }

    /// Whether every requested document was written.
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failed documents as load-style issues, for display.
    pub fn into_issues(self) -> Vec<StorageIssue> {
        self.failures
            .into_iter()
            .map(|failure| StorageIssue {
                path: failure.path,
                message: failure.error.detail(),
            })
            .collect()
    }
}

impl StorageIssue {
    pub(crate) fn from_error(error: &StorageError) -> Option<Self> {
        Some(Self {
            path: error.path()?.to_path_buf(),
            message: error.detail(),
        })
    }
}

impl fmt::Display for StorageIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// A request known to the session.
///
/// Only [`RequestEntry::Loaded`] carries the full document. Code that writes a
/// request therefore cannot accidentally rewrite an unloaded entry: the save
/// path requires a [`RequestSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestEntry {
    /// The complete document was read during this session.
    Loaded(Box<RequestSpec>),
    /// Metadata came from the cache index; the document is still unread.
    Unloaded {
        /// Fields needed to render and find the request.
        summary: RequestSummary,
        /// Identifies the file the summary describes.
        fingerprint: FileFingerprint,
    },
}

impl RequestEntry {
    /// The request identifier, which is also its filename stem.
    pub fn id(&self) -> RequestId {
        match self {
            Self::Loaded(request) => request.id,
            Self::Unloaded { summary, .. } => summary.id,
        }
    }

    /// The display name.
    pub fn name(&self) -> &str {
        match self {
            Self::Loaded(request) => &request.name,
            Self::Unloaded { summary, .. } => &summary.name,
        }
    }

    /// The HTTP method.
    pub fn method(&self) -> HttpMethod {
        match self {
            Self::Loaded(request) => request.method,
            Self::Unloaded { summary, .. } => summary.method,
        }
    }

    /// The request protocol.
    pub fn protocol(&self) -> RequestProtocol {
        match self {
            Self::Loaded(request) => request.protocol,
            Self::Unloaded { summary, .. } => summary.protocol,
        }
    }

    /// The stored URL, before variable resolution.
    ///
    /// A loaded request stores only its base URL; use [`Self::search_url`] for
    /// the text a search should match.
    pub fn url(&self) -> &str {
        match self {
            Self::Loaded(request) => &request.url,
            Self::Unloaded { summary, .. } => &summary.url,
        }
    }

    /// The URL a search matches: the base with its enabled query rows composed
    /// on, which is also what the cached summary of an unloaded request holds.
    pub fn search_url(&self) -> Cow<'_, str> {
        match self {
            Self::Loaded(request) if !request.query.is_empty() => {
                Cow::Owned(join_url_query(&request.url, &request.query))
            }
            Self::Loaded(request) => Cow::Borrowed(&request.url),
            Self::Unloaded { summary, .. } => Cow::Borrowed(&summary.url),
        }
    }

    /// The folder reference, or `None` at the collection root.
    pub fn folder_id(&self) -> Option<FolderId> {
        match self {
            Self::Loaded(request) => request.folder_id,
            Self::Unloaded { summary, .. } => summary.folder_id,
        }
    }

    /// The full document, when it has been loaded.
    pub fn spec(&self) -> Option<&RequestSpec> {
        match self {
            Self::Loaded(request) => Some(request),
            Self::Unloaded { .. } => None,
        }
    }

    /// The full document for mutation, when it has been loaded.
    pub fn spec_mut(&mut self) -> Option<&mut RequestSpec> {
        match self {
            Self::Loaded(request) => Some(request),
            Self::Unloaded { .. } => None,
        }
    }

    /// Changes only the folder reference, keeping an unloaded entry unloaded.
    pub fn set_folder_id(&mut self, folder_id: Option<FolderId>) {
        match self {
            Self::Loaded(request) => request.folder_id = folder_id,
            Self::Unloaded { summary, .. } => summary.folder_id = folder_id,
        }
    }
}

/// A collection and its request entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCollection {
    /// Collection metadata; `collection.id` is the directory slug.
    pub collection: Collection,
    /// Requests sorted by display name, then id. Entries may be unloaded.
    pub requests: Vec<RequestEntry>,
}

impl LoadedCollection {
    /// The collection's folder registry.
    pub fn folders(&self) -> &[Folder] {
        &self.collection.folders
    }

    /// The folder entry with `id`, if this collection has one.
    pub fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.collection.folder(id)
    }

    /// Direct child folders of `parent_id`, ordered by lowercase display name
    /// and then identifier. `None` selects the collection root.
    pub fn folder_children(&self, parent_id: Option<FolderId>) -> Vec<&Folder> {
        let mut children: Vec<&Folder> = self
            .collection
            .folders
            .iter()
            .filter(|folder| folder.parent_id == parent_id)
            .collect();
        children.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        children
    }

    /// Requests directly inside `parent_id`, ordered by lowercase display name
    /// and then identifier. `None` selects the collection root.
    pub fn folder_requests(&self, parent_id: Option<FolderId>) -> Vec<&RequestEntry> {
        let mut requests: Vec<&RequestEntry> = self
            .requests
            .iter()
            .filter(|request| request.folder_id() == parent_id)
            .collect();
        requests.sort_by(|a, b| {
            a.name()
                .to_lowercase()
                .cmp(&b.name().to_lowercase())
                .then_with(|| a.id().cmp(&b.id()))
        });
        requests
    }

    /// `id` and every folder below it, depth first in sibling order. Empty
    /// when the folder is not part of this collection.
    pub fn folder_subtree_ids(&self, id: FolderId) -> Vec<FolderId> {
        if self.folder(id).is_none() {
            return Vec::new();
        }
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![id];
        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            ids.push(current);
            for child in self.folder_children(Some(current)).into_iter().rev() {
                stack.push(child.id);
            }
        }
        ids
    }

    /// Ancestors of `id`, outermost first, excluding `id` itself. Empty for a
    /// root folder or an unknown identifier.
    pub fn folder_ancestors(&self, id: FolderId) -> Vec<FolderId> {
        let mut ancestors = Vec::new();
        let mut seen = HashSet::from([id]);
        let mut current = self.folder(id).and_then(|folder| folder.parent_id);
        while let Some(folder_id) = current {
            if !seen.insert(folder_id) {
                break;
            }
            ancestors.push(folder_id);
            current = self.folder(folder_id).and_then(|folder| folder.parent_id);
        }
        ancestors.reverse();
        ancestors
    }

    /// Number of requests in `id` and every folder below it.
    pub fn folder_request_count(&self, id: FolderId) -> usize {
        let subtree: HashSet<FolderId> = self.folder_subtree_ids(id).into_iter().collect();
        if subtree.is_empty() {
            return 0;
        }
        self.requests
            .iter()
            .filter(|request| {
                request
                    .folder_id()
                    .is_some_and(|folder_id| subtree.contains(&folder_id))
            })
            .count()
    }
}

/// An environment and the slug of its file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedEnvironment {
    /// The environment; `slug` is the filename stem.
    pub slug: String,
    /// Environment metadata and variables.
    pub environment: Environment,
}

/// The result of loading a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedWorkspace {
    /// Workspace metadata.
    pub workspace: Workspace,
    /// Collections sorted by `order`, then display name.
    pub collections: Vec<LoadedCollection>,
    /// Environments sorted by display name, then slug.
    pub environments: Vec<LoadedEnvironment>,
    /// Per-file problems; the affected entries are absent from the lists above.
    pub issues: Vec<StorageIssue>,
}

/// One collection's derived metadata, stored outside the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
struct RequestIndex {
    index_version: u32,
    collection_id: String,
    #[serde(default)]
    entries: Vec<RequestIndexEntry>,
}

impl RequestIndex {
    fn new(collection_id: &str, mut entries: Vec<RequestIndexEntry>) -> Self {
        entries.sort_by_key(|a| a.id);
        Self {
            index_version: INDEX_VERSION,
            collection_id: collection_id.to_owned(),
            entries,
        }
    }
}

/// One cached request summary plus the fingerprint that validates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
struct RequestIndexEntry {
    id: RequestId,
    name: String,
    method: HttpMethod,
    url: String,
    /// Defaulted so an index written before the field existed still loads and
    /// reports HTTP.
    #[serde(default)]
    protocol: RequestProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder_id: Option<FolderId>,
    file_name: String,
    modified_s: u64,
    modified_ns: u32,
    size: u64,
}

impl RequestIndexEntry {
    fn from_spec(request: &RequestSpec, fingerprint: FileFingerprint) -> Self {
        Self::from_summary(
            &RequestSummary {
                id: request.id,
                name: request.name.clone(),
                method: request.method,
                // The cached summary is what search sees before the document is
                // read, so it carries the same composed URL a loaded entry
                // reports.
                url: join_url_query(&request.url, &request.query),
                protocol: request.protocol,
                folder_id: request.folder_id,
            },
            fingerprint,
        )
    }

    fn from_summary(summary: &RequestSummary, fingerprint: FileFingerprint) -> Self {
        Self {
            id: summary.id,
            name: summary.name.clone(),
            method: summary.method,
            url: summary.url.clone(),
            protocol: summary.protocol,
            folder_id: summary.folder_id,
            file_name: fingerprint.file_name,
            modified_s: fingerprint.modified_s,
            modified_ns: fingerprint.modified_ns,
            size: fingerprint.size,
        }
    }

    fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint {
            file_name: self.file_name.clone(),
            modified_s: self.modified_s,
            modified_ns: self.modified_ns,
            size: self.size,
        }
    }

    fn summary(&self) -> RequestSummary {
        RequestSummary {
            id: self.id,
            name: self.name.clone(),
            method: self.method,
            url: self.url.clone(),
            protocol: self.protocol,
            folder_id: self.folder_id,
        }
    }

    fn matches(&self, fingerprint: &FileFingerprint) -> bool {
        self.fingerprint().matches(fingerprint)
    }
}

/// In-memory indexes for this store, shared by clones.
#[derive(Debug, Default)]
struct IndexState {
    indexes: HashMap<String, RequestIndex>,
    dirty: HashSet<String>,
}

/// Resolves a workspace-relative asset path under `root`.
///
/// `None` means the path does not name a place inside the workspace: an
/// absolute path, a parent-directory component, or an empty path. A document's
/// legacy absolute path is the caller's to use as given.
pub fn resolve_asset_under(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut resolved = root.to_path_buf();
    let mut parts = 0;
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => {
                resolved.push(part);
                parts += 1;
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
        }
    }
    (parts > 0).then_some(resolved)
}

/// A workspace rooted at a directory, with helpers for every persisted path.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    root: PathBuf,
    /// `<cache>/Posthaste/index/<workspace-key>`, or `None` when caching is
    /// unavailable. The index is always derived, so `None` only costs reads.
    cache_root: Option<PathBuf>,
    indexes: Arc<Mutex<IndexState>>,
}

/// What startup resolution selected, plus a remembered path that was skipped.
#[derive(Debug, Clone)]
pub struct WorkspaceResolution {
    /// The store for the selected directory.
    pub store: WorkspaceStore,
    /// The remembered workspace that was unusable, if one was skipped.
    pub unavailable_remembered: Option<PathBuf>,
    /// `true` when `--workspace` or `POSTHASTE_WORKSPACE` named the path.
    ///
    /// An explicit path names a workspace the user expects to exist; startup
    /// loads it without creating anything. The default location is different:
    /// a first run there bootstraps it.
    pub explicit: bool,
}

impl WorkspaceStore {
    /// Resolves the workspace path from `POSTHASTE_WORKSPACE`, or the platform
    /// per-user data directory beneath `Posthaste/workspace`.
    pub fn resolve() -> Result<Self, StorageError> {
        Self::resolve_with_defaults(None, None).map(|resolution| resolution.store)
    }

    /// Resolves startup from the process environment and the platform
    /// directories, with an optional `--workspace` launch override.
    pub fn resolve_with_defaults(
        launch: Option<PathBuf>,
        remembered: Option<PathBuf>,
    ) -> Result<WorkspaceResolution, StorageError> {
        Self::resolve_with_home(
            launch,
            paths::env_value("POSTHASTE_WORKSPACE"),
            remembered,
            paths::home(),
            dirs::cache_dir(),
        )
    }

    /// Resolves the workspace in precedence order: the `--workspace` launch
    /// override, `POSTHASTE_WORKSPACE`, a usable remembered workspace, then the
    /// platform default location.
    ///
    /// A remembered workspace is usable only when it is an existing directory
    /// containing `workspace.json`; otherwise it is reported in the resolution
    /// and the default location is selected. Nothing is read or written at the
    /// remembered path.
    pub fn resolve_with(
        launch: Option<PathBuf>,
        environment: Option<OsString>,
        remembered: Option<PathBuf>,
        data_dir: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
    ) -> Result<WorkspaceResolution, StorageError> {
        Self::resolve_with_home(
            launch,
            environment,
            remembered,
            paths::home_in(data_dir),
            cache_dir,
        )
    }

    /// The injectable form of [`Self::resolve_with_defaults`], taking the
    /// already-resolved product directory under the platform data directory.
    fn resolve_with_home(
        launch: Option<PathBuf>,
        environment: Option<OsString>,
        remembered: Option<PathBuf>,
        data_home: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
    ) -> Result<WorkspaceResolution, StorageError> {
        let override_path = launch
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| {
                environment
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            });
        if let Some(root) = override_path {
            return Ok(WorkspaceResolution {
                store: Self::with_cache_dir(root, cache_dir),
                unavailable_remembered: None,
                explicit: true,
            });
        }

        let mut unavailable_remembered = None;
        if let Some(root) = remembered {
            if root.is_dir() && root.join(WORKSPACE_FILE).is_file() {
                return Ok(WorkspaceResolution {
                    store: Self::with_cache_dir(root, cache_dir),
                    unavailable_remembered: None,
                    explicit: false,
                });
            }
            unavailable_remembered = Some(root);
        }

        let data_home = data_home.ok_or(StorageError::NoDataDir)?;
        let root = data_home.join("workspace");
        Ok(WorkspaceResolution {
            store: Self::with_cache_dir(root, cache_dir),
            unavailable_remembered,
            explicit: false,
        })
    }

    #[cfg(test)]
    fn resolve_from(
        override_path: Option<OsString>,
        data_dir: Option<PathBuf>,
    ) -> Result<Self, StorageError> {
        Self::resolve_with(None, override_path, None, data_dir, None)
            .map(|resolution| resolution.store)
    }

    /// Creates a store for an explicit root without request-metadata caching.
    ///
    /// This is the constructor used by tests that exercise a full load, so
    /// their behavior does not depend on the platform cache directory.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::new(root.into(), None)
    }

    /// Creates a store whose request indexes live below `cache_dir`, as
    /// `<cache_dir>/Posthaste/index/<workspace-key>`. A missing cache directory
    /// disables caching; loading and saving still succeed.
    pub fn with_cache_dir(root: impl Into<PathBuf>, cache_dir: Option<PathBuf>) -> Self {
        let root = root.into();
        let cache_root = cache_dir.map(|cache_dir| {
            cache_dir
                .join(paths::PRODUCT_DIR)
                .join(CACHE_INDEX_DIR)
                .join(workspace_key(&root).to_string())
        });
        Self::new(root, cache_root)
    }

    /// Creates a store whose request indexes are read from `index_root`
    /// directly. Production uses [`Self::with_cache_dir`]; this constructor
    /// keeps index tests independent of the platform cache location.
    pub fn at_cached(root: impl Into<PathBuf>, index_root: impl Into<PathBuf>) -> Self {
        Self::new(root.into(), Some(index_root.into()))
    }

    fn new(root: PathBuf, cache_root: Option<PathBuf>) -> Self {
        Self {
            root,
            cache_root,
            indexes: Arc::new(Mutex::new(IndexState::default())),
        }
    }

    /// The directory holding this workspace's derived indexes, when caching is
    /// enabled.
    pub fn cache_root(&self) -> Option<&Path> {
        self.cache_root.as_deref()
    }

    /// `<cache-root>/<collection-id>.json`, or `None` when caching is disabled.
    pub fn index_file(&self, slug: &str) -> Option<PathBuf> {
        let cache_root = self.cache_root.as_ref()?;
        if !is_valid_slug(slug) {
            return None;
        }
        Some(cache_root.join(format!("{slug}.{DOCUMENT_EXTENSION}")))
    }

    fn lock_indexes(&self) -> MutexGuard<'_, IndexState> {
        self.indexes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/workspace.json`.
    pub fn workspace_file(&self) -> PathBuf {
        self.root.join(WORKSPACE_FILE)
    }

    /// Whether `path` names a document preserved by a quarantine rename.
    ///
    /// A quarantined document is a file inside the workspace whose name is
    /// `<stem>.corrupt-<unix-millis>`, with `-<n>` appended when the name
    /// collided. The file does not have to exist: a preserved copy the user
    /// already deleted is still a quarantined path, so its report can be
    /// cleared by deleting it again.
    pub fn is_quarantined_document(&self, path: &Path) -> bool {
        is_beneath(&self.root, path)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_quarantine_name)
    }

    /// Deletes a preserved damaged document, refusing every other path.
    ///
    /// This is the only deletion entry point that the storage reports offer.
    /// The predicate is checked here as well as by the view, so a caller
    /// mistake cannot delete a live workspace document. A file that is already
    /// gone counts as deleted.
    pub fn delete_quarantined_document(&self, path: &Path) -> Result<(), StorageError> {
        if !self.is_quarantined_document(path) {
            return Err(StorageError::InvalidEntry {
                path: path.to_path_buf(),
                reason: "only a preserved damaged document can be deleted".into(),
            });
        }
        remove_document(path)
    }

    /// `<root>/collections`.
    pub fn collections_dir(&self) -> PathBuf {
        self.root.join(COLLECTIONS_DIR)
    }

    /// `<root>/environments`.
    pub fn environments_dir(&self) -> PathBuf {
        self.root.join(ENVIRONMENTS_DIR)
    }

    /// `<root>/files`, where picked file assets are copied. The directory is
    /// created on the first import, so a workspace that never attaches a file
    /// has none.
    pub fn files_dir(&self) -> PathBuf {
        self.root.join(FILES_DIR)
    }

    /// Records `source` as a workspace-owned asset and returns its path
    /// relative to the workspace root, with forward slashes.
    ///
    /// A regular file already inside the workspace is referenced where it is.
    /// Any other file is copied to `files/<uuid>[.<extension>]` — the source is
    /// left untouched, and its extension is preserved, lowercased. The copy is
    /// staged in a temporary file and renamed, so a failed copy leaves nothing
    /// behind and the returned path is only produced on success.
    pub fn import_file(&self, source: &Path) -> Result<String, StorageError> {
        let metadata = fs::metadata(source).map_err(|error| io_error(source, error))?;
        if !metadata.is_file() {
            return Err(StorageError::InvalidEntry {
                path: source.to_path_buf(),
                reason: "a file asset must be a regular file".into(),
            });
        }
        if let Some(relative) = relative_under(&self.root, source) {
            return Ok(slash_path(&relative));
        }

        let files = self.files_dir();
        let name = match source
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
        {
            Some(extension) if !extension.is_empty() => format!("{}.{extension}", Uuid::now_v7()),
            _ => Uuid::now_v7().to_string(),
        };
        let destination = files.join(&name);

        let bytes = fs::read(source).map_err(|error| io_error(source, error))?;
        persist_bytes_atomic(&destination, &bytes)?;

        Ok(format!("{FILES_DIR}/{name}"))
    }

    /// Resolves a workspace-relative asset path against the workspace root.
    ///
    /// `None` means the path does not name a place inside the workspace: an
    /// absolute path, a parent-directory component, or an empty path. A
    /// document's legacy absolute path is the caller's to use as given.
    pub fn resolve_asset(&self, relative: &str) -> Option<PathBuf> {
        resolve_asset_under(&self.root, relative)
    }

    /// `<root>/collections/<slug>`; rejects a slug that is not a single safe
    /// path component.
    pub fn collection_dir(&self, slug: &str) -> Result<PathBuf, StorageError> {
        if !is_valid_slug(slug) {
            return Err(self.invalid_slug(slug));
        }
        Ok(self.collections_dir().join(slug))
    }

    /// `<root>/collections/<slug>/collection.json`.
    pub fn collection_file(&self, slug: &str) -> Result<PathBuf, StorageError> {
        Ok(self.collection_dir(slug)?.join(COLLECTION_FILE))
    }

    /// `<root>/collections/<slug>/requests`.
    pub fn requests_dir(&self, slug: &str) -> Result<PathBuf, StorageError> {
        Ok(self.collection_dir(slug)?.join(REQUESTS_DIR))
    }

    /// `<root>/collections/<slug>/requests/<uuid>.json`.
    pub fn request_file(&self, slug: &str, id: RequestId) -> Result<PathBuf, StorageError> {
        Ok(self
            .requests_dir(slug)?
            .join(format!("{id}.{DOCUMENT_EXTENSION}")))
    }

    /// `<root>/environments/<slug>.json`; rejects a slug that is not a single
    /// safe path component.
    pub fn environment_file(&self, slug: &str) -> Result<PathBuf, StorageError> {
        if !is_valid_slug(slug) {
            return Err(StorageError::InvalidEntry {
                path: self.environments_dir().join(slug),
                reason: format!("invalid environment slug {slug:?}"),
            });
        }
        Ok(self
            .environments_dir()
            .join(format!("{slug}.{DOCUMENT_EXTENSION}")))
    }

    fn invalid_slug(&self, slug: &str) -> StorageError {
        StorageError::InvalidEntry {
            path: self.collections_dir().join(slug),
            reason: format!("invalid collection slug {slug:?}"),
        }
    }

    /// Loads the workspace, bootstrapping an empty root on first run.
    ///
    /// A missing or empty root is initialized with `workspace.json`,
    /// `collections/`, `environments/`, and the two version-control dotfiles. A
    /// non-empty root without that document, or a root that is not a
    /// directory, is rejected without writing anything.
    pub fn load(&self) -> Result<LoadedWorkspace, StorageError> {
        self.ensure_root()?;

        let canonical_path = self.workspace_file();
        let mut issues = Vec::new();
        let workspace = if canonical_path.is_file() {
            match read_versioned::<Workspace>(&canonical_path) {
                Ok(workspace) => workspace,
                Err(error) => {
                    self.substitute_damaged_workspace(&canonical_path, &error, &mut issues)
                }
            }
        } else {
            if !self.root_is_empty()? {
                return Err(StorageError::InvalidEntry {
                    path: self.root.clone(),
                    reason: format!(
                        "directory is not a Posthaste workspace ({WORKSPACE_FILE} is missing)"
                    ),
                });
            }
            let workspace = Workspace::new(DEFAULT_WORKSPACE_NAME);
            write_document_atomic(&canonical_path, &workspace)?;
            issues.extend(self.write_dotfiles());
            workspace
        };

        self.load_contents(workspace, issues)
    }

    /// Loads a workspace the user named explicitly, without creating anything.
    ///
    /// Unlike [`Self::load`], a missing directory, a root that is not a
    /// directory, and a directory without `workspace.json` are all errors
    /// rather than a first-run bootstrap. Startup reports the failure and offers
    /// to create the workspace, instead of writing to a path the user may have
    /// mistyped. A `workspace.json` that exists but cannot be used is still
    /// quarantined and replaced with a default, because leaving it in place
    /// would lose it to the next save.
    pub fn load_existing(&self) -> Result<LoadedWorkspace, StorageError> {
        let metadata = match fs::metadata(&self.root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::InvalidEntry {
                    path: self.root.clone(),
                    reason: "the workspace directory does not exist".into(),
                });
            }
            Err(error) => return Err(io_error(&self.root, error)),
        };
        if !metadata.is_dir() {
            return Err(StorageError::InvalidEntry {
                path: self.root.clone(),
                reason: "workspace path exists but is not a directory".into(),
            });
        }

        let canonical_path = self.workspace_file();
        if !canonical_path.is_file() {
            return Err(StorageError::InvalidEntry {
                path: self.root.clone(),
                reason: format!(
                    "directory is not a Posthaste workspace ({WORKSPACE_FILE} is missing)"
                ),
            });
        }

        let mut issues = Vec::new();
        let workspace = match read_versioned::<Workspace>(&canonical_path) {
            Ok(workspace) => workspace,
            Err(error) => self.substitute_damaged_workspace(&canonical_path, &error, &mut issues),
        };
        self.load_contents(workspace, issues)
    }

    /// Preserves a damaged `workspace.json` and substitutes a default workspace.
    ///
    /// The damaged file is renamed to `workspace.json.corrupt-<unix-millis>` in
    /// the same directory before anything can overwrite it, and a fresh default
    /// document is written to the canonical path so later saves start from a
    /// valid file. If the rename fails, the file stays where it is, no write is
    /// attempted, and the issue names the path that could not be preserved. The
    /// issue path is the canonical document only in that case, which keeps the
    /// app's re-save from ever targeting the damaged bytes.
    fn substitute_damaged_workspace(
        &self,
        canonical_path: &Path,
        error: &StorageError,
        issues: &mut Vec<StorageIssue>,
    ) -> Workspace {
        let workspace = Workspace::new(DEFAULT_WORKSPACE_NAME);
        match quarantine_document(canonical_path) {
            Ok(preserved) => {
                issues.push(issue_for(
                    &preserved,
                    format!(
                        "damaged workspace document preserved ({}); a new workspace.json was written",
                        error.detail()
                    ),
                ));
                if let Err(write_error) = write_document_atomic(canonical_path, &workspace) {
                    push_error_issue(issues, &write_error);
                }
            }
            Err(quarantine_error) => {
                issues.push(issue_for(
                    canonical_path,
                    format!(
                        "damaged workspace document could not be preserved ({}); it will not be overwritten",
                        quarantine_error.detail()
                    ),
                ));
            }
        }
        workspace
    }

    /// Reads the documents that hang off a workspace root, creating the two
    /// collection directories when they are missing.
    fn load_contents(
        &self,
        workspace: Workspace,
        mut issues: Vec<StorageIssue>,
    ) -> Result<LoadedWorkspace, StorageError> {
        // Residue from an interrupted atomic write is never data: remove it
        // from the directories this store owns before enumerating them.
        cleanup_temporary_files(&self.root);
        ensure_dir(&self.collections_dir())?;
        ensure_dir(&self.environments_dir())?;

        let mut collections = self.load_collections(&mut issues)?;
        collections.sort_by(|a, b| {
            a.collection
                .order
                .cmp(&b.collection.order)
                .then_with(|| {
                    a.collection
                        .name
                        .to_lowercase()
                        .cmp(&b.collection.name.to_lowercase())
                })
                .then_with(|| a.collection.id.cmp(&b.collection.id))
        });

        let mut environments = self.load_environments(&mut issues)?;
        environments.sort_by(|a, b| {
            a.environment
                .name
                .to_lowercase()
                .cmp(&b.environment.name.to_lowercase())
                .then_with(|| a.slug.cmp(&b.slug))
        });

        Ok(LoadedWorkspace {
            workspace,
            collections,
            environments,
            issues,
        })
    }

    /// Whether [`Self::bootstrap`] could create a workspace here without
    /// overwriting anything: the root is missing, or is a directory that holds
    /// no `workspace.json`.
    pub fn is_bootstrappable(&self) -> bool {
        if self.workspace_file().exists() {
            return false;
        }
        match fs::metadata(&self.root) {
            Ok(metadata) => metadata.is_dir(),
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }

    /// Creates the workspace layout in this directory without loading it.
    ///
    /// Unlike [`Self::load`], an existing directory may hold unrelated files:
    /// the user pointed at it and confirmed initialization. An existing
    /// `workspace.json` is never replaced.
    pub fn bootstrap(&self) -> Result<Workspace, StorageError> {
        self.bootstrap_named(DEFAULT_WORKSPACE_NAME)
    }

    /// [`Self::bootstrap`] with the non-fatal problems the bootstrap hit.
    pub fn bootstrap_with_issues(&self) -> Result<(Workspace, Vec<StorageIssue>), StorageError> {
        self.bootstrap_named_with_issues(DEFAULT_WORKSPACE_NAME)
    }

    /// Creates the workspace layout with `name` as the display name.
    pub fn bootstrap_named(&self, name: &str) -> Result<Workspace, StorageError> {
        self.bootstrap_named_with_issues(name)
            .map(|(workspace, _)| workspace)
    }

    /// [`Self::bootstrap_named`], also returning the non-fatal problems the
    /// bootstrap hit — a dotfile or directory it could not create. The
    /// workspace is usable either way.
    pub fn bootstrap_named_with_issues(
        &self,
        name: &str,
    ) -> Result<(Workspace, Vec<StorageIssue>), StorageError> {
        self.ensure_root()?;
        let path = self.workspace_file();
        if path.exists() {
            return Err(StorageError::InvalidEntry {
                path,
                reason: "workspace.json already exists".into(),
            });
        }
        let workspace = Workspace::new(name);
        write_document_atomic(&self.workspace_file(), &workspace)?;
        ensure_dir(&self.collections_dir())?;
        ensure_dir(&self.environments_dir())?;
        Ok((workspace, self.write_dotfiles()))
    }

    /// Creates `.gitattributes` and `.gitignore` when they are absent.
    ///
    /// `create_new` makes this a no-op for a file that already exists, even an
    /// empty one the user wrote, and a failure is reported rather than failing
    /// the workspace. Nothing here runs git or creates a repository.
    fn write_dotfiles(&self) -> Vec<StorageIssue> {
        let mut issues = Vec::new();
        for (name, content) in [
            (GITATTRIBUTES_FILE, GITATTRIBUTES_CONTENT),
            (GITIGNORE_FILE, GITIGNORE_CONTENT),
        ] {
            let path = self.root.join(name);
            let file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A file in the dotfile's place belongs to the user and is
                    // never replaced. Anything else — a directory, say — means
                    // the rule could not be written at all.
                    if !path.is_file() {
                        issues.push(issue_for(
                            &path,
                            format!("{name} was not created: the path is not a file"),
                        ));
                    }
                    continue;
                }
                Err(error) => {
                    issues.push(issue_for(&path, format!("{name} was not created: {error}")));
                    continue;
                }
            };
            let mut file = file;
            if let Err(error) = file.write_all(content.as_bytes()) {
                issues.push(issue_for(&path, format!("{name} was not written: {error}")));
            }
        }
        issues
    }

    fn ensure_root(&self) -> Result<(), StorageError> {
        match fs::metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(StorageError::InvalidEntry {
                path: self.root.clone(),
                reason: "workspace path exists but is not a directory".into(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.root).map_err(|error| io_error(&self.root, error))
            }
            Err(error) => Err(io_error(&self.root, error)),
        }
    }

    fn root_is_empty(&self) -> Result<bool, StorageError> {
        let mut entries = fs::read_dir(&self.root).map_err(|error| io_error(&self.root, error))?;
        Ok(entries.next().is_none())
    }

    fn load_collections(
        &self,
        issues: &mut Vec<StorageIssue>,
    ) -> Result<Vec<LoadedCollection>, StorageError> {
        let collections_dir = self.collections_dir();
        cleanup_temporary_files(&collections_dir);
        let entries = match fs::read_dir(&collections_dir) {
            Ok(entries) => entries,
            Err(error) => return Err(io_error(&collections_dir, error)),
        };

        let mut loaded = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    issues.push(StorageIssue {
                        path: collections_dir.clone(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    issues.push(issue_for(&path, error.to_string()));
                    continue;
                }
            };
            if !file_type.is_dir() {
                continue;
            }
            cleanup_temporary_files(&path);

            let Some(slug) = entry.file_name().to_str().map(str::to_owned) else {
                issues.push(issue_for(&path, "directory name is not valid UTF-8"));
                continue;
            };
            if !is_valid_slug(&slug) {
                issues.push(issue_for(&path, "invalid collection directory name"));
                continue;
            }

            let collection_path = path.join(COLLECTION_FILE);
            if !collection_path.is_file() {
                // Unrelated directories are ignored rather than reported.
                continue;
            }

            let mut collection = match read_versioned::<Collection>(&collection_path) {
                Ok(collection) => collection,
                Err(error) => {
                    push_error_issue(issues, &error);
                    continue;
                }
            };
            if collection.id != slug {
                // Adopt the directory name so an interrupted rename (or a
                // directory renamed by hand) still loads. The next save
                // rewrites the document with the matching id.
                issues.push(issue_for(
                    &collection_path,
                    format!(
                        "collection id {:?} does not match directory name {:?}; using the directory name",
                        collection.id, slug
                    ),
                ));
                collection.id = slug.clone();
            }

            let folder_ids = resolve_folders(&mut collection, &collection_path, issues);
            let requests = self.load_requests(&slug, &folder_ids, issues)?;
            loaded.push(LoadedCollection {
                collection,
                requests,
            });
        }

        Ok(loaded)
    }

    /// Reads one request document through the versioned loader and rejects an
    /// identifier that does not match the filename stem.
    pub fn load_request(&self, slug: &str, id: RequestId) -> Result<RequestSpec, StorageError> {
        let path = self.request_file(slug, id)?;
        if !path.is_file() {
            return Err(StorageError::InvalidEntry {
                path,
                reason: "request document does not exist".into(),
            });
        }
        let request = read_versioned::<RequestSpec>(&path)?;
        if request.id != id {
            return Err(StorageError::InvalidEntry {
                path,
                reason: format!("request id {} does not match file name {id}", request.id),
            });
        }
        Ok(request)
    }

    /// Lists request metadata with one directory enumeration per collection.
    ///
    /// A cached entry whose filename, modification time, and size all match is
    /// reused without opening the document. Everything else is parsed, and
    /// cached entries without a matching file are dropped. The refreshed index
    /// is written when it differs from the previous one.
    fn load_requests(
        &self,
        slug: &str,
        folder_ids: &HashSet<FolderId>,
        issues: &mut Vec<StorageIssue>,
    ) -> Result<Vec<RequestEntry>, StorageError> {
        let requests_dir = self.requests_dir(slug)?;

        match fs::metadata(&requests_dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                issues.push(issue_for(&requests_dir, "requests path is not a directory"));
                return Ok(Vec::new());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = fs::create_dir_all(&requests_dir) {
                    issues.push(issue_for(&requests_dir, error.to_string()));
                    return Ok(Vec::new());
                }
            }
            Err(error) => {
                issues.push(issue_for(&requests_dir, error.to_string()));
                return Ok(Vec::new());
            }
        }

        cleanup_temporary_files(&requests_dir);
        let entries = match fs::read_dir(&requests_dir) {
            Ok(entries) => entries,
            Err(error) => {
                issues.push(issue_for(&requests_dir, error.to_string()));
                return Ok(Vec::new());
            }
        };

        let documents = scan_request_documents(entries, &requests_dir, issues);
        let previous = self.read_index(slug).ok();
        let mut cached: HashMap<Uuid, RequestIndexEntry> = previous
            .as_ref()
            .map(|index| {
                index
                    .entries
                    .iter()
                    .map(|entry| (entry.id.as_uuid(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let mut requests = Vec::new();
        let mut index_entries = Vec::new();
        for (stem, document) in documents {
            let Some(stem_id) = parse_request_stem(&stem) else {
                issues.push(issue_for(&document.path, "request file name is not a UUID"));
                continue;
            };

            if let Some(fingerprint) = &document.fingerprint
                && let Some(cached_entry) = cached.remove(&stem_id)
                && cached_entry.matches(fingerprint)
            {
                let mut summary = cached_entry.summary();
                if let Some(folder_id) = summary.folder_id
                    && !folder_ids.contains(&folder_id)
                {
                    issues.push(issue_for(
                        &document.path,
                        format!(
                            "request {:?} refers to unknown folder {folder_id}; moved to the collection root",
                            summary.name
                        ),
                    ));
                    summary.folder_id = None;
                }
                index_entries.push(RequestIndexEntry::from_summary(
                    &summary,
                    fingerprint.clone(),
                ));
                requests.push(RequestEntry::Unloaded {
                    summary,
                    fingerprint: fingerprint.clone(),
                });
                continue;
            }

            let mut request = match read_versioned::<RequestSpec>(&document.path) {
                Ok(request) => request,
                Err(error) => {
                    push_error_issue(issues, &error);
                    continue;
                }
            };
            if request.id.as_uuid() != stem_id {
                issues.push(issue_for(
                    &document.path,
                    format!("request id {} does not match file name {stem}", request.id),
                ));
                continue;
            }
            if let Some(folder_id) = request.folder_id
                && !folder_ids.contains(&folder_id)
            {
                issues.push(issue_for(
                    &document.path,
                    format!(
                        "request {:?} refers to unknown folder {folder_id}; moved to the collection root",
                        request.name
                    ),
                ));
                request.folder_id = None;
            }
            if let Some(fingerprint) = &document.fingerprint {
                index_entries.push(RequestIndexEntry::from_spec(&request, fingerprint.clone()));
            }
            requests.push(RequestEntry::Loaded(Box::new(request)));
        }

        requests.sort_by(|a, b| {
            a.name()
                .to_lowercase()
                .cmp(&b.name().to_lowercase())
                .then_with(|| a.id().cmp(&b.id()))
        });

        if self.cache_root.is_some() {
            index_entries.sort_by_key(|a| a.id);
            let index = RequestIndex::new(slug, index_entries);
            self.remember_reconciled_index(slug, index, previous.as_ref());
        }
        Ok(requests)
    }

    /// Reads the derived index for `slug`.
    ///
    /// A missing, corrupt, stale-version, or wrong-collection index is reported
    /// as an error because every caller can simply rebuild it.
    fn read_index(&self, slug: &str) -> Result<RequestIndex, StorageError> {
        let path = self
            .index_file(slug)
            .ok_or_else(|| StorageError::InvalidEntry {
                path: self.root.clone(),
                reason: "request caching is disabled".into(),
            })?;
        let contents = fs::read_to_string(&path).map_err(|error| io_error(&path, error))?;
        let index: RequestIndex =
            serde_json::from_str(&contents).map_err(|error| StorageError::Parse {
                path: path.clone(),
                format: "JSON",
                message: error.to_string(),
            })?;
        if index.index_version != INDEX_VERSION {
            return Err(StorageError::InvalidEntry {
                path,
                reason: format!(
                    "index_version {} is not supported version {INDEX_VERSION}",
                    index.index_version
                ),
            });
        }
        if index.collection_id != slug {
            return Err(StorageError::InvalidEntry {
                path,
                reason: format!(
                    "index collection id {:?} does not match {slug:?}",
                    index.collection_id
                ),
            });
        }
        Ok(index)
    }

    /// Writes one derived index atomically. A failure is caller-discardable and
    /// leaves any previous index in place.
    fn write_index(&self, slug: &str, index: &RequestIndex) -> Result<(), StorageError> {
        let Some(path) = self.index_file(slug) else {
            return Ok(());
        };
        write_document_atomic(&path, index).map(|_| ())
    }

    /// Keeps a freshly reconciled index in memory and persists it when it
    /// differs from the file that was read. A failed write stays dirty and is
    /// retried on the next flush.
    fn remember_reconciled_index(
        &self,
        slug: &str,
        index: RequestIndex,
        previous: Option<&RequestIndex>,
    ) {
        let changed = previous != Some(&index);
        {
            let mut state = self.lock_indexes();
            state.indexes.insert(slug.to_owned(), index);
            if changed {
                state.dirty.insert(slug.to_owned());
            } else {
                state.dirty.remove(slug);
            }
        }
        if changed {
            self.flush_index(slug);
        }
    }

    /// Replaces one collection's index from the current in-memory entries.
    ///
    /// Loaded entries contribute a fresh fingerprint; unloaded entries keep the
    /// fingerprint that was validated at load time. The write happens on the
    /// next [`Self::flush_indexes`] call.
    pub fn update_collection_index(&self, slug: &str, requests: &[RequestEntry]) {
        if self.cache_root.is_none() {
            return;
        }
        let mut entries = Vec::with_capacity(requests.len());
        for request in requests {
            match request {
                RequestEntry::Loaded(spec) => {
                    if let Some(fingerprint) = self.request_fingerprint(slug, spec.id) {
                        entries.push(RequestIndexEntry::from_spec(spec, fingerprint));
                    }
                }
                RequestEntry::Unloaded {
                    summary,
                    fingerprint,
                } => {
                    entries.push(RequestIndexEntry::from_summary(
                        summary,
                        fingerprint.clone(),
                    ));
                }
            }
        }
        entries.sort_by_key(|a| a.id);
        let index = RequestIndex::new(slug, entries);
        let mut state = self.lock_indexes();
        state.indexes.insert(slug.to_owned(), index);
        state.dirty.insert(slug.to_owned());
    }

    /// Flushes every dirty index, silently ignoring failures.
    pub fn flush_indexes(&self) {
        let dirty: Vec<String> = self.lock_indexes().dirty.iter().cloned().collect();
        for slug in dirty {
            self.flush_index(&slug);
        }
    }

    fn flush_index(&self, slug: &str) {
        if self.cache_root.is_none() {
            return;
        }
        let index = self.lock_indexes().indexes.get(slug).cloned();
        let Some(index) = index else {
            self.lock_indexes().dirty.remove(slug);
            return;
        };
        if self.write_index(slug, &index).is_ok() {
            self.lock_indexes().dirty.remove(slug);
        }
    }

    /// Removes one collection's cached index, if present. Failures are
    /// ignored because the index is derived.
    pub fn drop_index(&self, slug: &str) {
        {
            let mut state = self.lock_indexes();
            state.indexes.remove(slug);
            state.dirty.remove(slug);
        }
        if let Some(path) = self.index_file(slug) {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {}
            }
        }
    }

    /// Moves one collection's cached index to a new slug, if there is one.
    /// Failures are ignored because the index is derived.
    pub fn rename_index(&self, from: &str, to: &str) {
        let Some(from_path) = self.index_file(from) else {
            return;
        };
        if self.index_file(to).is_none() {
            return;
        }
        let mut index = {
            let mut state = self.lock_indexes();
            state.dirty.remove(from);
            state.indexes.remove(from)
        };
        if index.is_none() {
            index = self.read_index(from).ok();
        }
        let _ = fs::remove_file(&from_path);
        let Some(mut index) = index else {
            return;
        };
        index.collection_id = to.to_owned();
        index.entries.sort_by_key(|a| a.id);
        {
            let mut state = self.lock_indexes();
            state.indexes.insert(to.to_owned(), index.clone());
            state.dirty.insert(to.to_owned());
        }
        self.flush_index(to);
    }

    fn request_fingerprint(&self, slug: &str, id: RequestId) -> Option<FileFingerprint> {
        FileFingerprint::from_path(&self.request_file(slug, id).ok()?)
    }

    /// Loads every `environments/<slug>.json` document.
    ///
    /// The slug comes from the filename. Renaming an environment moves its
    /// file to the slug of the new name. Files that do not use a supported
    /// extension, or whose stem is not a safe slug, are reported and skipped.
    fn load_environments(
        &self,
        issues: &mut Vec<StorageIssue>,
    ) -> Result<Vec<LoadedEnvironment>, StorageError> {
        let environments_dir = self.environments_dir();
        cleanup_temporary_files(&environments_dir);
        let entries = match fs::read_dir(&environments_dir) {
            Ok(entries) => entries,
            Err(error) => return Err(io_error(&environments_dir, error)),
        };

        let mut loaded = Vec::new();
        for (stem, path) in documents_by_stem(entries, &environments_dir, "environment", issues) {
            if !is_valid_slug(&stem) {
                issues.push(issue_for(&path, "invalid environment file name"));
                continue;
            }

            let environment = match read_versioned::<Environment>(&path) {
                Ok(environment) => environment,
                Err(error) => {
                    push_error_issue(issues, &error);
                    continue;
                }
            };
            loaded.push(LoadedEnvironment {
                slug: stem,
                environment,
            });
        }

        Ok(loaded)
    }

    /// Creates a collection with a unique slug derived from `name`.
    pub fn create_collection(&self, name: &str, order: u32) -> Result<Collection, StorageError> {
        let name = name.trim();
        let name = if name.is_empty() {
            "New Collection"
        } else {
            name
        };
        let slug = self.unique_slug(name)?;
        let mut collection = Collection::new(slug, name);
        collection.order = order;
        self.save_collection(&collection)?;
        Ok(collection)
    }

    /// Creates a request with default values in `slug`, returning the saved
    /// spec (including its new id).
    pub fn create_request(&self, slug: &str, name: &str) -> Result<RequestSpec, StorageError> {
        self.create_request_in(slug, name, None)
    }

    /// Creates a request with default values in `slug`, targeting `folder_id`
    /// or the collection root. The folder reference is recorded in the same
    /// write that creates the document.
    pub fn create_request_in(
        &self,
        slug: &str,
        name: &str,
        folder_id: Option<FolderId>,
    ) -> Result<RequestSpec, StorageError> {
        let name = name.trim();
        let name = if name.is_empty() { "New Request" } else { name };
        let mut request = RequestSpec::new(name);
        request.folder_id = folder_id;
        self.save_request(slug, &request)?;
        Ok(request)
    }

    /// Creates an environment with a unique slug derived from `name`, returning
    /// the slug and the saved environment.
    pub fn create_environment(&self, name: &str) -> Result<(String, Environment), StorageError> {
        let name = name.trim();
        let name = if name.is_empty() {
            "New Environment"
        } else {
            name
        };
        let slug = self.unique_environment_slug(name)?;
        let environment = Environment::new(name);
        self.save_environment(&slug, &environment)?;
        Ok((slug, environment))
    }

    /// Moves a collection directory from one slug to another.
    ///
    /// The caller computes `to` with [`Self::collection_slug_for`]. A
    /// filesystem failure leaves the old directory in place and is returned as
    /// an error; an existing destination is never overwritten.
    pub fn move_collection(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_dir = self.collection_dir(from)?;
        let to_dir = self.collection_dir(to)?;
        fs::rename(&from_dir, &to_dir).map_err(|error| io_error(&from_dir, error))
    }

    /// Moves an environment document from one slug to another.
    ///
    /// A missing source document is an error, and a filesystem failure leaves
    /// the old file in place.
    pub fn move_environment(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_json = self.environment_file(from)?;
        let to_json = self.environment_file(to)?;
        match fs::rename(&from_json, &to_json) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::InvalidEntry {
                    path: from_json,
                    reason: "environment document does not exist".into(),
                })
            }
            Err(error) => Err(io_error(&from_json, error)),
        }
    }

    /// Saves collection metadata as JSON under the directory derived from
    /// [`Collection::id`]. A rename that changes the id moves the directory
    /// first with [`Self::move_collection`].
    pub fn save_collection(&self, collection: &Collection) -> Result<SaveReport, StorageError> {
        let path = self.collection_file(&collection.id)?;
        let requests_dir = self.requests_dir(&collection.id)?;
        fs::create_dir_all(&requests_dir).map_err(|error| io_error(&requests_dir, error))?;
        write_document_atomic(&path, collection)?;
        Ok(SaveReport::written(path))
    }

    /// Saves a request under its collection and id as JSON.
    pub fn save_request(
        &self,
        slug: &str,
        request: &RequestSpec,
    ) -> Result<SaveReport, StorageError> {
        let path = self.request_file(slug, request.id)?;
        let requests_dir = self.requests_dir(slug)?;
        fs::create_dir_all(&requests_dir).map_err(|error| io_error(&requests_dir, error))?;
        write_document_atomic(&path, request)?;
        Ok(SaveReport::written(path))
    }

    /// Saves an environment to `environments/<slug>.json`. A rename that
    /// changes the slug moves the file first with [`Self::move_environment`].
    pub fn save_environment(
        &self,
        slug: &str,
        environment: &Environment,
    ) -> Result<SaveReport, StorageError> {
        let path = self.environment_file(slug)?;
        write_document_atomic(&path, environment)?;
        Ok(SaveReport::written(path))
    }

    /// Saves `workspace.json`.
    pub fn save_workspace(&self, workspace: &Workspace) -> Result<SaveReport, StorageError> {
        let path = self.workspace_file();
        write_document_atomic(&path, workspace)?;
        Ok(SaveReport::written(path))
    }

    /// Saves the workspace and every loaded collection, request, and
    /// environment. Entries that failed to load are absent from the lists and
    /// are left untouched.
    ///
    /// Every requested document is attempted; each failure is named in the
    /// report so one unwritable file does not hide the writes that succeeded.
    /// The returned error is reserved for the root directory itself being
    /// unavailable, which makes every further write meaningless.
    pub fn save_all(
        &self,
        workspace: &Workspace,
        collections: &[LoadedCollection],
        environments: &[LoadedEnvironment],
    ) -> Result<SaveReport, StorageError> {
        ensure_dir(&self.root)?;
        let mut report = SaveReport::default();
        report.absorb(self.save_workspace(workspace), self.workspace_file());
        report.append(self.save_collections(collections));
        report.append(self.save_environments(environments));
        Ok(report)
    }

    /// Saves every loaded collection and request without touching
    /// `workspace.json`. This is used for a re-save after a failure when the
    /// workspace metadata file itself could not be loaded.
    pub fn save_collections(&self, collections: &[LoadedCollection]) -> SaveReport {
        let mut report = SaveReport::default();
        for loaded in collections {
            let collection_path = self
                .collection_file(&loaded.collection.id)
                .unwrap_or_else(|_| self.collections_dir());
            report.absorb(self.save_collection(&loaded.collection), collection_path);
            for request in &loaded.requests {
                // Only a loaded entry has full contents. Skipping an unloaded
                // one keeps its file byte-for-byte untouched.
                if let Some(request) = request.spec() {
                    report.absorb(
                        self.save_request(&loaded.collection.id, request),
                        self.collections_dir(),
                    );
                }
            }
        }
        report
    }

    /// Deletes a collection directory and everything under it.
    pub fn delete_collection(&self, slug: &str) -> Result<(), StorageError> {
        let dir = self.collection_dir(slug)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(&dir, error)),
        }
    }

    /// Cascade-deletes the folder subtree identified by `folder_ids`.
    ///
    /// The updated collection document is written first; that write is the
    /// commit point. Only then are the subtree's request files deleted. A
    /// collection write failure leaves `loaded` and every request file
    /// untouched. A request file that cannot be deleted stays loaded at the
    /// collection root with its folder reference cleared, and is reported in
    /// the returned issues. `folder_ids` must contain the whole subtree, so
    /// callers pass the folder and every descendant.
    pub fn delete_folder_subtree(
        &self,
        loaded: &mut LoadedCollection,
        folder_ids: &[FolderId],
    ) -> Result<Vec<StorageIssue>, StorageError> {
        let removed: HashSet<FolderId> = folder_ids.iter().copied().collect();
        let mut updated = loaded.collection.clone();
        updated
            .folders
            .retain(|folder| !removed.contains(&folder.id));
        let mut issues = self.save_collection(&updated)?.into_issues();
        loaded.collection = updated;

        let doomed: Vec<RequestId> = loaded
            .requests
            .iter()
            .filter(|request| {
                request
                    .folder_id()
                    .is_some_and(|folder_id| removed.contains(&folder_id))
            })
            .map(|request| request.id())
            .collect();

        for id in doomed {
            match self.delete_request(&loaded.collection.id, id) {
                Ok(()) => loaded.requests.retain(|request| request.id() != id),
                Err(error) => {
                    issues.push(issue_for(
                        error.path().unwrap_or(&self.root),
                        format!(
                            "request {id} could not be deleted: {}; moved to the collection root",
                            error.detail()
                        ),
                    ));
                    if let Some(request) = loaded
                        .requests
                        .iter_mut()
                        .find(|request| request.id() == id)
                    {
                        request.set_folder_id(None);
                    }
                }
            }
        }
        Ok(issues)
    }

    /// Deletes one request file, whichever container extension it uses.
    pub fn delete_request(&self, slug: &str, id: RequestId) -> Result<(), StorageError> {
        remove_document(&self.request_file(slug, id)?)
    }

    /// Saves every loaded environment without touching `workspace.json`.
    pub fn save_environments(&self, environments: &[LoadedEnvironment]) -> SaveReport {
        let mut report = SaveReport::default();
        for loaded in environments {
            let path = self
                .environment_file(&loaded.slug)
                .unwrap_or_else(|_| self.environments_dir());
            report.absorb(
                self.save_environment(&loaded.slug, &loaded.environment),
                path,
            );
        }
        report
    }

    /// Deletes one environment file, whichever container extension it uses.
    pub fn delete_environment(&self, slug: &str) -> Result<(), StorageError> {
        remove_document(&self.environment_file(slug)?)
    }

    /// The slug a collection would receive if renamed to `name`, excluding
    /// `current` from the collision scan.
    ///
    /// A name that slugifies to `current` returns `current`, so a case-only or
    /// punctuation-only rename needs no directory move. A name whose slug is
    /// used by another collection receives the next `-2`, `-3`, … variant.
    pub fn collection_slug_for(&self, name: &str, current: &str) -> Result<String, StorageError> {
        self.unique_slug_in(&self.collections_dir(), name, "collection", Some(current))
    }

    /// The slug an environment would receive if renamed to `name`, excluding
    /// `current` from the collision scan. See [`Self::collection_slug_for`].
    pub fn environment_slug_for(&self, name: &str, current: &str) -> Result<String, StorageError> {
        self.unique_slug_in(&self.environments_dir(), name, "environment", Some(current))
    }

    fn unique_slug(&self, name: &str) -> Result<String, StorageError> {
        self.unique_slug_in(&self.collections_dir(), name, "collection", None)
    }

    fn unique_environment_slug(&self, name: &str) -> Result<String, StorageError> {
        self.unique_slug_in(&self.environments_dir(), name, "environment", None)
    }

    fn unique_slug_in(
        &self,
        dir: &Path,
        name: &str,
        fallback: &str,
        current: Option<&str>,
    ) -> Result<String, StorageError> {
        let base = slugify(name, fallback);
        let current = current.map(str::to_lowercase);
        let mut taken = Vec::new();
        match fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        let name = name
                            .strip_suffix(&format!(".{DOCUMENT_EXTENSION}"))
                            .unwrap_or(name);
                        let name = name.to_lowercase();
                        if current.as_ref() != Some(&name) {
                            taken.push(name);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(dir, error)),
        }

        if !taken.iter().any(|slug| slug == &base.to_lowercase()) {
            return Ok(base);
        }
        for suffix in 2.. {
            let candidate = with_suffix(&base, &format!("-{suffix}"), fallback);
            if !taken.iter().any(|slug| slug == &candidate.to_lowercase()) {
                return Ok(candidate);
            }
        }
        unreachable!("suffix space is unbounded")
    }
}

impl Drop for WorkspaceStore {
    fn drop(&mut self) {
        // Index updates are best effort. Flushing here makes a shutdown after
        // an in-memory mutation leave no stale metadata behind; a failure is
        // already silent.
        self.flush_indexes();
    }
}

/// Validates a collection's in-memory folder list and returns the identifiers
/// that survived.
///
/// A repeated identifier keeps the first entry and excludes later ones. A
/// folder whose parent is missing, or a folder that closes a parent cycle, is
/// placed at the collection root. Every repair is reported once and no file is
/// written, so a hand-edited document keeps its bytes until an explicit save.
/// Siblings are ordered by lowercase name and then identifier.
fn resolve_folders(
    collection: &mut Collection,
    collection_path: &Path,
    issues: &mut Vec<StorageIssue>,
) -> HashSet<FolderId> {
    let mut seen = HashSet::new();
    let mut kept = Vec::with_capacity(collection.folders.len());
    let mut duplicates = Vec::new();
    for folder in std::mem::take(&mut collection.folders) {
        if seen.insert(folder.id) {
            kept.push(folder);
        } else {
            duplicates.push(folder.id);
        }
    }
    collection.folders = kept;
    for id in duplicates {
        issues.push(issue_for(
            collection_path,
            format!("duplicate folder identifier {id}; the later entry was ignored"),
        ));
    }

    let mut parent_of: HashMap<FolderId, Option<FolderId>> = collection
        .folders
        .iter()
        .map(|folder| (folder.id, folder.parent_id))
        .collect();
    let mut ids: Vec<FolderId> = parent_of.keys().copied().collect();
    ids.sort();

    for id in ids {
        let mut visited = HashSet::from([id]);
        // The folder whose remaining parent edge is broken: the chain start,
        // or the child of the missing or repeated parent.
        let mut child = id;
        let mut missing_parent = None;
        let mut cycle_at = None;
        while let Some(parent) = parent_of[&child] {
            if !parent_of.contains_key(&parent) {
                missing_parent = Some(parent);
                break;
            }
            if !visited.insert(parent) {
                cycle_at = Some(parent);
                break;
            }
            child = parent;
        }

        if let Some(parent) = missing_parent {
            let name = collection
                .folder(child)
                .map(|folder| folder.name.clone())
                .unwrap_or_default();
            parent_of.insert(child, None);
            if let Some(folder) = collection.folder_mut(child) {
                folder.parent_id = None;
            }
            issues.push(issue_for(
                collection_path,
                format!(
                    "folder {name:?} refers to missing parent folder {parent}; moved to the collection root"
                ),
            ));
        } else if let Some(node) = cycle_at {
            let name = collection
                .folder(node)
                .map(|folder| folder.name.clone())
                .unwrap_or_default();
            parent_of.insert(node, None);
            if let Some(folder) = collection.folder_mut(node) {
                folder.parent_id = None;
            }
            issues.push(issue_for(
                collection_path,
                format!("folder {name:?} is part of a parent cycle; moved to the collection root"),
            ));
        }
    }

    collection.folders.sort_by(|a, b| {
        a.parent_id
            .cmp(&b.parent_id)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });

    parent_of.into_keys().collect()
}

/// Returns whether `slug` is a single, safe path component.
pub fn is_valid_slug(slug: &str) -> bool {
    if slug.is_empty() || slug == "." || slug == ".." || slug.contains(['/', '\\', ':', '\0']) {
        return false;
    }

    let mut components = Path::new(slug).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Parses a request filename stem into the UUID used as its identity.
pub fn parse_request_stem(stem: &str) -> Option<Uuid> {
    Uuid::parse_str(stem).ok()
}

/// Builds a readable, filesystem-safe slug from a display name. `fallback` is
/// used when the name has no alphanumeric characters.
fn slugify(name: &str, fallback: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for character in name.trim().to_lowercase().chars() {
        if character.is_alphanumeric() {
            slug.push(character);
            pending_dash = false;
        } else if !slug.is_empty() && !pending_dash {
            slug.push('-');
            pending_dash = true;
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { fallback } else { slug };
    let slug = truncate_chars(slug, MAX_SLUG_CHARS);
    if is_reserved_name(&slug) {
        format!("_{}", truncate_chars(&slug, MAX_SLUG_CHARS - 1))
    } else {
        slug
    }
}

fn with_suffix(base: &str, suffix: &str, fallback: &str) -> String {
    let max_base = MAX_SLUG_CHARS.saturating_sub(suffix.chars().count());
    let truncated = truncate_chars(base, max_base);
    let base = truncated.trim_end_matches('-');
    let base = if base.is_empty() { fallback } else { base };
    format!("{base}{suffix}")
}

fn is_reserved_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(name.as_str(), "con" | "prn" | "aux" | "nul" | "clock$")
        || (name.len() == 4
            && (name.starts_with("com") || name.starts_with("lpt"))
            && name.as_bytes()[3].is_ascii_digit()
            && name.as_bytes()[3] != b'0')
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// `path` relative to `root` when it canonically lies inside it.
///
/// Both sides are canonicalized first, so `..`, a symlink, and a differently
/// spelled root are resolved before comparing. Windows path components compare
/// case-insensitively.
fn relative_under(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = fs::canonicalize(root).ok()?;
    let path = fs::canonicalize(path).ok()?;
    let root_parts: Vec<Component<'_>> = root.components().collect();
    let path_parts: Vec<Component<'_>> = path.components().collect();
    if path_parts.len() <= root_parts.len() {
        return None;
    }
    let inside = root_parts
        .iter()
        .zip(&path_parts)
        .all(|(root, path)| components_match(root, path));
    if !inside {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in &path_parts[root_parts.len()..] {
        relative.push(component.as_os_str());
    }
    Some(relative)
}

fn components_match(first: &Component<'_>, second: &Component<'_>) -> bool {
    match (first, second) {
        (Component::Prefix(first), Component::Prefix(second)) => first.kind() == second.kind(),
        (Component::RootDir, Component::RootDir) | (Component::CurDir, Component::CurDir) => true,
        (Component::ParentDir, Component::ParentDir) => true,
        (Component::Normal(first), Component::Normal(second)) => {
            if cfg!(windows) {
                first.eq_ignore_ascii_case(second)
            } else {
                first == second
            }
        }
        _ => false,
    }
}

/// A path with `/` separators, the form a document records.
fn slash_path(path: &Path) -> String {
    let mut slash = String::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            if !slash.is_empty() {
                slash.push('/');
            }
            slash.push_str(&part.to_string_lossy());
        }
    }
    slash
}

pub(crate) fn ensure_dir(path: &Path) -> Result<(), StorageError> {
    fs::create_dir_all(path).map_err(|error| io_error(path, error))
}

pub(crate) fn io_error(path: &Path, error: std::io::Error) -> StorageError {
    StorageError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

pub(crate) fn issue_for(path: &Path, message: impl Into<String>) -> StorageIssue {
    StorageIssue {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn push_error_issue(issues: &mut Vec<StorageIssue>, error: &StorageError) {
    if let Some(issue) = StorageIssue::from_error(error) {
        issues.push(issue);
    }
}

/// A document that carries a `schema_version` this build can check.
pub trait Versioned {
    fn schema_version(&self) -> u32;
}

impl Versioned for Workspace {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl Versioned for Collection {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl Versioned for RequestSpec {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl Versioned for Environment {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

/// Reads one JSON document and rejects a newer `schema_version`.
pub(crate) fn read_versioned<T: DeserializeOwned + Versioned>(
    path: &Path,
) -> Result<T, StorageError> {
    let contents = fs::read_to_string(path).map_err(|error| io_error(path, error))?;
    let value: T = serde_json::from_str(&contents).map_err(|error| StorageError::Parse {
        path: path.to_path_buf(),
        format: "JSON",
        message: error.to_string(),
    })?;
    if value.schema_version() > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: value.schema_version(),
            supported: SCHEMA_VERSION,
        });
    }
    Ok(value)
}

/// What [`JsonStore::load`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome<T> {
    /// The document does not exist, or the path is not a readable file.
    Absent,
    /// The document exists and parsed.
    Loaded(T),
}

/// One machine-local JSON document addressed by its file path.
///
/// The store owns root/version/write plumbing only: it reads through
/// [`read_versioned`], so a newer `schema_version` is rejected the same way for
/// every document, and it writes through [`persist_bytes_atomic`], the single
/// atomic temp-file-and-rename path. Each document's own facade keeps its
/// tolerance policy and its `Loaded*` result type.
#[derive(Debug, Clone)]
pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    /// A store for an explicit document path.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A store for `file` inside `root`.
    pub fn in_dir(root: impl Into<PathBuf>, file: &str) -> Self {
        Self {
            path: root.into().join(file),
        }
    }

    /// The document path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes atomic-write residue beside the document. Best-effort.
    pub fn cleanup(&self) {
        if let Some(directory) = self.path.parent() {
            cleanup_temporary_files(directory);
        }
    }

    /// Reads the document, rejecting a newer schema. Residue from an
    /// interrupted write beside it is removed first. A missing or non-file path
    /// is [`LoadOutcome::Absent`], not an error.
    pub fn load<T: DeserializeOwned + Versioned>(&self) -> Result<LoadOutcome<T>, StorageError> {
        self.cleanup();
        if !self.path.is_file() {
            return Ok(LoadOutcome::Absent);
        }
        read_versioned::<T>(&self.path).map(LoadOutcome::Loaded)
    }

    /// Serializes `value` exactly as [`write_document_atomic`] does: pretty
    /// JSON with one trailing newline.
    pub fn encode<T: Serialize>(&self, value: &T) -> Result<Vec<u8>, StorageError> {
        let mut contents =
            serde_json::to_string_pretty(value).map_err(|error| StorageError::Serialize {
                path: self.path.clone(),
                message: error.to_string(),
            })?;
        contents.push('\n');
        Ok(contents.into_bytes())
    }

    /// Encoded bytes straight to the atomic writer.
    pub fn write_encoded(&self, bytes: &[u8]) -> Result<(), StorageError> {
        persist_bytes_atomic(&self.path, bytes)
    }

    /// Serializes and atomically replaces the document.
    pub fn write<T: Serialize>(&self, value: &T) -> Result<(), StorageError> {
        let bytes = self.encode(value)?;
        self.write_encoded(&bytes)
    }
}

/// Suffix of an atomic write's temporary file. The name is recognizable on
/// purpose: `cleanup_temporary_files` removes residue an interrupted write left
/// behind, and `.gitignore` hides it in a workspace under version control.
pub(crate) const TEMP_SUFFIX: &str = ".posthaste-tmp";

/// Writes one document as pretty-printed JSON with a trailing newline.
///
/// The content is serialized before the file is touched and written through
/// [`persist_bytes_atomic`], so an interrupted write leaves the previous
/// document intact.
pub(crate) fn write_document_atomic<T: Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), StorageError> {
    JsonStore::at(path).write(value)
}

/// Replaces `path` with `bytes` by writing a temporary sibling and renaming it.
///
/// This is the only place that builds a temporary file for a document write.
/// The temporary file carries [`TEMP_SUFFIX`] so an interrupted write is
/// recognizable and removable by [`cleanup_temporary_files`], and it is created
/// in the destination directory so the final rename stays on one filesystem.
///
/// The rename makes the replacement atomic against process interruption: a
/// crash leaves either the previous complete file or the new one. Nothing is
/// flushed to the device, so a power loss can still lose the newest write; see
/// the module documentation for the durability contract.
pub(crate) fn persist_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), StorageError> {
    let parent = path.parent().ok_or_else(|| StorageError::InvalidEntry {
        path: path.to_path_buf(),
        reason: "destination has no parent directory".into(),
    })?;
    ensure_dir(parent)?;

    let mut temp = tempfile::Builder::new()
        .prefix(".posthaste-")
        .suffix(TEMP_SUFFIX)
        .tempfile_in(parent)
        .map_err(|error| io_error(path, error))?;
    temp.write_all(bytes)
        .map_err(|error| io_error(path, error))?;
    temp.flush().map_err(|error| io_error(path, error))?;
    temp.persist(path)
        .map_err(|error| io_error(path, error.error))?;
    Ok(())
}

/// Renames a damaged document out of the way and returns its preserved path.
///
/// The new name is `<name>.corrupt-<unix-millis>`. A name that already exists
/// gets `-2`, `-3`, ... appended, so an earlier quarantine is never replaced,
/// even within the same millisecond. A rename keeps the original bytes on disk
/// and removes the damaged path, so a later save cannot target it.
fn quarantine_document(path: &Path) -> Result<PathBuf, StorageError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StorageError::InvalidEntry {
            path: path.to_path_buf(),
            reason: "document name is not valid UTF-8".into(),
        })?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    for attempt in 1.. {
        let suffix = if attempt == 1 {
            format!("{QUARANTINE_MARKER}{millis}")
        } else {
            format!("{QUARANTINE_MARKER}{millis}-{attempt}")
        };
        let candidate = path.with_file_name(format!("{name}{suffix}"));
        if candidate.exists() {
            continue;
        }
        return fs::rename(path, &candidate)
            .map(|()| candidate)
            .map_err(|error| io_error(path, error));
    }
    unreachable!("suffix space is unbounded")
}

/// Whether `path` is lexically inside `root`.
///
/// `Path::starts_with` compares components only, so a path that leaves the root
/// through `..` and names something there would otherwise pass. Every component
/// after the root prefix is required to be a normal name.
fn is_beneath(root: &Path, path: &Path) -> bool {
    if !path.starts_with(root) {
        return false;
    }
    let root_components = root.components().count();
    path.components()
        .skip(root_components)
        .all(|component| !matches!(component, Component::ParentDir))
}

/// Whether a file name carries the quarantine marker and timestamp.
///
/// The accepted forms are `<stem>.corrupt-<millis>` and
/// `<stem>.corrupt-<millis>-<n>`, with a non-empty stem and decimal numbers.
fn is_quarantine_name(name: &str) -> bool {
    let Some((stem, suffix)) = name.rsplit_once(QUARANTINE_MARKER) else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    let mut parts = suffix.split('-');
    let Some(millis) = parts.next() else {
        return false;
    };
    if !is_decimal(millis) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(attempt) if is_decimal(attempt) && parts.next().is_none() => true,
        _ => false,
    }
}

fn is_decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Removes atomic-write residue from a directory the app owns.
///
/// Only files whose name ends with [`TEMP_SUFFIX`] are removed; every other
/// file is left alone. Best-effort by design: a file that cannot be removed
/// now is simply retried the next time the directory is loaded, and failing to
/// clean residue must never fail a load.
pub(crate) fn cleanup_temporary_files(directory: &Path) {
    let Ok(read_dir) = fs::read_dir(directory) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let is_residue = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(TEMP_SUFFIX));
        if is_residue && path.is_file() {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Lists one document per `.json` file stem in `read_dir`. Files with any other
/// extension are ignored. Stems are returned in sorted order so issues and
/// loads are deterministic.
pub(crate) fn documents_by_stem(
    read_dir: fs::ReadDir,
    dir: &Path,
    kind: &str,
    issues: &mut Vec<StorageIssue>,
) -> Vec<(String, PathBuf)> {
    let mut documents: HashMap<String, PathBuf> = HashMap::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                issues.push(issue_for(dir, error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if extension != DOCUMENT_EXTENSION {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            issues.push(issue_for(
                &path,
                format!("{kind} file name is not valid UTF-8"),
            ));
            continue;
        };
        documents.insert(stem.to_owned(), path);
    }

    let mut documents: Vec<(String, PathBuf)> = documents.into_iter().collect();
    documents.sort_by(|a, b| a.0.cmp(&b.0));
    documents
}

/// One request file found while scanning a collection's requests directory.
struct ScannedRequest {
    path: PathBuf,
    /// Present when the file's metadata could be read.
    fingerprint: Option<FileFingerprint>,
}

/// Lists one request document per `.json` file stem. Each carries the
/// fingerprint used for cache reuse.
fn scan_request_documents(
    read_dir: fs::ReadDir,
    dir: &Path,
    issues: &mut Vec<StorageIssue>,
) -> Vec<(String, ScannedRequest)> {
    let mut documents: HashMap<String, ScannedRequest> = HashMap::new();
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                issues.push(issue_for(dir, error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        let is_file = match entry.file_type() {
            Ok(file_type) => file_type.is_file(),
            Err(error) => {
                issues.push(issue_for(&path, error.to_string()));
                continue;
            }
        };
        if !is_file {
            continue;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if extension != DOCUMENT_EXTENSION {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            issues.push(issue_for(&path, "request file name is not valid UTF-8"));
            continue;
        };
        documents.insert(
            stem.to_owned(),
            ScannedRequest {
                fingerprint: FileFingerprint::from_dir_entry(&entry),
                path,
            },
        );
    }

    let mut documents: Vec<(String, ScannedRequest)> = documents.into_iter().collect();
    documents.sort_by(|a, b| a.0.cmp(&b.0));
    documents
}

/// The UUID v5 key that isolates one workspace's cached metadata.
fn workspace_key(root: &Path) -> Uuid {
    let canonical = match fs::canonicalize(root) {
        Ok(canonical) => canonical,
        Err(_) => match (root.parent(), root.file_name()) {
            (Some(parent), Some(name)) => match fs::canonicalize(parent) {
                Ok(parent) => parent.join(name),
                Err(_) => root.to_path_buf(),
            },
            _ => root.to_path_buf(),
        },
    };
    Uuid::new_v5(
        &WORKSPACE_KEY_NAMESPACE,
        canonical.to_string_lossy().as_bytes(),
    )
}

/// A workspace folder that cannot be created for the requested name.
#[derive(Debug, Error)]
pub enum WorkspaceCreateError {
    /// The name is empty or unusable as a folder name.
    #[error(transparent)]
    Name(#[from] WorkspaceNameError),
    /// The target exists and already holds something.
    #[error("{path}: the folder is not empty")]
    TargetNotEmpty { path: PathBuf },
    /// Creating or bootstrapping the directory failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A directory operation failed outside the storage helper.
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

/// Why a workspace name cannot be used as a folder name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WorkspaceNameError {
    #[error("Enter a workspace name.")]
    Empty,
    #[error("The name contains a character that cannot be used in a folder name.")]
    InvalidCharacter,
    #[error("That name is reserved on this platform.")]
    Reserved,
}

/// Trims and validates a workspace name.
///
/// The name becomes one directory component, so path separators, the Windows
/// forbidden characters, control characters, trailing dots, and reserved
/// device names are rejected. The returned value is the trimmed name.
pub fn validate_workspace_name(name: &str) -> Result<&str, WorkspaceNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(WorkspaceNameError::Empty);
    }
    if name == "." || name == ".." || name.ends_with('.') {
        return Err(WorkspaceNameError::InvalidCharacter);
    }
    if name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
    }) {
        return Err(WorkspaceNameError::InvalidCharacter);
    }
    let stem = name.split('.').next().unwrap_or(name);
    if is_reserved_name(stem) {
        return Err(WorkspaceNameError::Reserved);
    }
    Ok(name)
}

/// The default parent for a newly created workspace: `Posthaste` beneath the
/// platform documents directory, or beneath the home directory when documents
/// is unavailable.
pub fn default_workspace_parent() -> Option<PathBuf> {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .map(|directory| directory.join(paths::PRODUCT_DIR))
}

/// Whether `directory` already holds a workspace document, so choosing it
/// opens the workspace rather than asking to initialize one.
pub fn has_workspace_document(directory: &Path) -> bool {
    directory.join(WORKSPACE_FILE).is_file()
}

/// Creates `<location>/<name>` as a workspace and returns its store.
///
/// A missing location and target are created. An existing empty directory is
/// bootstrapped in place. An existing target holding any entry is rejected and
/// left untouched.
pub fn create_workspace(
    location: &Path,
    name: &str,
) -> Result<WorkspaceStore, WorkspaceCreateError> {
    let name = validate_workspace_name(name)?;
    let target = location.join(name);
    match fs::metadata(&target) {
        Ok(metadata) if metadata.is_dir() => {
            let mut entries = fs::read_dir(&target).map_err(|error| WorkspaceCreateError::Io {
                path: target.clone(),
                message: error.to_string(),
            })?;
            if entries.next().is_some() {
                return Err(WorkspaceCreateError::TargetNotEmpty { path: target });
            }
        }
        Ok(_) => return Err(WorkspaceCreateError::TargetNotEmpty { path: target }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&target).map_err(|error| WorkspaceCreateError::Io {
                path: target.clone(),
                message: error.to_string(),
            })?;
        }
        Err(error) => {
            return Err(WorkspaceCreateError::Io {
                path: target,
                message: error.to_string(),
            });
        }
    }

    let store = WorkspaceStore::at(&target);
    store.bootstrap_named(name)?;
    Ok(store)
}

/// Normalizes a directory path for storage: made absolute and canonicalized
/// when the directory exists, with the Windows verbatim prefix removed.
pub fn normalize_directory_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let canonical = fs::canonicalize(&absolute).unwrap_or(absolute);
    strip_verbatim_prefix(canonical)
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// Whether two paths name the same directory. Paths are canonicalized when
/// possible, and on Windows the comparison is case-insensitive.
pub fn same_directory(first: &Path, second: &Path) -> bool {
    #[cfg(windows)]
    let identity = |path: &Path| {
        PathBuf::from(
            normalize_directory_path(path)
                .to_string_lossy()
                .to_lowercase(),
        )
    };
    #[cfg(not(windows))]
    let identity = |path: &Path| normalize_directory_path(path);
    identity(first) == identity(second)
}

/// Removes a document, ignoring a file that is already gone.
pub(crate) fn remove_document(path: &Path) -> Result<(), StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use tempfile::TempDir;

    use super::*;

    fn store() -> (TempDir, WorkspaceStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = WorkspaceStore::at(dir.path());
        (dir, store)
    }

    fn read_collection(store: &WorkspaceStore, slug: &str) -> Collection {
        let path = store.collection_file(slug).unwrap();
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn read_request(store: &WorkspaceStore, slug: &str, id: RequestId) -> RequestSpec {
        let path = store.request_file(slug, id).unwrap();
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn read_environment(store: &WorkspaceStore, slug: &str) -> Environment {
        let path = store.environment_file(slug).unwrap();
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    struct SeededTree {
        collection: Collection,
        auth: FolderId,
        tokens: FolderId,
        billing: FolderId,
        login: RequestSpec,
        status: RequestSpec,
        ping: RequestSpec,
    }

    /// Creates a collection with `Auth/Tokens` and `Billing`, plus one request
    /// in each folder and one at the collection root.
    fn seed_tree(store: &WorkspaceStore) -> SeededTree {
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let auth = collection.add_folder("Auth", None);
        let tokens = collection.add_folder("Tokens", Some(auth));
        let billing = collection.add_folder("Billing", None);
        store.save_collection(&collection).unwrap();

        let mut login = store.create_request(&collection.id, "Login").unwrap();
        login.folder_id = Some(tokens);
        store.save_request(&collection.id, &login).unwrap();
        let mut status = store.create_request(&collection.id, "Status").unwrap();
        status.folder_id = Some(auth);
        store.save_request(&collection.id, &status).unwrap();
        let ping = store.create_request(&collection.id, "Ping").unwrap();

        SeededTree {
            collection,
            auth,
            tokens,
            billing,
            login,
            status,
            ping,
        }
    }

    fn loaded_entry<'a>(loaded: &'a mut LoadedWorkspace, slug: &str) -> &'a mut LoadedCollection {
        loaded
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
            .expect("collection loaded")
    }

    #[test]
    fn resolution_override_wins() {
        let store = WorkspaceStore::resolve_from(
            Some(OsString::from("/tmp/custom-workspace")),
            Some(PathBuf::from("/tmp/data")),
        )
        .unwrap();
        assert_eq!(store.root(), Path::new("/tmp/custom-workspace"));
    }

    #[test]
    fn empty_override_falls_back_to_data_directory() {
        let store =
            WorkspaceStore::resolve_from(Some(OsString::new()), Some(PathBuf::from("/tmp/data")))
                .unwrap();
        assert_eq!(
            store.root(),
            Path::new("/tmp/data").join("Posthaste").join("workspace")
        );
    }

    #[test]
    fn missing_data_directory_is_reported() {
        let error = WorkspaceStore::resolve_from(None, None).unwrap_err();
        assert!(matches!(error, StorageError::NoDataDir));
    }

    fn workspace_in(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).expect("create workspace dir");
        fs::write(dir.join(WORKSPACE_FILE), "{}").expect("write workspace.json");
        dir.to_path_buf()
    }

    #[test]
    fn launch_path_wins_over_every_other_source() {
        let dir = tempfile::tempdir().expect("dir");
        let launch = dir.path().join("launch");
        let remembered = workspace_in(&dir.path().join("remembered"));

        let resolution = WorkspaceStore::resolve_with(
            Some(launch.clone()),
            Some(OsString::from(dir.path().join("environment"))),
            Some(remembered),
            Some(dir.path().join("data")),
            None,
        )
        .expect("resolve");

        assert_eq!(resolution.store.root(), launch);
        assert_eq!(resolution.unavailable_remembered, None);
        assert!(
            resolution.explicit,
            "a launch path is an explicit selection"
        );
    }

    #[test]
    fn environment_wins_over_the_remembered_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let environment = dir.path().join("environment");
        let remembered = workspace_in(&dir.path().join("remembered"));

        let resolution = WorkspaceStore::resolve_with(
            None,
            Some(environment.clone().into_os_string()),
            Some(remembered),
            Some(dir.path().join("data")),
            None,
        )
        .expect("resolve");

        assert_eq!(resolution.store.root(), environment);
    }

    #[test]
    fn a_usable_remembered_workspace_is_opened() {
        let dir = tempfile::tempdir().expect("dir");
        let remembered = workspace_in(&dir.path().join("remembered"));

        let resolution = WorkspaceStore::resolve_with(
            None,
            None,
            Some(remembered.clone()),
            Some(dir.path().join("data")),
            None,
        )
        .expect("resolve");

        assert_eq!(resolution.store.root(), remembered);
        assert_eq!(resolution.unavailable_remembered, None);
    }

    #[test]
    fn an_unusable_remembered_workspace_falls_back_with_a_notice() {
        let dir = tempfile::tempdir().expect("dir");
        let data = dir.path().join("data");
        let missing = dir.path().join("deleted-workspace");

        let resolution = WorkspaceStore::resolve_with(
            None,
            None,
            Some(missing.clone()),
            Some(data.clone()),
            None,
        )
        .expect("resolve");

        assert_eq!(
            resolution.store.root(),
            data.join(paths::PRODUCT_DIR).join("workspace")
        );
        assert_eq!(resolution.unavailable_remembered, Some(missing.clone()));
        assert!(!missing.exists(), "nothing is created at the skipped path");
    }

    #[test]
    fn a_remembered_directory_without_a_workspace_file_falls_back() {
        let dir = tempfile::tempdir().expect("dir");
        let data = dir.path().join("data");
        let plain = dir.path().join("plain");
        fs::create_dir(&plain).expect("create plain dir");

        let resolution =
            WorkspaceStore::resolve_with(None, None, Some(plain.clone()), Some(data.clone()), None)
                .expect("resolve");

        assert_eq!(
            resolution.store.root(),
            data.join(paths::PRODUCT_DIR).join("workspace")
        );
        assert_eq!(resolution.unavailable_remembered, Some(plain));
    }

    #[test]
    fn empty_overrides_fall_through_to_the_remembered_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let remembered = workspace_in(&dir.path().join("remembered"));

        let resolution = WorkspaceStore::resolve_with(
            Some(PathBuf::new()),
            Some(OsString::new()),
            Some(remembered.clone()),
            Some(dir.path().join("data")),
            None,
        )
        .expect("resolve");

        assert_eq!(resolution.store.root(), remembered);
        assert!(
            !resolution.explicit,
            "a remembered workspace is not an explicit selection"
        );
    }

    #[test]
    fn bootstrap_writes_the_layout_and_leaves_unrelated_files_alone() {
        let dir = tempfile::tempdir().expect("dir");
        let unrelated = dir.path().join("notes.txt");
        fs::write(&unrelated, "keep me").expect("write unrelated file");
        let store = WorkspaceStore::at(dir.path());

        let workspace = store.bootstrap().expect("bootstrap");

        assert_eq!(workspace.name, DEFAULT_WORKSPACE_NAME);
        assert!(store.workspace_file().is_file());
        assert!(store.collections_dir().is_dir());
        assert!(store.environments_dir().is_dir());
        assert_eq!(
            fs::read_to_string(&unrelated).expect("read unrelated file"),
            "keep me"
        );
        let loaded = store.load().expect("load the bootstrapped workspace");
        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
        assert!(loaded.collections.is_empty());
        assert!(loaded.environments.is_empty());
    }

    #[test]
    fn bootstrap_refuses_to_replace_an_existing_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap_named("Original").expect("bootstrap");
        let before = fs::read_to_string(store.workspace_file()).expect("read");

        let error = store.bootstrap_named("Replacement").unwrap_err();

        assert!(matches!(error, StorageError::InvalidEntry { .. }));
        assert_eq!(
            fs::read_to_string(store.workspace_file()).expect("read"),
            before
        );
    }

    #[test]
    fn a_fresh_workspace_carries_both_git_friendly_dotfiles() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());

        store.load().expect("first run");

        assert_eq!(
            fs::read_to_string(dir.path().join(GITATTRIBUTES_FILE)).expect("attributes"),
            GITATTRIBUTES_CONTENT
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(GITIGNORE_FILE)).expect("ignore"),
            GITIGNORE_CONTENT
        );
        assert!(
            !dir.path().join(".git").exists(),
            "versioning the workspace stays the user's choice"
        );
    }

    #[test]
    fn bootstrapping_a_directory_writes_the_dotfiles_and_keeps_its_other_files() {
        let dir = tempfile::tempdir().expect("dir");
        let unrelated = dir.path().join("notes.txt");
        fs::write(&unrelated, "keep me").expect("write unrelated file");
        let store = WorkspaceStore::at(dir.path());

        let (_, issues) = store.bootstrap_with_issues().expect("bootstrap");

        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            fs::read_to_string(dir.path().join(GITATTRIBUTES_FILE)).expect("attributes"),
            GITATTRIBUTES_CONTENT
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(GITIGNORE_FILE)).expect("ignore"),
            ".posthaste-tmp\nhistory/\nThumbs.db\n.DS_Store\n",
            "a workspace created now hides this build's residue"
        );
        assert_eq!(
            fs::read_to_string(&unrelated).expect("read unrelated file"),
            "keep me"
        );
    }

    #[test]
    fn an_existing_dotfile_is_never_replaced() {
        let dir = tempfile::tempdir().expect("dir");
        let attributes = dir.path().join(GITATTRIBUTES_FILE);
        let ignore = dir.path().join(GITIGNORE_FILE);
        fs::write(&attributes, "*.json -text\n").expect("write attributes");
        fs::write(&ignore, "mine\n").expect("write ignore");
        let store = WorkspaceStore::at(dir.path());

        let (_, issues) = store.bootstrap_with_issues().expect("bootstrap");

        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(
            fs::read_to_string(&attributes).expect("attributes"),
            "*.json -text\n"
        );
        assert_eq!(fs::read_to_string(&ignore).expect("ignore"), "mine\n");
        assert!(store.workspace_file().is_file());
    }

    #[test]
    fn a_dotfile_that_cannot_be_created_is_reported_without_failing_the_bootstrap() {
        let dir = tempfile::tempdir().expect("dir");
        // A directory in the dotfile's place makes `create_new` fail with
        // something other than "already exists".
        fs::create_dir(dir.path().join(GITIGNORE_FILE)).expect("make gitignore a directory");
        let store = WorkspaceStore::at(dir.path());

        let (workspace, issues) = store.bootstrap_with_issues().expect("bootstrap");

        assert_eq!(workspace.name, DEFAULT_WORKSPACE_NAME);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].path, dir.path().join(GITIGNORE_FILE));
        assert!(
            issues[0].message.contains("was not created"),
            "{}",
            issues[0].message
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(GITATTRIBUTES_FILE)).expect("attributes"),
            GITATTRIBUTES_CONTENT,
            "one failure must not stop the other file"
        );
        let loaded = store.load_existing().expect("the workspace still loads");
        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
    }

    #[test]
    fn loading_removes_atomic_write_residue_and_keeps_every_other_file() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let (environment, _) = store.create_environment("Dev").expect("environment");

        let residue = [
            dir.path().join(format!(".posthaste-aaa{TEMP_SUFFIX}")),
            store
                .collection_dir(&collection.id)
                .expect("collection dir")
                .join(format!(".posthaste-bbb{TEMP_SUFFIX}")),
            store
                .requests_dir(&collection.id)
                .expect("requests dir")
                .join(format!(".posthaste-ccc{TEMP_SUFFIX}")),
            store
                .environments_dir()
                .join(format!(".posthaste-ddd{TEMP_SUFFIX}")),
        ];
        for path in &residue {
            fs::write(path, "{\"half\": ").expect("write residue");
        }
        let unrelated = dir.path().join(".posthaste-notes.txt");
        fs::write(&unrelated, "keep me").expect("write unrelated file");

        let loaded = store.load_existing().expect("load");

        for path in &residue {
            assert!(!path.exists(), "{} must be removed", path.display());
        }
        assert_eq!(
            fs::read_to_string(&unrelated).expect("unrelated file"),
            "keep me",
            "only files ending with the temporary suffix are removed"
        );
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, environment);
    }

    #[test]
    fn re_saving_an_unchanged_document_is_byte_identical_and_uses_lf() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap().expect("bootstrap");
        let collection = store.create_collection("Demo", 0).expect("collection");
        let request = store
            .create_request(&collection.id, "Ping")
            .expect("request");
        let (environment, _) = store.create_environment("Dev").expect("environment");
        let loaded = store.load_existing().expect("load");

        let files = [
            store.workspace_file(),
            store
                .collection_file(&collection.id)
                .expect("collection file"),
            store
                .request_file(&collection.id, request.id)
                .expect("request file"),
            store
                .environment_file(&environment)
                .expect("environment file"),
        ];
        let before: Vec<Vec<u8>> = files
            .iter()
            .map(|path| fs::read(path).expect("read document"))
            .collect();

        store
            .save_all(&loaded.workspace, &loaded.collections, &loaded.environments)
            .expect("re-save");

        for (path, before) in files.iter().zip(before) {
            let after = fs::read(path).expect("read document");
            assert_eq!(after, before, "{} changed without an edit", path.display());
            assert!(
                !after.contains(&b'\r'),
                "{} must use LF line endings",
                path.display()
            );
        }
    }

    #[test]
    fn a_picked_file_outside_the_workspace_is_copied_into_files() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path().join("workspace"));
        store.bootstrap().expect("bootstrap");
        let source = dir.path().join("Payload.BIN");
        fs::write(&source, b"\x00\x01payload").expect("write source");

        let relative = store.import_file(&source).expect("import");

        assert!(relative.starts_with("files/"), "{relative}");
        assert!(relative.ends_with(".bin"), "the extension is lowercased");
        let stored = store.resolve_asset(&relative).expect("resolve the record");
        assert!(stored.starts_with(store.files_dir()));
        assert_eq!(fs::read(&stored).expect("read copy"), b"\x00\x01payload");
        assert_eq!(
            fs::read(&source).expect("read source"),
            b"\x00\x01payload",
            "the picked file is copied, never moved"
        );
    }

    #[test]
    fn a_file_inside_the_workspace_is_referenced_where_it_is() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap().expect("bootstrap");
        let inside = dir.path().join("samples").join("body.json");
        fs::create_dir_all(inside.parent().expect("parent")).expect("samples dir");
        fs::write(&inside, "{}").expect("write inside file");

        let relative = store.import_file(&inside).expect("import");

        assert_eq!(relative, "samples/body.json");
        assert!(!store.files_dir().exists(), "nothing is copied");
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_copy_leaves_no_partial_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path().join("workspace"));
        store.bootstrap().expect("bootstrap");
        let source = dir.path().join("locked.bin");
        fs::write(&source, b"payload").expect("write source");
        // Denying every other opener makes the read fail before anything is
        // staged.
        let _locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&source)
            .expect("lock the source");

        let error = store.import_file(&source).expect_err("the copy must fail");

        assert!(matches!(error, StorageError::Io { .. }), "{error}");
        match fs::read_dir(store.files_dir()) {
            Ok(entries) => {
                assert_eq!(entries.count(), 0, "no partial or temporary file remains");
            }
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::NotFound,
                "an unreadable files directory that is not missing"
            ),
        }
    }

    #[test]
    fn asset_paths_that_escape_the_workspace_are_rejected() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());

        assert_eq!(
            store.resolve_asset("files/abc.bin"),
            Some(dir.path().join("files").join("abc.bin"))
        );
        assert_eq!(
            store.resolve_asset("notes.txt"),
            Some(dir.path().join("notes.txt"))
        );
        assert_eq!(store.resolve_asset("../outside.json"), None);
        assert_eq!(store.resolve_asset("files/../../outside.json"), None);
        assert_eq!(store.resolve_asset(""), None);
        #[cfg(windows)]
        assert_eq!(store.resolve_asset(r"C:\Windows\win.ini"), None);
        #[cfg(not(windows))]
        assert_eq!(store.resolve_asset("/etc/passwd"), None);
    }

    #[test]
    fn explicit_loading_never_creates_the_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let missing = dir.path().join("typo");
        let store = WorkspaceStore::at(&missing);

        let error = store.load_existing().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("the workspace directory does not exist"),
            "{error}"
        );
        assert!(!missing.exists(), "nothing may be written at the path");
        assert!(store.is_bootstrappable());
    }

    #[test]
    fn explicit_loading_rejects_a_directory_without_a_workspace_document() {
        let dir = tempfile::tempdir().expect("dir");
        let unrelated = dir.path().join("notes.txt");
        fs::write(&unrelated, "keep me").expect("write unrelated file");
        let store = WorkspaceStore::at(dir.path());

        let error = store.load_existing().unwrap_err();

        assert!(
            error.to_string().contains("workspace.json is missing"),
            "{error}"
        );
        assert!(!store.workspace_file().exists());
        assert!(!store.collections_dir().exists());
        assert_eq!(
            fs::read_to_string(&unrelated).expect("read unrelated file"),
            "keep me"
        );
        assert!(store.is_bootstrappable());
    }

    #[test]
    fn explicit_loading_reads_a_bootstrapped_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let store = WorkspaceStore::at(dir.path());
        store.bootstrap_named("Payments").expect("bootstrap");
        let collection = store.create_collection("Ledger", 0).expect("collection");

        let loaded = store.load_existing().expect("load the workspace");

        assert_eq!(loaded.workspace.name, "Payments");
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.id, collection.id);
        assert!(!store.is_bootstrappable());
    }

    #[test]
    fn two_spellings_of_one_directory_are_the_same() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("Workspace");
        fs::create_dir(&workspace).expect("create workspace dir");
        let other = dir.path().join("Other");
        fs::create_dir(&other).expect("create other dir");

        assert!(same_directory(&workspace, &workspace.join(".")));
        assert!(!same_directory(&workspace, &other));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_differing_only_in_case_are_the_same() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("Workspace");
        fs::create_dir(&workspace).expect("create workspace dir");
        let lowered = PathBuf::from(workspace.to_string_lossy().to_lowercase());

        assert!(same_directory(&workspace, &lowered));
    }

    #[test]
    fn workspace_names_are_trimmed_and_validated() {
        assert_eq!(validate_workspace_name("  Payments  "), Ok("Payments"));
        assert_eq!(validate_workspace_name("llm测试"), Ok("llm测试"));
        for name in [
            "", "   ", ".", "..", "a/b", "a\\b", "C:", "a*b", "a?b", "a\"b",
        ] {
            assert!(
                matches!(
                    validate_workspace_name(name),
                    Err(WorkspaceNameError::Empty | WorkspaceNameError::InvalidCharacter)
                ),
                "{name:?} should be rejected"
            );
        }
        for name in [
            "a<b",
            "a>b",
            "a|b",
            "ends with a dot.",
            "CON",
            "nul.txt",
            "LPT1",
        ] {
            assert!(
                matches!(
                    validate_workspace_name(name),
                    Err(WorkspaceNameError::InvalidCharacter | WorkspaceNameError::Reserved)
                ),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn create_workspace_makes_missing_parents_and_names_the_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let location = dir.path().join("Documents").join(paths::PRODUCT_DIR);

        let store = create_workspace(&location, "  Payments  ").expect("create workspace");

        assert_eq!(store.root(), location.join("Payments"));
        let loaded = store.load().expect("load");
        assert_eq!(loaded.workspace.name, "Payments");
    }

    #[test]
    fn create_workspace_bootstraps_an_existing_empty_directory_in_place() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("Scratch");
        fs::create_dir(&target).expect("create empty target");

        let store = create_workspace(dir.path(), "Scratch").expect("create workspace");

        assert_eq!(store.root(), target);
        assert!(store.workspace_file().is_file());
    }

    #[test]
    fn create_workspace_rejects_a_non_empty_target_without_touching_it() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("Taken");
        fs::create_dir(&target).expect("create target");
        let kept = target.join("keep.txt");
        fs::write(&kept, "keep me").expect("write kept file");

        let error = create_workspace(dir.path(), "Taken").unwrap_err();

        assert!(matches!(error, WorkspaceCreateError::TargetNotEmpty { .. }));
        assert_eq!(fs::read_to_string(&kept).expect("read kept"), "keep me");
        assert!(!target.join(WORKSPACE_FILE).exists());
    }

    #[test]
    fn create_workspace_rejects_an_invalid_name_without_writing() {
        let dir = tempfile::tempdir().expect("dir");

        let error = create_workspace(dir.path(), "a/b").unwrap_err();

        assert!(matches!(
            error,
            WorkspaceCreateError::Name(WorkspaceNameError::InvalidCharacter)
        ));
        assert_eq!(
            fs::read_dir(dir.path()).expect("read dir").count(),
            0,
            "no directory is created for a rejected name"
        );
    }

    #[test]
    fn rejects_unsafe_slugs() {
        for slug in ["", ".", "..", "a/b", "a\\b", "C:", "nested/child"] {
            assert!(!is_valid_slug(slug), "{slug:?} should be rejected");
        }
        assert!(is_valid_slug("my-collection"));
        assert!(is_valid_slug("用户接口"));
    }

    #[test]
    fn parses_only_uuid_request_stems() {
        let id = RequestId::new();
        assert_eq!(parse_request_stem(&id.to_string()), Some(id.as_uuid()));
        assert_eq!(parse_request_stem("not-a-uuid"), None);
        assert_eq!(parse_request_stem(""), None);
    }

    #[test]
    fn layout_paths_are_stable() {
        let store = WorkspaceStore::at("/tmp/ws");
        let id = RequestId::new();

        assert_eq!(store.workspace_file(), Path::new("/tmp/ws/workspace.json"));
        assert_eq!(store.collections_dir(), Path::new("/tmp/ws/collections"));
        assert_eq!(
            store.collection_file("demo").unwrap(),
            Path::new("/tmp/ws/collections/demo/collection.json")
        );
        assert_eq!(
            store.requests_dir("demo").unwrap(),
            Path::new("/tmp/ws/collections/demo/requests")
        );
        assert_eq!(
            store.request_file("demo", id).unwrap(),
            Path::new("/tmp/ws/collections/demo/requests").join(format!("{id}.json"))
        );
        assert_eq!(
            store.environment_file("staging").unwrap(),
            Path::new("/tmp/ws/environments/staging.json")
        );
        assert!(store.collection_file("../escape").is_err());
        assert!(store.environment_file("../escape").is_err());
    }

    #[test]
    fn bootstrap_creates_workspace_layout() {
        let (dir, store) = store();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
        assert!(store.workspace_file().is_file());
        assert!(store.collections_dir().is_dir());
        assert!(store.environments_dir().is_dir());
        assert!(loaded.collections.is_empty());
        assert!(loaded.issues.is_empty());

        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.workspace, loaded.workspace);
        assert_eq!(dir.path(), store.root());
    }

    #[test]
    fn refuses_a_non_empty_directory_without_workspace_file() {
        let (dir, store) = store();
        fs::write(dir.path().join("notes.txt"), "keep me").unwrap();

        let error = store.load().unwrap_err();
        assert!(matches!(error, StorageError::InvalidEntry { .. }));
        assert!(!store.workspace_file().exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn refuses_a_workspace_path_that_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::write(&root, "not a directory").unwrap();
        let store = WorkspaceStore::at(&root);

        let error = store.load().unwrap_err();
        assert!(matches!(error, StorageError::InvalidEntry { .. }));
    }

    #[test]
    fn creating_a_collection_round_trips() {
        let (_dir, store) = store();
        store.load().unwrap();

        let collection = store.create_collection("Payments", 2).unwrap();
        assert_eq!(collection.id, "payments");
        assert_eq!(collection.name, "Payments");
        assert_eq!(collection.order, 2);

        let loaded = store.load().unwrap();
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection, collection);
    }

    #[test]
    fn creating_a_request_round_trips_all_fields() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let mut request = store.create_request(&collection.id, "Get user").unwrap();
        request.method = crate::model::HttpMethod::Post;
        request.url = "https://example.com/users".into();
        request.headers = vec![crate::model::KeyValue::new("accept", "application/json")];
        request.query = vec![crate::model::KeyValue::new("page", "1")];
        request.body = crate::model::RequestBody::Json("{\"ok\":true}".into());
        request.timeout_ms = Some(5_000);
        store.save_request(&collection.id, &request).unwrap();

        let loaded = store.load().unwrap();
        let loaded_request = &loaded.collections[0].requests[0];
        assert_eq!(loaded_request.spec(), Some(&request));
    }

    #[test]
    fn slugifies_unicode_names() {
        let (_dir, store) = store();
        store.load().unwrap();

        let collection = store.create_collection("用户接口", 0).unwrap();
        assert_eq!(collection.id, "用户接口");
        assert!(store.collection_file(&collection.id).unwrap().is_file());
    }

    #[test]
    fn duplicate_names_get_unique_slugs() {
        let (_dir, store) = store();
        store.load().unwrap();

        let first = store.create_collection("New Collection", 0).unwrap();
        let second = store.create_collection("New Collection", 1).unwrap();

        assert_eq!(first.id, "new-collection");
        assert_eq!(second.id, "new-collection-2");
        assert_ne!(first.id, second.id);
        assert!(store.collection_file(&first.id).unwrap().is_file());
    }

    #[test]
    fn reserved_names_are_escaped() {
        let (_dir, store) = store();
        store.load().unwrap();

        let collection = store.create_collection("CON", 0).unwrap();
        assert_eq!(collection.id, "_con");
    }

    #[test]
    fn empty_names_get_a_default_slug() {
        let (_dir, store) = store();
        store.load().unwrap();

        let collection = store.create_collection("!!!", 0).unwrap();
        assert_eq!(collection.id, "collection");
    }

    #[test]
    fn creating_an_environment_round_trips() {
        let (_dir, store) = store();
        store.load().unwrap();

        let (slug, mut environment) = store.create_environment("Staging").unwrap();
        assert_eq!(slug, "staging");
        assert_eq!(environment.name, "Staging");
        assert!(store.environment_file(&slug).unwrap().is_file());

        environment
            .vars
            .insert("base_url".into(), "https://staging.example.com".into());
        store.save_environment(&slug, &environment).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, "staging");
        assert_eq!(loaded.environments[0].environment, environment);
        assert_eq!(read_environment(&store, &slug), environment);
    }

    #[test]
    fn duplicate_environment_names_get_unique_slugs() {
        let (_dir, store) = store();
        store.load().unwrap();

        let (first, _) = store.create_environment("New Environment").unwrap();
        let (second, _) = store.create_environment("New Environment").unwrap();

        assert_eq!(first, "new-environment");
        assert_eq!(second, "new-environment-2");
        assert!(store.environment_file(&first).unwrap().is_file());
        assert!(store.environment_file(&second).unwrap().is_file());
    }

    #[test]
    fn empty_environment_names_use_their_own_fallback() {
        let (_dir, store) = store();
        store.load().unwrap();

        let (slug, environment) = store.create_environment("!!!").unwrap();
        assert_eq!(slug, "environment");
        assert_eq!(environment.name, "!!!");

        let (slug, environment) = store.create_environment("   ").unwrap();
        assert_eq!(slug, "new-environment");
        assert_eq!(environment.name, "New Environment");
    }

    #[test]
    fn rename_slugs_exclude_the_entry_being_renamed() {
        let (_dir, store) = store();
        store.load().unwrap();
        store.create_collection("Demo", 0).unwrap();
        store.create_collection("Payments", 1).unwrap();
        store.create_environment("Dev").unwrap();
        store.create_environment("Prod").unwrap();

        assert_eq!(store.collection_slug_for("Demo", "demo").unwrap(), "demo");
        assert_eq!(store.collection_slug_for("DEMO", "demo").unwrap(), "demo");
        assert_eq!(
            store.collection_slug_for("Payments", "demo").unwrap(),
            "payments-2"
        );
        assert_eq!(
            store.collection_slug_for("Payments", "payments").unwrap(),
            "payments"
        );
        assert_eq!(
            store.environment_slug_for("Development", "dev").unwrap(),
            "development"
        );
        assert_eq!(store.environment_slug_for("PROD", "prod").unwrap(), "prod");
        assert_eq!(store.environment_slug_for("Prod", "dev").unwrap(), "prod-2");

        // The other entries' files are untouched.
        assert!(store.collection_file("payments").unwrap().is_file());
        assert!(store.environment_file("prod").unwrap().is_file());
    }

    #[test]
    fn moving_a_collection_keeps_its_requests() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();

        store.move_collection("demo", "payments").unwrap();
        assert!(!store.collection_dir("demo").unwrap().exists());
        assert!(store.collection_file("payments").unwrap().is_file());

        // The document still stores the old id until the caller rewrites it;
        // the loader adopts the directory name in the meantime.
        let loaded = store.load().unwrap();
        assert_eq!(loaded.collections[0].collection.id, "payments");
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
    }

    #[test]
    fn moving_an_environment_relocates_its_json_document() {
        let (_dir, store) = store();
        store.load().unwrap();
        let (slug, mut environment) = store.create_environment("Dev").unwrap();
        environment
            .vars
            .insert("host".into(), "dev.example.com".into());
        store.save_environment(&slug, &environment).unwrap();

        store.move_environment("dev", "development").unwrap();
        assert!(!store.environment_file("dev").unwrap().exists());
        assert!(store.environment_file("development").unwrap().is_file());

        let loaded = store.load().unwrap();
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, "development");
        assert_eq!(
            loaded.environments[0].environment.vars.get("host"),
            Some(&"dev.example.com".to_owned())
        );
    }

    #[test]
    fn a_failed_collection_move_leaves_the_directory_in_place() {
        let (_dir, store) = store();
        store.load().unwrap();
        store.create_collection("Demo", 0).unwrap();
        let blocked = store.collections_dir().join("taken");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("keep"), "keep").unwrap();

        let error = store.move_collection("demo", "taken").unwrap_err();
        assert!(matches!(error, StorageError::Io { .. }));
        assert!(store.collection_file("demo").unwrap().is_file());
        assert!(blocked.join("keep").is_file());
    }

    #[test]
    fn a_failed_environment_move_leaves_the_file_in_place() {
        let (_dir, store) = store();
        store.load().unwrap();
        store.create_environment("Dev").unwrap();
        let blocked = store.environment_file("taken").unwrap();
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("keep"), "keep").unwrap();

        let error = store.move_environment("dev", "taken").unwrap_err();
        assert!(matches!(error, StorageError::Io { .. }));
        assert!(store.environment_file("dev").unwrap().is_file());
        assert!(blocked.join("keep").is_file());
    }

    #[test]
    fn moving_a_missing_environment_is_an_error() {
        let (_dir, store) = store();
        store.load().unwrap();

        let error = store.move_environment("missing", "other").unwrap_err();
        assert!(matches!(error, StorageError::InvalidEntry { .. }));
    }

    #[test]
    fn loader_adopts_the_directory_name_when_the_stored_id_differs() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        fs::rename(
            store.collection_dir("demo").unwrap(),
            store.collection_dir("payments").unwrap(),
        )
        .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.id, "payments");
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.message.contains("does not match directory name"))
        );

        store
            .save_collection(&loaded.collections[0].collection)
            .unwrap();
        assert_eq!(read_collection(&store, "payments").id, "payments");
        assert!(!store.collection_dir("demo").unwrap().exists());
    }

    #[test]
    fn environments_load_sorted_by_name() {
        let (_dir, store) = store();
        store.load().unwrap();
        store.create_environment("Zulu").unwrap();
        store.create_environment("alpha").unwrap();
        store.create_environment("Beta").unwrap();

        let loaded = store.load().unwrap();
        let names: Vec<_> = loaded
            .environments
            .iter()
            .map(|entry| entry.environment.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "Beta", "Zulu"]);
    }

    #[test]
    fn deleting_environments_is_targeted() {
        let (_dir, store) = store();
        store.load().unwrap();
        let (keep, _) = store.create_environment("Keep").unwrap();
        let (remove, _) = store.create_environment("Remove").unwrap();

        store.delete_environment(&remove).unwrap();

        assert!(!store.environment_file(&remove).unwrap().exists());
        assert!(store.environment_file(&keep).unwrap().is_file());
        store.delete_environment(&remove).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, keep);
    }

    #[test]
    fn corrupt_environment_files_are_reported_and_left_untouched() {
        let (_dir, store) = store();
        store.load().unwrap();
        let (good, _) = store.create_environment("Good").unwrap();
        let (broken, _) = store.create_environment("Broken").unwrap();
        let broken_path = store.environment_file(&broken).unwrap();
        fs::write(&broken_path, b"not: [valid").unwrap();
        let before = fs::read(&broken_path).unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, good);
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, broken_path);
        assert!(loaded.issues[0].message.contains("invalid JSON"));
        assert_eq!(fs::read(&broken_path).unwrap(), before);
    }

    #[test]
    fn newer_schema_environments_are_rejected_and_left_untouched() {
        let (_dir, store) = store();
        store.load().unwrap();
        let (slug, _) = store.create_environment("Future").unwrap();
        let path = store.environment_file(&slug).unwrap();
        fs::write(&path, r#"{"schema_version":99,"name":"Future","vars":{}}"#).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.environments.is_empty());
        assert_eq!(loaded.issues.len(), 1);
        assert!(loaded.issues[0].message.contains("newer than supported"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn unsupported_environment_files_and_directories_are_ignored() {
        let (_dir, store) = store();
        store.load().unwrap();
        fs::write(store.environments_dir().join("notes.txt"), "ignore").unwrap();
        fs::create_dir(store.environments_dir().join("nested")).unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        assert!(loaded.environments.is_empty());
    }

    #[test]
    fn collections_sort_by_order_then_name() {
        let (_dir, store) = store();
        store.load().unwrap();
        store.create_collection("Zulu", 2).unwrap();
        store.create_collection("Alpha", 1).unwrap();
        store.create_collection("Beta", 1).unwrap();

        let loaded = store.load().unwrap();
        let names: Vec<_> = loaded
            .collections
            .iter()
            .map(|entry| entry.collection.name.as_str())
            .collect();
        assert_eq!(names, ["Alpha", "Beta", "Zulu"]);
    }

    #[test]
    fn requests_sort_by_name() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        store.create_request(&collection.id, "Zeta").unwrap();
        store.create_request(&collection.id, "alpha").unwrap();

        let loaded = store.load().unwrap();
        let names: Vec<_> = loaded.collections[0]
            .requests
            .iter()
            .map(|request| request.name())
            .collect();
        assert_eq!(names, ["alpha", "Zeta"]);
    }

    #[test]
    fn renaming_does_not_move_files() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Original", 0).unwrap();
        let request = store
            .create_request(&collection.id, "Original request")
            .unwrap();
        let collection_path = store.collection_file(&collection.id).unwrap();
        let request_path = store.request_file(&collection.id, request.id).unwrap();

        collection.name = "Renamed".into();
        store.save_collection(&collection).unwrap();
        let mut renamed_request = request.clone();
        renamed_request.name = "Renamed request".into();
        store
            .save_request(&collection.id, &renamed_request)
            .unwrap();

        assert!(collection_path.is_file());
        assert!(request_path.is_file());
        assert_eq!(read_collection(&store, &collection.id).name, "Renamed");
        assert_eq!(
            read_request(&store, &collection.id, request.id).name,
            "Renamed request"
        );
    }

    #[test]
    fn folder_entries_round_trip_through_the_collection_document() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let auth = collection.add_folder("Auth", None);
        let tokens = collection.add_folder("Tokens", Some(auth));
        store.save_collection(&collection).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        let stored = &loaded.collections[0].collection;
        assert_eq!(stored.folders.len(), 2);
        assert_eq!(stored.folder(auth).unwrap().name, "Auth");
        assert_eq!(stored.folder(auth).unwrap().parent_id, None);
        assert_eq!(stored.folder(tokens).unwrap().parent_id, Some(auth));
        assert_eq!(read_collection(&store, &collection.id), *stored);
    }

    #[test]
    fn folder_identifiers_stay_stable_across_rename_and_move() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let alpha = collection.add_folder("Alpha", None);
        let beta = collection.add_folder("Beta", None);
        let child = collection.add_folder("Child", Some(alpha));
        store.save_collection(&collection).unwrap();
        let collection_path = store.collection_file(&collection.id).unwrap();

        assert!(collection.rename_folder(alpha, "Renamed"));
        assert!(collection.move_folder(child, Some(beta)));
        store.save_collection(&collection).unwrap();

        let stored = read_collection(&store, &collection.id);
        assert_eq!(stored.folders.len(), 3);
        assert_eq!(stored.folder(alpha).unwrap().name, "Renamed");
        assert_eq!(stored.folder(child).unwrap().name, "Child");
        assert_eq!(stored.folder(child).unwrap().parent_id, Some(beta));
        assert!(collection_path.is_file());

        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        let reloaded = &loaded.collections[0].collection;
        assert_eq!(reloaded.folders.len(), 3);
        assert_eq!(reloaded.folder(alpha).unwrap().name, "Renamed");
        assert_eq!(reloaded.folder(child).unwrap().parent_id, Some(beta));
    }

    #[test]
    fn saving_a_collection_replaces_the_folder_list_atomically() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let first = collection.add_folder("First", None);
        collection.add_folder("Second", None);
        store.save_collection(&collection).unwrap();

        collection.remove_folder(first);
        store.save_collection(&collection).unwrap();

        let stored = read_collection(&store, &collection.id);
        assert_eq!(stored.folders.len(), 1);
        assert_eq!(stored.folders[0].name, "Second");

        let mut leftovers: Vec<_> = fs::read_dir(store.collection_dir(&collection.id).unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        leftovers.sort();
        assert_eq!(leftovers, ["collection.json", "requests"]);
    }

    #[test]
    fn folderless_collections_keep_their_file_unchanged() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&before).contains("folders"));

        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        assert_eq!(fs::read(&path).unwrap(), before);

        let mut edited = loaded.collections[0].collection.clone();
        edited.order = 3;
        store.save_collection(&edited).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("folders"));
    }

    /// Creates a folder entry with a fixed identifier, bypassing the cycle
    /// checks that the collection API applies to interactive moves.
    fn raw_folder(id: FolderId, name: &str, parent_id: Option<FolderId>) -> crate::model::Folder {
        let mut folder = crate::model::Folder::new(name, parent_id);
        folder.id = id;
        folder
    }

    #[test]
    fn nested_folders_load_with_sorted_siblings_and_requests() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let beta = raw_folder(FolderId::new(), "beta", None);
        let alpha_first = raw_folder(FolderId::new(), "alpha", None);
        let alpha_second = raw_folder(FolderId::new(), "Alpha", None);
        let tokens = raw_folder(FolderId::new(), "Tokens", Some(alpha_first.id));
        collection.folders = vec![
            beta.clone(),
            tokens.clone(),
            alpha_second.clone(),
            alpha_first.clone(),
        ];
        store.save_collection(&collection).unwrap();

        let mut login = store.create_request(&collection.id, "Login").unwrap();
        login.folder_id = Some(tokens.id);
        store.save_request(&collection.id, &login).unwrap();
        let status = store.create_request(&collection.id, "Status").unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        let stored = &loaded.collections[0].collection;
        // Root folders sort by lowercase name, ties by identifier; children
        // follow their parent group.
        assert_eq!(
            stored.folders,
            vec![alpha_first.clone(), alpha_second, beta, tokens.clone()]
        );
        assert_eq!(
            stored.folder(tokens.id).unwrap().parent_id,
            Some(alpha_first.id)
        );

        let requests = &loaded.collections[0].requests;
        let login_loaded = requests
            .iter()
            .find(|request| request.id() == login.id)
            .unwrap();
        assert_eq!(login_loaded.folder_id(), Some(tokens.id));
        let status_loaded = requests
            .iter()
            .find(|request| request.id() == status.id)
            .unwrap();
        assert_eq!(status_loaded.folder_id(), None);
    }

    #[test]
    fn loaded_folder_accessors_order_and_count_a_nested_tree() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let auth = collection.add_folder("Auth", None);
        let billing = collection.add_folder("billing", None);
        let tokens = collection.add_folder("Tokens", Some(auth));
        let keys = collection.add_folder("keys", Some(auth));
        let refresh = collection.add_folder("Refresh", Some(tokens));
        store.save_collection(&collection).unwrap();

        let mut login = store.create_request(&collection.id, "login").unwrap();
        login.folder_id = Some(tokens);
        store.save_request(&collection.id, &login).unwrap();
        let mut refresh_request = store
            .create_request(&collection.id, "Refresh token")
            .unwrap();
        refresh_request.folder_id = Some(refresh);
        store
            .save_request(&collection.id, &refresh_request)
            .unwrap();
        let mut key_request = store.create_request(&collection.id, "Key").unwrap();
        key_request.folder_id = Some(keys);
        store.save_request(&collection.id, &key_request).unwrap();
        let root_request = store.create_request(&collection.id, "Root").unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        let entry = &loaded.collections[0];

        let roots: Vec<_> = entry
            .folder_children(None)
            .iter()
            .map(|folder| folder.name.as_str())
            .collect();
        assert_eq!(roots, ["Auth", "billing"]);
        let auth_children: Vec<_> = entry
            .folder_children(Some(auth))
            .iter()
            .map(|folder| folder.name.as_str())
            .collect();
        assert_eq!(auth_children, ["keys", "Tokens"]);
        assert!(entry.folder_children(Some(billing)).is_empty());

        let root_requests = entry.folder_requests(None);
        assert_eq!(root_requests.len(), 1);
        assert_eq!(root_requests[0].spec(), Some(&root_request));
        let token_requests = entry.folder_requests(Some(tokens));
        assert_eq!(token_requests.len(), 1);
        assert_eq!(token_requests[0].spec(), Some(&login));
        assert!(entry.folder_requests(Some(auth)).is_empty());

        assert_eq!(
            entry.folder_subtree_ids(auth),
            vec![auth, keys, tokens, refresh]
        );
        assert_eq!(entry.folder_subtree_ids(billing), vec![billing]);
        assert!(entry.folder_subtree_ids(FolderId::new()).is_empty());

        assert_eq!(entry.folder_ancestors(refresh), vec![auth, tokens]);
        assert_eq!(entry.folder_ancestors(tokens), vec![auth]);
        assert!(entry.folder_ancestors(auth).is_empty());

        assert_eq!(entry.folder_request_count(auth), 3);
        assert_eq!(entry.folder_request_count(tokens), 2);
        assert_eq!(entry.folder_request_count(refresh), 1);
        assert_eq!(entry.folder_request_count(keys), 1);
        assert_eq!(entry.folder_request_count(billing), 0);
        assert_eq!(entry.folder_request_count(FolderId::new()), 0);
        assert_eq!(entry.folders().len(), 5);
        assert_eq!(entry.folder(auth).unwrap().name, "Auth");
    }

    #[test]
    fn collection_without_a_folders_field_loads_as_empty() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("folders"));

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        assert!(loaded.collections[0].collection.folders.is_empty());
    }

    #[test]
    fn duplicate_folder_identifiers_keep_the_first_entry() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let id = FolderId::new();
        let first = raw_folder(id, "First", None);
        let duplicate = raw_folder(id, "Second", None);
        collection.folders = vec![first.clone(), duplicate];
        store.save_collection(&collection).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.folders, vec![first]);
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, path);
        assert!(loaded.issues[0].message.contains("duplicate folder"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn orphaned_folders_are_re_rooted() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let orphan = raw_folder(FolderId::new(), "Orphan", Some(FolderId::new()));
        let child = raw_folder(FolderId::new(), "Child", Some(orphan.id));
        collection.folders = vec![orphan.clone(), child.clone()];
        store.save_collection(&collection).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        let stored = &loaded.collections[0].collection;
        assert_eq!(
            stored.folders.len(),
            2,
            "orphaned folders are never dropped"
        );
        assert_eq!(stored.folder(orphan.id).unwrap().parent_id, None);
        assert_eq!(stored.folder(child.id).unwrap().parent_id, Some(orphan.id));
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, path);
        assert!(loaded.issues[0].message.contains("missing parent"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn orphaned_requests_are_re_rooted() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let mut request = store.create_request(&collection.id, "Orphan").unwrap();
        request.folder_id = Some(FolderId::new());
        store.save_request(&collection.id, &request).unwrap();
        let path = store.request_file(&collection.id, request.id).unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].folder_id(), None);
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, path);
        assert!(loaded.issues[0].message.contains("unknown folder"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn parent_cycles_are_broken_without_dropping_entries() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let a = raw_folder(FolderId::new(), "Alpha", None);
        let b = raw_folder(FolderId::new(), "Beta", Some(a.id));
        let mut a_cyclic = a.clone();
        a_cyclic.parent_id = Some(b.id);
        collection.folders = vec![a_cyclic, b.clone()];
        store.save_collection(&collection).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        let before = fs::read(&path).unwrap();
        let request = store.create_request(&collection.id, "Inside").unwrap();

        let loaded = store.load().unwrap();

        let stored = &loaded.collections[0].collection;
        assert_eq!(stored.folders.len(), 2, "both folders still load");
        assert_eq!(
            stored
                .folders
                .iter()
                .filter(|f| f.parent_id.is_none())
                .count(),
            1
        );
        assert!(stored.folder(a.id).is_some() && stored.folder(b.id).is_some());
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, path);
        assert!(loaded.issues[0].message.contains("cycle"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn malformed_folder_lists_exclude_the_collection() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        fs::write(
            &path,
            "schema_version: 1\nid: demo\nname: Demo\norder: 0\nfolders: not-a-list\n",
        )
        .unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.collections.is_empty());
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, path);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn folder_identifiers_must_be_uuids() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        fs::write(
            &path,
            "schema_version: 1\nid: demo\nname: Demo\norder: 0\nfolders:\n  - id: not-a-uuid\n    name: Bad\n",
        )
        .unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.collections.is_empty());
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, path);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn deleting_collections_is_targeted() {
        let (_dir, store) = store();
        store.load().unwrap();
        let keep = store.create_collection("Keep", 0).unwrap();
        let remove = store.create_collection("Remove", 1).unwrap();
        let request = store.create_request(&remove.id, "Gone").unwrap();

        store.delete_collection(&remove.id).unwrap();

        assert!(!store.collection_dir(&remove.id).unwrap().exists());
        assert!(store.collection_dir(&keep.id).unwrap().is_dir());
        store.delete_collection(&remove.id).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.id, keep.id);
        assert!(!store.request_file(&remove.id, request.id).unwrap().exists());
    }

    #[test]
    fn deleting_requests_is_targeted() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let keep = store.create_request(&collection.id, "Keep").unwrap();
        let remove = store.create_request(&collection.id, "Remove").unwrap();

        store.delete_request(&collection.id, remove.id).unwrap();

        assert!(
            !store
                .request_file(&collection.id, remove.id)
                .unwrap()
                .exists()
        );
        assert!(
            store
                .request_file(&collection.id, keep.id)
                .unwrap()
                .is_file()
        );
        let loaded = store.load().unwrap();
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].spec(), Some(&keep));
    }

    #[test]
    fn full_re_saves_rewrite_folder_lists() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let auth = collection.add_folder("Auth", None);
        let tokens = collection.add_folder("Tokens", Some(auth));
        let loaded = LoadedCollection {
            collection: collection.clone(),
            requests: Vec::new(),
        };

        assert!(
            store
                .save_collections(std::slice::from_ref(&loaded))
                .is_success()
        );
        let mut stored = read_collection(&store, &collection.id);
        assert_eq!(stored.folders.len(), 2);
        assert_eq!(stored.folder(tokens).unwrap().parent_id, Some(auth));

        let mut edited = collection.clone();
        assert!(edited.rename_folder(auth, "Authentication"));
        let loaded = LoadedCollection {
            collection: edited,
            requests: Vec::new(),
        };
        store
            .save_all(&Workspace::new("ws"), std::slice::from_ref(&loaded), &[])
            .unwrap();

        stored = read_collection(&store, &collection.id);
        assert_eq!(stored.folder(auth).unwrap().name, "Authentication");
        assert_eq!(stored.folder(tokens).unwrap().parent_id, Some(auth));
    }

    #[test]
    fn cascade_deletion_removes_the_subtree_and_leaves_others_untouched() {
        let (_dir, store) = store();
        store.load().unwrap();
        let seeded = seed_tree(&store);
        let other = store.create_collection("Other", 1).unwrap();
        let other_path = store.collection_file(&other.id).unwrap();
        let other_before = fs::read(&other_path).unwrap();

        let mut loaded = store.load().unwrap();
        let entry = loaded_entry(&mut loaded, &seeded.collection.id);
        let issues = store
            .delete_folder_subtree(entry, &[seeded.auth, seeded.tokens])
            .unwrap();

        assert!(issues.is_empty());
        assert!(entry.collection.folder(seeded.auth).is_none());
        assert!(entry.collection.folder(seeded.tokens).is_none());
        assert_eq!(
            entry.collection.folder(seeded.billing).unwrap().parent_id,
            None
        );
        assert_eq!(entry.requests.len(), 1);
        assert_eq!(entry.requests[0].id(), seeded.ping.id);

        let requests = store.requests_dir(&seeded.collection.id).unwrap();
        assert!(!requests.join(format!("{}.json", seeded.login.id)).exists());
        assert!(!requests.join(format!("{}.json", seeded.status.id)).exists());
        assert!(
            store
                .request_file(&seeded.collection.id, seeded.ping.id)
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read(&other_path).unwrap(), other_before);

        let reloaded = store.load().unwrap();
        assert!(reloaded.issues.is_empty());
        let stored = reloaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == seeded.collection.id)
            .unwrap();
        assert_eq!(stored.collection.folders.len(), 1);
        assert_eq!(stored.requests.len(), 1);
        assert_eq!(stored.requests[0].id(), seeded.ping.id);
    }

    #[test]
    fn failed_request_deletion_re_roots_the_request() {
        let (_dir, store) = store();
        store.load().unwrap();
        let seeded = seed_tree(&store);
        let mut loaded = store.load().unwrap();
        let entry = loaded_entry(&mut loaded, &seeded.collection.id);
        let login_path = store
            .request_file(&seeded.collection.id, seeded.login.id)
            .unwrap();

        // A directory in place of the file makes deletion fail.
        fs::remove_file(&login_path).unwrap();
        fs::create_dir(&login_path).unwrap();

        let issues = store
            .delete_folder_subtree(entry, &[seeded.auth, seeded.tokens])
            .unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, login_path);
        assert!(issues[0].message.contains("could not be deleted"));
        assert!(login_path.exists(), "the undeleted request keeps its file");
        let login = entry
            .requests
            .iter()
            .find(|request| request.id() == seeded.login.id)
            .expect("the undeleted request stays loaded");
        assert_eq!(login.folder_id(), None);
        assert!(
            entry
                .requests
                .iter()
                .all(|request| request.id() != seeded.status.id),
            "successfully deleted requests leave memory"
        );
        assert_eq!(entry.requests.len(), 2);
    }

    #[test]
    fn failed_collection_write_leaves_requests_intact() {
        let (_dir, store) = store();
        store.load().unwrap();
        let seeded = seed_tree(&store);
        let mut loaded = store.load().unwrap();
        let entry = loaded_entry(&mut loaded, &seeded.collection.id);
        let collection_path = store.collection_file(&seeded.collection.id).unwrap();
        let folders_before = entry.collection.folders.clone();
        let requests_before = entry.requests.clone();

        // A directory at the document path makes the atomic rename fail.
        fs::remove_file(&collection_path).unwrap();
        fs::create_dir(&collection_path).unwrap();

        let error = store
            .delete_folder_subtree(entry, &[seeded.auth, seeded.tokens])
            .unwrap_err();

        assert!(matches!(error, StorageError::Io { .. }));
        assert_eq!(entry.collection.folders, folders_before);
        assert_eq!(entry.requests, requests_before);
        for id in [seeded.login.id, seeded.status.id, seeded.ping.id] {
            assert!(
                store
                    .request_file(&seeded.collection.id, id)
                    .unwrap()
                    .is_file()
            );
        }
    }

    #[test]
    fn corrupt_files_are_reported_and_left_untouched() {
        let (_dir, store) = store();
        store.load().unwrap();
        let good = store.create_collection("Good", 0).unwrap();
        let broken = store.create_collection("Broken", 1).unwrap();
        let broken_path = store.collection_file(&broken.id).unwrap();
        fs::write(&broken_path, b"not: [valid").unwrap();
        let before = fs::read(&broken_path).unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.id, good.id);
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, broken_path);
        assert!(loaded.issues[0].message.contains("invalid JSON"));
        assert_eq!(fs::read(&broken_path).unwrap(), before);
    }

    #[test]
    fn newer_schema_files_are_rejected_and_left_untouched() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Future", 0).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        fs::write(
            &path,
            r#"{"schema_version":99,"id":"future","name":"Future","order":0}"#,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.collections.is_empty());
        assert_eq!(loaded.issues.len(), 1);
        assert!(loaded.issues[0].message.contains("newer than supported"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Request").unwrap();
        let path = store.request_file(&collection.id, request.id).unwrap();
        let mut document: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        document["future_field"] = serde_json::json!("ignored");
        fs::write(&path, serde_json::to_string_pretty(&document).unwrap()).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.collections[0].requests[0].name(), "Request");
    }

    #[test]
    fn id_mismatches_adopt_the_directory_name() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let path = store.collection_file(&collection.id).unwrap();
        fs::write(
            &path,
            r#"{"schema_version":1,"id":"other","name":"Demo","order":0}"#,
        )
        .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].collection.id, "demo");
        assert!(loaded.issues[0].message.contains("does not match"));
    }

    #[test]
    fn missing_directories_are_created() {
        let (dir, store) = store();
        store.load().unwrap();
        fs::remove_dir_all(store.collections_dir()).unwrap();
        fs::remove_dir_all(store.environments_dir()).unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.collections.is_empty());
        assert!(store.collections_dir().is_dir());
        assert!(store.environments_dir().is_dir());
        assert_eq!(dir.path(), store.root());
    }

    #[test]
    fn unreadable_request_files_are_reported_without_aborting() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let good = store.create_request(&collection.id, "Good").unwrap();
        let bad = store.create_request(&collection.id, "Bad").unwrap();
        let bad_path = store.request_file(&collection.id, bad.id).unwrap();
        fs::write(&bad_path, [0xff, 0xfe, 0xfd]).unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].spec(), Some(&good));
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].path, bad_path);
        assert!(!loaded.issues[0].message.is_empty());
    }

    #[test]
    fn missing_requests_directory_is_recreated() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        fs::remove_dir_all(store.requests_dir(&collection.id).unwrap()).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.collections.len(), 1);
        assert!(store.requests_dir(&collection.id).unwrap().is_dir());
    }

    #[test]
    fn unsupported_files_and_subdirectories_are_ignored() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let requests = store.requests_dir(&collection.id).unwrap();
        fs::write(requests.join("notes.txt"), "ignore").unwrap();
        fs::create_dir(requests.join("nested")).unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        assert!(loaded.collections[0].requests.is_empty());
    }

    #[test]
    fn save_all_restores_changed_in_memory_state() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Original", 0).unwrap();
        let request = store
            .create_request(&collection.id, "Original request")
            .unwrap();
        let (environment_slug, _) = store.create_environment("Original env").unwrap();

        let mut loaded = store.load().unwrap();
        loaded.workspace.name = "Renamed workspace".into();
        loaded.collections[0].collection.name = "Renamed collection".into();
        loaded.collections[0].requests[0].spec_mut().unwrap().name = "Renamed request".into();
        loaded.environments[0].environment.name = "Renamed env".into();
        loaded.environments[0]
            .environment
            .vars
            .insert("host".into(), "example.com".into());
        store
            .save_all(&loaded.workspace, &loaded.collections, &loaded.environments)
            .unwrap();

        let reloaded = store.load().unwrap();
        assert_eq!(reloaded.workspace.name, "Renamed workspace");
        assert_eq!(
            reloaded.collections[0].collection.name,
            "Renamed collection"
        );
        assert_eq!(
            reloaded.collections[0].requests[0].name(),
            "Renamed request"
        );
        assert_eq!(reloaded.collections[0].requests[0].id(), request.id);
        assert_eq!(reloaded.environments[0].slug, environment_slug);
        assert_eq!(reloaded.environments[0].environment.name, "Renamed env");
        assert_eq!(
            reloaded.environments[0].environment.vars.get("host"),
            Some(&"example.com".to_owned())
        );
    }

    #[derive(Serialize)]
    struct Sample {
        value: u32,
    }

    #[test]
    fn atomic_write_replaces_content_without_leftovers() {
        let (dir, _store) = store();
        let path = dir.path().join("document.json");
        fs::write(&path, "{\"value\": 1}\n").unwrap();

        write_document_atomic(&path, &Sample { value: 2 }).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n  \"value\": 2\n}\n");
        let json_files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        assert_eq!(json_files.len(), 1);
    }

    #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
    struct VersionedSample {
        #[serde(default)]
        schema_version: u32,
        value: u32,
    }

    impl Versioned for VersionedSample {
        fn schema_version(&self) -> u32 {
            self.schema_version
        }
    }

    #[test]
    fn json_store_reports_the_same_load_outcomes_as_the_typed_stores() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::in_dir(dir.path(), "sample.json");

        assert_eq!(
            store.load::<VersionedSample>().unwrap(),
            LoadOutcome::Absent,
            "a missing document is absent, not an error"
        );

        store
            .write(&VersionedSample {
                schema_version: SCHEMA_VERSION,
                value: 7,
            })
            .unwrap();
        assert_eq!(
            store.load::<VersionedSample>().unwrap(),
            LoadOutcome::Loaded(VersionedSample {
                schema_version: SCHEMA_VERSION,
                value: 7,
            })
        );
        assert!(
            !fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().ends_with(TEMP_SUFFIX)),
            "a completed write leaves no temporary residue"
        );

        fs::write(store.path(), b"{ not json").unwrap();
        let error = store.load::<VersionedSample>().unwrap_err();
        assert!(matches!(error, StorageError::Parse { .. }), "got {error:?}");

        fs::write(
            store.path(),
            format!(r#"{{"schema_version": {}}}"#, SCHEMA_VERSION + 1),
        )
        .unwrap();
        let error = store.load::<VersionedSample>().unwrap_err();
        assert!(
            matches!(error, StorageError::UnsupportedSchema { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn stores_rooted_at_one_directory_write_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let first = JsonStore::in_dir(dir.path(), "first.json");
        let second = JsonStore::in_dir(dir.path(), "second.json");

        first
            .write(&VersionedSample {
                schema_version: SCHEMA_VERSION,
                value: 1,
            })
            .unwrap();
        second
            .write(&VersionedSample {
                schema_version: SCHEMA_VERSION,
                value: 2,
            })
            .unwrap();

        assert_ne!(first.path(), second.path());
        assert_eq!(
            first.load::<VersionedSample>().unwrap(),
            LoadOutcome::Loaded(VersionedSample {
                schema_version: SCHEMA_VERSION,
                value: 1,
            }),
            "one store never reads the other's document"
        );
        assert_eq!(
            second.load::<VersionedSample>().unwrap(),
            LoadOutcome::Loaded(VersionedSample {
                schema_version: SCHEMA_VERSION,
                value: 2,
            })
        );
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "only the two documents remain: {names:?}");
        assert!(
            names.iter().all(|name| !name.ends_with(TEMP_SUFFIX)),
            "neither store sees the other's temporary residue: {names:?}"
        );
    }

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(<S::Error as serde::ser::Error>::custom("synthetic failure"))
        }
    }

    #[test]
    fn serialization_failure_leaves_destination_unchanged() {
        let (dir, _store) = store();
        let path = dir.path().join("document.json");
        fs::write(&path, "{\"value\": 1}\n").unwrap();

        let error = write_document_atomic(&path, &FailingSerialize).unwrap_err();

        assert!(matches!(error, StorageError::Serialize { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"value\": 1}\n");
    }

    #[test]
    fn temp_file_creation_failure_is_reported() {
        let (dir, _store) = store();
        let parent_as_file = dir.path().join("not-a-dir");
        fs::write(&parent_as_file, "file").unwrap();
        let path = parent_as_file.join("document.json");

        let error = write_document_atomic(&path, &Sample { value: 1 }).unwrap_err();
        assert!(matches!(error, StorageError::Io { .. }));
        assert_eq!(fs::read_to_string(&parent_as_file).unwrap(), "file");
    }

    #[test]
    fn json_documents_round_trip_byte_identically() {
        let (_dir, store) = store();
        let loaded = store.load().unwrap();
        let path = store.workspace_file();
        let before = fs::read(&path).unwrap();
        let text = String::from_utf8(before.clone()).unwrap();
        assert!(text.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(value["schema_version"], 1);

        let report = store.save_workspace(&loaded.workspace).unwrap();

        assert!(report.is_success());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn corrupt_json_is_reported_and_left_untouched() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let path = store.request_file(&collection.id, request.id).unwrap();
        fs::write(&path, "not: [valid").unwrap();
        let before = fs::read(&path).unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.collections[0].requests.is_empty());
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path == path && issue.message.contains("invalid JSON"))
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn workspace_document_corruption_is_quarantined_and_replaced() {
        let (_dir, store) = store();
        store.load().unwrap();
        fs::write(store.workspace_file(), "not: [valid").unwrap();
        let before = fs::read(store.workspace_file()).unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
        assert_eq!(loaded.issues.len(), 1);
        let quarantined = fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("workspace.json.corrupt-"))
            })
            .expect("the damaged document is preserved");
        assert!(loaded.issues[0].path == quarantined);
        assert_eq!(fs::read(&quarantined).unwrap(), before);
        assert!(store.workspace_file().is_file());
        assert_ne!(fs::read(store.workspace_file()).unwrap(), before);

        // Creating a collection and saving again writes a valid document and
        // never touches the preserved copy.
        store.create_collection("Fresh", 0).unwrap();
        let report = store.save_workspace(&loaded.workspace).unwrap();
        assert!(report.is_success());
        assert_eq!(fs::read(&quarantined).unwrap(), before);
        assert_eq!(
            read_versioned::<Workspace>(&store.workspace_file())
                .unwrap()
                .name,
            DEFAULT_WORKSPACE_NAME
        );
    }

    fn quarantine_files(store: &WorkspaceStore) -> Vec<PathBuf> {
        fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".corrupt-"))
            })
            .collect()
    }

    #[cfg(windows)]
    #[test]
    fn an_undetachable_quarantine_is_reported_and_never_overwritten() {
        use std::os::windows::fs::OpenOptionsExt;

        let (_dir, store) = store();
        store.load().unwrap();
        let path = store.workspace_file();
        fs::write(&path, "not: [valid").unwrap();
        let before = fs::read(&path).unwrap();

        // An exclusive handle makes both reading the document and renaming it
        // fail, the way a locked file would in practice.
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let loaded = store.load().unwrap();
        drop(locked);

        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path == path && issue.message.contains("could not be preserved")),
            "{:?}",
            loaded.issues
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(quarantine_files(&store).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_undetachable_quarantine_is_reported_and_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = store();
        store.load().unwrap();
        let path = store.workspace_file();
        fs::write(&path, "not: [valid").unwrap();
        let before = fs::read(&path).unwrap();

        // A directory that cannot be written cannot accept the rename.
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o555)).unwrap();
        let loaded = store.load().unwrap();
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path == path && issue.message.contains("could not be preserved")),
            "{:?}",
            loaded.issues
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(quarantine_files(&store).is_empty());
    }

    #[test]
    fn quarantine_files_are_never_enumerated_as_documents() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let (environment_slug, _environment) = store.create_environment("Dev").unwrap();
        let preserved = store.root().join("workspace.json.corrupt-123");
        fs::write(&preserved, "not: [valid").unwrap();
        let nested = store
            .request_file(&collection.id, request.id)
            .unwrap()
            .with_file_name("preserved.json.corrupt-42");
        fs::write(&nested, "not: [valid").unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert_eq!(loaded.environments.len(), 1);
        assert_eq!(loaded.environments[0].slug, environment_slug);
        assert_eq!(fs::read(&preserved).unwrap(), b"not: [valid");
        assert_eq!(fs::read(&nested).unwrap(), b"not: [valid");
        assert!(store.is_quarantined_document(&nested));
    }

    #[test]
    fn only_quarantined_documents_are_deletable() {
        let (_dir, store) = store();
        store.load().unwrap();
        let preserved = store.root().join("workspace.json.corrupt-1700000000000");
        let collided = store.root().join("workspace.json.corrupt-1700000000000-2");
        let nested_dir = store.collections_dir().join("demo").join(REQUESTS_DIR);
        fs::create_dir_all(&nested_dir).unwrap();
        let nested = nested_dir.join("request.json.corrupt-9");
        for path in [&preserved, &collided, &nested] {
            fs::write(path, b"damaged").unwrap();
        }

        assert!(store.is_quarantined_document(&preserved));
        assert!(store.is_quarantined_document(&collided));
        assert!(store.is_quarantined_document(&nested));

        assert!(!store.is_quarantined_document(&store.workspace_file()));
        assert!(!store.is_quarantined_document(&store.root().join("workspace.json.corrupt")));
        assert!(!store.is_quarantined_document(&store.root().join("workspace.json.corrupt-abc")));
        assert!(!store.is_quarantined_document(&store.root().join("workspace.json.corrupt-5-x")));
        assert!(!store.is_quarantined_document(&store.root().join(".corrupt-5")));

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("workspace.json.corrupt-5");
        fs::write(&outside, b"damaged").unwrap();
        assert!(!store.is_quarantined_document(&outside));

        let traversal = store
            .root()
            .join("..")
            .join(store.root().file_name().unwrap())
            .join("workspace.json.corrupt-5");
        assert!(!store.is_quarantined_document(&traversal));
    }

    #[test]
    fn deleting_a_quarantined_document_removes_only_that_file() {
        let (_dir, store) = store();
        store.load().unwrap();
        let preserved = store.root().join("workspace.json.corrupt-77");
        fs::write(&preserved, b"damaged").unwrap();
        let workspace_before = fs::read(store.workspace_file()).unwrap();

        store.delete_quarantined_document(&preserved).unwrap();

        assert!(!preserved.exists());
        assert_eq!(fs::read(store.workspace_file()).unwrap(), workspace_before);

        // A preserved file that is already gone counts as deleted.
        store.delete_quarantined_document(&preserved).unwrap();
    }

    #[test]
    fn deleting_a_live_document_is_refused() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let live = store.request_file(&collection.id, request.id).unwrap();
        let before = fs::read(&live).unwrap();

        let error = store.delete_quarantined_document(&live).unwrap_err();

        assert!(matches!(error, StorageError::InvalidEntry { .. }));
        assert_eq!(fs::read(&live).unwrap(), before);
    }

    #[cfg(windows)]
    #[test]
    fn a_locked_quarantined_document_is_not_deleted() {
        use std::os::windows::fs::OpenOptionsExt;

        let (_dir, store) = store();
        store.load().unwrap();
        let preserved = store.root().join("workspace.json.corrupt-5");
        fs::write(&preserved, b"damaged").unwrap();
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&preserved)
            .unwrap();

        let error = store.delete_quarantined_document(&preserved).unwrap_err();
        drop(locked);

        assert!(matches!(error, StorageError::Io { .. }), "{error:?}");
        assert_eq!(fs::read(&preserved).unwrap(), b"damaged");
    }

    #[cfg(unix)]
    #[test]
    fn a_quarantined_document_in_an_unwritable_directory_is_not_deleted() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store) = store();
        store.load().unwrap();
        let preserved = store.root().join("workspace.json.corrupt-5");
        fs::write(&preserved, b"damaged").unwrap();
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o555)).unwrap();

        let error = store.delete_quarantined_document(&preserved).unwrap_err();
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(error, StorageError::Io { .. }), "{error:?}");
        assert_eq!(fs::read(&preserved).unwrap(), b"damaged");
    }

    #[test]
    fn save_all_reports_one_failed_document_and_writes_the_rest() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let (environment_slug, _environment) = store.create_environment("Dev").unwrap();
        let loaded = store.load().unwrap();

        // A directory in place of the request file makes that one write fail.
        let request_path = store.request_file(&collection.id, request.id).unwrap();
        fs::remove_file(&request_path).unwrap();
        fs::create_dir(&request_path).unwrap();

        let report = store
            .save_all(&loaded.workspace, &loaded.collections, &loaded.environments)
            .unwrap();

        assert!(!report.is_success());
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].path, request_path);
        assert!(!report.written.contains(&request_path));
        for expected in [
            store.workspace_file(),
            store.collection_file(&collection.id).unwrap(),
            store.environment_file(&environment_slug).unwrap(),
        ] {
            assert!(
                report.written.contains(&expected),
                "{} was not written",
                expected.display()
            );
        }
        assert!(
            serde_json::from_str::<serde_json::Value>(
                &fs::read_to_string(store.workspace_file()).unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn interrupted_write_residue_is_cleaned_up_without_touching_documents() {
        let (dir, _store) = store();
        let path = dir.path().join("document.json");
        fs::write(&path, "{\"value\": 1}\n").unwrap();
        let residue = dir
            .path()
            .join(format!(".posthaste-interrupted{TEMP_SUFFIX}"));
        fs::write(&residue, "{\"value\": 2").unwrap();

        cleanup_temporary_files(dir.path());

        assert!(!residue.exists(), "the interrupted write's residue is gone");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"value\": 1}\n",
            "the previous document is untouched"
        );
    }

    // ---------------------------------------------------------------------
    // Lazy request loading
    // ---------------------------------------------------------------------

    fn cached_store() -> (TempDir, TempDir, WorkspaceStore) {
        let root = tempfile::tempdir().expect("workspace dir");
        let cache = tempfile::tempdir().expect("cache dir");
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        (root, cache, store)
    }

    fn index_entry(name: &str) -> RequestIndexEntry {
        let id = RequestId::new();
        RequestIndexEntry {
            id,
            name: name.to_owned(),
            method: HttpMethod::Get,
            url: String::new(),
            protocol: RequestProtocol::Http,
            folder_id: None,
            file_name: format!("{id}.json"),
            modified_s: 1,
            modified_ns: 2,
            size: 3,
        }
    }

    fn unloaded_entry(request: &RequestSpec, path: &Path) -> RequestEntry {
        RequestEntry::Unloaded {
            summary: RequestSummary {
                id: request.id,
                name: request.name.clone(),
                method: request.method,
                url: request.url.clone(),
                protocol: request.protocol,
                folder_id: request.folder_id,
            },
            fingerprint: FileFingerprint::from_path(path).expect("fingerprint"),
        }
    }

    fn patch_index_fingerprint(store: &WorkspaceStore, slug: &str, path: &Path) {
        let mut index = store.read_index(slug).expect("index exists");
        let fingerprint = FileFingerprint::from_path(path).expect("fingerprint");
        let entry = index.entries.first_mut().expect("cached entry");
        entry.file_name = fingerprint.file_name;
        entry.modified_s = fingerprint.modified_s;
        entry.modified_ns = fingerprint.modified_ns;
        entry.size = fingerprint.size;
        store.write_index(slug, &index).expect("write index");
    }

    fn tree_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).expect("read tree").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    #[ignore = "owner runs the release baseline; cargo test -p posthaste-core --release -- --ignored --nocapture load_baseline"]
    fn load_baseline() {
        let root = tempfile::tempdir().expect("workspace dir");
        let cache = tempfile::tempdir().expect("cache dir");
        let store = WorkspaceStore::at(root.path());
        store.load().expect("load workspace");
        let collection = store.create_collection("Baseline", 0).expect("collection");
        for index in 0..2_000 {
            let mut request = RequestSpec::new(format!("Request {index:04}"));
            request.url = format!("https://example.com/items/{index}");
            store
                .save_request(&collection.id, &request)
                .expect("save request");
        }

        let started = std::time::Instant::now();
        let full = store.load().expect("full load");
        let full_elapsed = started.elapsed();

        let cached = WorkspaceStore::with_cache_dir(root.path(), Some(cache.path().to_path_buf()));
        cached.load().expect("build index");
        let started = std::time::Instant::now();
        let cached_load = cached.load().expect("cached load");
        let cached_elapsed = started.elapsed();

        let count = full.collections[0].requests.len();
        assert_eq!(count, 2_000);
        assert_eq!(cached_load.collections[0].requests.len(), 2_000);
        println!(
            "load_baseline: {count} requests, full {} ms, cached {} ms",
            full_elapsed.as_millis(),
            cached_elapsed.as_millis()
        );
    }

    #[test]
    fn request_entry_accessors_read_both_variants() {
        let folder = FolderId::new();
        let mut spec = RequestSpec::new("Loaded");
        spec.method = HttpMethod::Post;
        spec.url = "https://example.com/loaded".into();
        spec.folder_id = Some(folder);
        let loaded = RequestEntry::Loaded(Box::new(spec.clone()));

        assert_eq!(loaded.id(), spec.id);
        assert_eq!(loaded.name(), "Loaded");
        assert_eq!(loaded.method(), HttpMethod::Post);
        assert_eq!(loaded.url(), "https://example.com/loaded");
        assert_eq!(loaded.folder_id(), Some(folder));
        assert_eq!(loaded.spec(), Some(&spec));
        assert!(loaded.clone().spec_mut().is_some());

        let summary = RequestSummary {
            id: RequestId::new(),
            name: "Unloaded".into(),
            method: HttpMethod::Delete,
            url: "https://example.com/unloaded".into(),
            protocol: RequestProtocol::Http,
            folder_id: Some(folder),
        };
        let fingerprint = FileFingerprint {
            file_name: format!("{}.json", summary.id),
            modified_s: 1,
            modified_ns: 2,
            size: 3,
        };
        let mut unloaded = RequestEntry::Unloaded {
            summary: summary.clone(),
            fingerprint,
        };

        assert_eq!(unloaded.id(), summary.id);
        assert_eq!(unloaded.name(), "Unloaded");
        assert_eq!(unloaded.method(), HttpMethod::Delete);
        assert_eq!(unloaded.url(), "https://example.com/unloaded");
        assert_eq!(unloaded.folder_id(), Some(folder));
        assert_eq!(unloaded.spec(), None);
        assert!(unloaded.spec_mut().is_none());

        unloaded.set_folder_id(None);
        assert_eq!(unloaded.folder_id(), None);
        assert!(matches!(unloaded, RequestEntry::Unloaded { .. }));
    }

    #[test]
    fn load_request_reads_one_document() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();

        assert_eq!(
            store.load_request(&collection.id, request.id).unwrap(),
            request
        );
    }

    #[test]
    fn an_index_entry_written_before_protocols_still_loads() {
        let json = serde_json::to_string(&index_entry("Old")).expect("serialize");
        let json = json.replace(r#""protocol":"http","#, "");
        assert!(!json.contains(r#""protocol""#));

        let entry: RequestIndexEntry = serde_json::from_str(&json).expect("old cache loads");
        assert_eq!(entry.protocol, RequestProtocol::Http);
    }

    #[test]
    fn request_mutations_keep_the_websocket_protocol() {
        let (_dir, store) = store();
        store.load().unwrap();
        let mut collection = store.create_collection("Demo", 0).unwrap();
        let folder = collection.add_folder("Sockets", None);
        store.save_collection(&collection).unwrap();

        let mut request = store.create_request(&collection.id, "Socket").unwrap();
        request.protocol = RequestProtocol::WebSocket;
        request.subprotocols = vec!["graphql-ws".to_owned()];
        store.save_request(&collection.id, &request).unwrap();

        // Rename.
        let mut renamed = request.clone();
        renamed.name = "Renamed socket".into();
        store.save_request(&collection.id, &renamed).unwrap();
        let loaded = read_request(&store, &collection.id, request.id);
        assert_eq!(loaded.name, "Renamed socket");
        assert_eq!(loaded.protocol, RequestProtocol::WebSocket);
        assert_eq!(loaded.subprotocols, vec!["graphql-ws".to_owned()]);

        // Move into a folder.
        let mut moved = renamed.clone();
        moved.folder_id = Some(folder);
        store.save_request(&collection.id, &moved).unwrap();
        let loaded = read_request(&store, &collection.id, request.id);
        assert_eq!(loaded.folder_id, Some(folder));
        assert_eq!(loaded.protocol, RequestProtocol::WebSocket);

        // Duplicate as a new document.
        let mut duplicated = moved.clone();
        duplicated.id = RequestId::new();
        duplicated.name = "Renamed socket copy".into();
        store.save_request(&collection.id, &duplicated).unwrap();
        let loaded = read_request(&store, &collection.id, duplicated.id);
        assert_eq!(loaded.protocol, RequestProtocol::WebSocket);
        assert_eq!(loaded.subprotocols, vec!["graphql-ws".to_owned()]);
    }

    #[test]
    fn load_request_reports_a_missing_document() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let missing = RequestId::new();

        let error = store.load_request(&collection.id, missing).unwrap_err();
        assert!(matches!(error, StorageError::InvalidEntry { .. }));
        assert!(error.detail().contains("does not exist"));
        assert_eq!(
            error.path(),
            Some(
                store
                    .request_file(&collection.id, missing)
                    .unwrap()
                    .as_path()
            )
        );
    }

    #[test]
    fn load_request_reports_corrupt_json() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let path = store.request_file(&collection.id, request.id).unwrap();
        fs::write(&path, "not: [valid").unwrap();

        let error = store.load_request(&collection.id, request.id).unwrap_err();
        assert!(matches!(error, StorageError::Parse { .. }));
        assert!(error.detail().contains("invalid JSON"));
        assert_eq!(error.path(), Some(path.as_path()));
    }

    #[test]
    fn load_request_rejects_a_newer_schema() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let path = store.request_file(&collection.id, request.id).unwrap();
        let document = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "id": request.id,
            "name": "Future",
            "method": "GET",
            "url": "",
        });
        fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();

        let error = store.load_request(&collection.id, request.id).unwrap_err();
        assert!(matches!(
            error,
            StorageError::UnsupportedSchema {
                found,
                supported,
                ..
            } if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
        ));
    }

    #[test]
    fn load_request_rejects_a_stem_id_mismatch() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();
        let path = store.request_file(&collection.id, request.id).unwrap();
        let mut other = RequestSpec::new("Other");
        other.id = RequestId::new();
        fs::write(&path, serde_json::to_string(&other).unwrap()).unwrap();

        let error = store.load_request(&collection.id, request.id).unwrap_err();
        assert!(matches!(error, StorageError::InvalidEntry { .. }));
        assert!(error.detail().contains("does not match file name"));
    }

    #[test]
    fn workspace_keys_are_stable_and_isolated() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        assert_eq!(workspace_key(first.path()), workspace_key(first.path()));
        assert_ne!(workspace_key(first.path()), workspace_key(second.path()));
    }

    #[test]
    fn cache_location_is_below_the_platform_cache_directory() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::with_cache_dir(root.path(), Some(cache.path().to_path_buf()));

        let expected = cache
            .path()
            .join(paths::PRODUCT_DIR)
            .join("index")
            .join(workspace_key(root.path()).to_string());
        assert_eq!(store.cache_root(), Some(expected.as_path()));
        assert_eq!(store.index_file("demo"), Some(expected.join("demo.json")));
    }

    #[test]
    fn an_unresolvable_cache_disables_caching_without_failing() {
        let (dir, _store) = store();
        let store = WorkspaceStore::with_cache_dir(dir.path(), None);

        let loaded = store.load().unwrap();
        assert_eq!(loaded.workspace.name, DEFAULT_WORKSPACE_NAME);
        assert!(store.cache_root().is_none());
        assert!(store.index_file("demo").is_none());
    }

    #[test]
    fn index_round_trips_with_sorted_entries() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        let first = index_entry("Zeta");
        let second = index_entry("Alpha");
        let index = RequestIndex::new("demo", vec![first.clone(), second.clone()]);

        store.write_index("demo", &index).unwrap();
        let read = store.read_index("demo").unwrap();

        assert_eq!(read, index);
        assert_eq!(read.entries[0].id, first.id.min(second.id));
    }

    #[test]
    fn index_read_errors_are_discardable() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        let path = store.index_file("demo").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, "not json").unwrap();
        assert!(matches!(
            store.read_index("demo"),
            Err(StorageError::Parse { .. })
        ));

        for version in [INDEX_VERSION - 1, INDEX_VERSION + 1] {
            let document = serde_json::json!({
                "index_version": version,
                "collection_id": "demo",
                "entries": [],
            });
            fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
            let error = store.read_index("demo").unwrap_err();
            assert!(matches!(error, StorageError::InvalidEntry { .. }));
            assert!(error.detail().contains("index_version"));
        }

        let document = serde_json::json!({
            "index_version": INDEX_VERSION,
            "collection_id": "other",
            "entries": [],
        });
        fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
        let error = store.read_index("demo").unwrap_err();
        assert!(matches!(error, StorageError::InvalidEntry { .. }));
        assert!(error.detail().contains("does not match"));
    }

    #[test]
    fn failed_index_write_leaves_no_partial_file() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        let path = store.index_file("demo").unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::create_dir(&path).unwrap();

        let error = store
            .write_index(
                "demo",
                &RequestIndex::new("demo", vec![index_entry("Ping")]),
            )
            .unwrap_err();
        assert!(error.path().is_some());

        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("demo.json")]);
    }

    #[test]
    fn first_load_writes_an_index_and_reads_every_request() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Ping").unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].spec(), Some(&request));
        let index = store.read_index(&collection.id).unwrap();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].id, request.id);
        assert_eq!(index.entries[0].name, "Ping");
        assert!(store.index_file(&collection.id).unwrap().is_file());
    }

    #[test]
    fn unchanged_files_reuse_cached_metadata_without_being_read() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let request = store.create_request(&collection.id, "Cached").unwrap();
        store.load().unwrap();

        // Make the file unreadable as a request while keeping its fingerprint
        // equal to the cached one. A load that opens the file would report a
        // parse issue and drop the row.
        let path = store.request_file(&collection.id, request.id).unwrap();
        fs::write(&path, b"not: [valid").unwrap();
        patch_index_fingerprint(&store, &collection.id, &path);

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].name(), "Cached");
        assert_eq!(loaded.collections[0].requests[0].spec(), None);
    }

    #[test]
    fn cached_metadata_carries_every_query_parameter_for_search() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let mut request = store.create_request(&collection.id, "Items").unwrap();
        request.url = "https://example.com/items".to_owned();
        request.query = vec![
            crate::model::KeyValue::new("page", "2"),
            crate::model::KeyValue::new("sort", "name"),
        ];
        store.save_request(&collection.id, &request).unwrap();

        let loaded = store.load().unwrap();
        let entry = &loaded.collections[0].requests[0];
        assert_eq!(
            entry.search_url(),
            "https://example.com/items?page=2&sort=name"
        );

        // The cached summary is what a later load shows without opening the
        // document, so it has to carry the same composed URL.
        let index = store.read_index(&collection.id).unwrap();
        assert_eq!(
            index.entries[0].url,
            "https://example.com/items?page=2&sort=name"
        );

        let reloaded = store.load().unwrap();
        let entry = &reloaded.collections[0].requests[0];
        assert_eq!(entry.spec(), None, "the document should stay unread");
        assert_eq!(
            entry.search_url(),
            "https://example.com/items?page=2&sort=name"
        );
    }

    #[test]
    fn changed_new_and_deleted_files_reconcile() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let changed = store.create_request(&collection.id, "Before").unwrap();
        let deleted = store.create_request(&collection.id, "Deleted").unwrap();
        store.load().unwrap();

        let mut renamed = changed.clone();
        renamed.name = "After a longer name".into();
        renamed.url = "https://example.com/changed/content".into();
        store.save_request(&collection.id, &renamed).unwrap();
        store.delete_request(&collection.id, deleted.id).unwrap();
        let added = store.create_request(&collection.id, "Added").unwrap();

        let loaded = store.load().unwrap();

        assert!(loaded.issues.is_empty());
        let requests = &loaded.collections[0].requests;
        assert_eq!(requests.len(), 2);
        let names: Vec<_> = requests.iter().map(|request| request.name()).collect();
        assert_eq!(names, ["Added", "After a longer name"]);
        assert_eq!(
            requests
                .iter()
                .find(|request| request.id() == added.id)
                .and_then(|request| request.spec()),
            Some(&added)
        );
        assert_eq!(
            requests
                .iter()
                .find(|request| request.id() == changed.id)
                .and_then(|request| request.spec()),
            Some(&renamed)
        );
        let index = store.read_index(&collection.id).unwrap();
        assert_eq!(index.entries.len(), 2);
        assert!(index.entries.iter().all(|entry| entry.id != deleted.id));
    }

    #[test]
    fn a_corrupt_request_is_reported_while_others_load() {
        let (root, cache, store) = cached_store();
        let _ = (root, cache);
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let good = store.create_request(&collection.id, "Good").unwrap();
        let bad = store.create_request(&collection.id, "Bad").unwrap();
        let bad_path = store.request_file(&collection.id, bad.id).unwrap();
        fs::write(&bad_path, b"not: [valid").unwrap();

        let loaded = store.load().unwrap();

        assert_eq!(loaded.collections[0].requests.len(), 1);
        assert_eq!(loaded.collections[0].requests[0].spec(), Some(&good));
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path == bad_path && issue.message.contains("invalid JSON"))
        );
        let index = store.read_index(&collection.id).unwrap();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].id, good.id);
    }

    #[test]
    fn at_is_cache_free() {
        let (_dir, store) = store();
        assert!(store.cache_root().is_none());
        assert!(store.index_file("demo").is_none());

        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        store.create_request(&collection.id, "Ping").unwrap();
        for path in tree_files(store.root()) {
            assert!(!path.to_string_lossy().contains("index"));
        }
    }

    #[test]
    fn no_index_is_written_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::with_cache_dir(root.path(), Some(cache.path().to_path_buf()));
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        store.create_request(&collection.id, "Ping").unwrap();
        store.load().unwrap();

        // The dotfiles bootstrap writes belong to the workspace by design;
        // every other file is a document.
        let files: Vec<_> = tree_files(store.root())
            .into_iter()
            .filter(|path| {
                !path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        assert_eq!(files.len(), 3);
        assert!(files.iter().all(|path| !path.starts_with(cache.path())));
        assert!(store.index_file(&collection.id).unwrap().is_file());
        assert!(
            !store
                .index_file(&collection.id)
                .unwrap()
                .starts_with(store.root())
        );
    }

    #[test]
    fn full_re_save_skips_unloaded_requests() {
        let (_dir, store) = store();
        store.load().unwrap();
        let collection = store.create_collection("Demo", 0).unwrap();
        let loaded_request = store.create_request(&collection.id, "Loaded").unwrap();
        let unloaded_request = store.create_request(&collection.id, "Unloaded").unwrap();
        let loaded_path = store
            .request_file(&collection.id, loaded_request.id)
            .unwrap();
        let unloaded_path = store
            .request_file(&collection.id, unloaded_request.id)
            .unwrap();
        let unloaded_before = fs::read(&unloaded_path).unwrap();

        let mut rewritten = loaded_request.clone();
        rewritten.name = "Rewritten".into();
        let requests = vec![
            RequestEntry::Loaded(Box::new(rewritten)),
            unloaded_entry(&unloaded_request, &unloaded_path),
        ];
        let mut loaded = LoadedCollection {
            collection: collection.clone(),
            requests,
        };
        store
            .save_all(&Workspace::new("ws"), std::slice::from_ref(&loaded), &[])
            .unwrap();

        assert_eq!(fs::read(&unloaded_path).unwrap(), unloaded_before);
        assert_eq!(
            read_request(&store, &collection.id, loaded_request.id).name,
            "Rewritten"
        );

        loaded.requests[0].spec_mut().unwrap().name = "Again".into();
        assert!(
            store
                .save_collections(std::slice::from_ref(&loaded))
                .is_success()
        );
        assert_eq!(fs::read(&unloaded_path).unwrap(), unloaded_before);
        assert_eq!(
            read_request(&store, &collection.id, loaded_request.id).name,
            "Again"
        );
        assert!(loaded_path.is_file());
    }
}
