//! Loaded workspace state and the mutation operations the sidebar drives.
//!
//! Writes are synchronous: each mutation reports [`SaveStatus::Saved`] or
//! [`SaveStatus::Failed`] before returning. After a failure the next mutation
//! performs a full re-save so the current in-memory state eventually reaches
//! disk, while files that failed to load are never rewritten.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use posthaste_core::interchange::{
    BundleCollection, ImportBundle, ImportFormat, InterchangeError, render_bundle,
};
use posthaste_core::model::{Collection, Folder, FolderId, RequestId, RequestSpec, Workspace};
use posthaste_core::storage::{
    DEFAULT_WORKSPACE_NAME, LoadedCollection, LoadedEnvironment, LoadedWorkspace, RequestEntry,
    SaveReport, StorageError, StorageIssue, WorkspaceStore,
};
use posthaste_core::variables::{VariableCatalog, Variables};

use crate::i18n::{self, Key, MessageKey, counted};

/// Outcome of the most recent autosave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveStatus {
    /// No save has been attempted yet.
    Idle,
    /// The most recent change is on disk.
    Saved,
    /// The most recent change could not be written.
    Failed(String),
}

/// Which stored variable map an edit belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum VariableTarget {
    /// Workspace defaults in `workspace.json`.
    Defaults,
    /// Variables in `environments/<slug>.json`.
    Environment { slug: String },
}

/// Which item an inline rename input edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Collection { slug: String },
    Folder { slug: String, id: FolderId },
    Request { slug: String, id: RequestId },
    Environment { slug: String },
}

/// Which item an inline delete confirmation refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    Collection { slug: String },
    Folder { slug: String, id: FolderId },
    Request { slug: String, id: RequestId },
    Environment { slug: String },
}

/// Which row's `⋮` action menu is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowMenuTarget {
    Collection { slug: String },
    Folder { slug: String, id: FolderId },
    Request { slug: String, id: RequestId },
    Environment { slug: String },
}

/// The item whose move destination is being chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveTarget {
    Request { slug: String, id: RequestId },
    Folder { slug: String, id: FolderId },
}

impl MoveTarget {
    /// The collection the item belongs to.
    pub fn slug(&self) -> &str {
        match self {
            Self::Request { slug, .. } | Self::Folder { slug, .. } => slug,
        }
    }
}

/// Where a move picker sends its item: a collection and a folder in it.
///
/// The collection is part of the destination because the picker offers every
/// collection, not only the item's own: a move across collections writes the
/// item into the target collection and removes it from the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveDestination {
    pub slug: String,
    pub folder: Option<FolderId>,
}

impl MoveDestination {
    pub fn new(slug: impl Into<String>, folder: Option<FolderId>) -> Self {
        Self {
            slug: slug.into(),
            folder,
        }
    }
}

/// What one import created, and which items it could not keep as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub collections: usize,
    pub folders: usize,
    pub requests: usize,
    pub environments: usize,
    /// Items that were adjusted, skipped, or could not be written, with the
    /// reason shown in the report.
    pub skipped: Vec<ImportSkip>,
    /// Set when the import could not run at all; nothing was created then.
    pub error: Option<String>,
    /// The format the sniffer recognized, named in the report.
    pub format: Option<ImportFormat>,
}

impl ImportOutcome {
    /// Whether the import created nothing.
    pub fn is_empty(&self) -> bool {
        self.collections == 0 && self.requests == 0 && self.environments == 0
    }
}

/// One item an import could not keep exactly as the document wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSkip {
    pub item: String,
    pub reason: String,
}

/// The loaded workspace plus persistence state.
pub struct WorkspaceState {
    /// `None` when the workspace could not be resolved or loaded; mutations are
    /// disabled in that case.
    pub store: Option<WorkspaceStore>,
    pub workspace: Workspace,
    pub collections: Vec<LoadedCollection>,
    /// Environments ordered by display name, then slug.
    pub environments: Vec<LoadedEnvironment>,
    pub issues: Vec<StorageIssue>,
    pub fatal_error: Option<String>,
    /// The explicit startup directory a failed load could initialize, when it
    /// is missing or holds no `workspace.json`. The welcome screen offers to
    /// create the workspace here; a plain storage failure leaves it `None`.
    pub creatable_root: Option<PathBuf>,
    pub save_status: SaveStatus,
    /// A write failed; the next mutation saves every loaded entry.
    pub needs_resync: bool,
    /// `false` when `workspace.json` itself failed to load and must not be
    /// rewritten by a re-save.
    pub workspace_file_ok: bool,
    /// Validation message for an empty rename.
    pub rename_error: Option<String>,
    /// The item whose name is being edited inline.
    pub rename_target: Option<RenameTarget>,
    /// The item awaiting inline delete confirmation.
    pub delete_target: Option<DeleteTarget>,
    /// Paths of issues that describe failed writes rather than failed loads.
    /// A later successful save clears exactly these.
    save_issue_paths: Vec<PathBuf>,
    /// The preserved damaged document whose deletion is awaiting confirmation.
    pending_issue_delete: Option<StorageIssue>,
}

impl WorkspaceState {
    /// Loads `store`, keeping the app usable when loading fails fatally.
    pub fn load(store: WorkspaceStore) -> Self {
        match store.load() {
            Ok(loaded) => Self::loaded(store, loaded),
            Err(error) => Self::unavailable(error.to_string()),
        }
    }

    /// Loads a workspace the user named explicitly, without creating anything.
    ///
    /// A missing directory or one without `workspace.json` fails like any other
    /// storage error, but records the directory as creatable so the welcome
    /// screen can offer to initialize it.
    pub fn load_existing(store: WorkspaceStore) -> Self {
        match store.load_existing() {
            Ok(loaded) => Self::loaded(store, loaded),
            Err(error) => {
                let creatable_root = store
                    .is_bootstrappable()
                    .then(|| store.root().to_path_buf());
                Self::unavailable_at(error.to_string(), creatable_root)
            }
        }
    }

    fn loaded(store: WorkspaceStore, loaded: LoadedWorkspace) -> Self {
        let workspace_file_ok = !loaded
            .issues
            .iter()
            .any(|issue| issue.path == store.workspace_file());
        Self {
            store: Some(store),
            workspace: loaded.workspace,
            collections: loaded.collections,
            environments: loaded.environments,
            issues: loaded.issues,
            fatal_error: None,
            creatable_root: None,
            save_status: SaveStatus::Idle,
            needs_resync: false,
            workspace_file_ok,
            rename_error: None,
            rename_target: None,
            delete_target: None,
            save_issue_paths: Vec::new(),
            pending_issue_delete: None,
        }
    }

    /// An empty, read-only workspace state used when storage is unavailable.
    pub fn unavailable(message: String) -> Self {
        Self::unavailable_at(message, None)
    }

    fn unavailable_at(message: String, creatable_root: Option<PathBuf>) -> Self {
        Self {
            store: None,
            workspace: Workspace::new(DEFAULT_WORKSPACE_NAME),
            collections: Vec::new(),
            environments: Vec::new(),
            issues: Vec::new(),
            fatal_error: Some(message),
            creatable_root,
            save_status: SaveStatus::Idle,
            needs_resync: false,
            workspace_file_ok: false,
            rename_error: None,
            rename_target: None,
            delete_target: None,
            save_issue_paths: Vec::new(),
            pending_issue_delete: None,
        }
    }

    pub fn collection(&self, slug: &str) -> Option<&LoadedCollection> {
        self.collections
            .iter()
            .find(|entry| entry.collection.id == slug)
    }

    fn collection_mut(&mut self, slug: &str) -> Option<&mut LoadedCollection> {
        self.collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
    }

    /// The request entry with `id` inside `slug`, loaded or not.
    pub fn request_entry(&self, slug: &str, id: RequestId) -> Option<&RequestEntry> {
        self.collection(slug)?
            .requests
            .iter()
            .find(|request| request.id() == id)
    }

    /// The fully loaded request with `id` inside `slug`.
    ///
    /// Returns `None` for an unloaded entry; callers that need the document
    /// call [`Self::ensure_request_loaded`] first.
    pub fn request(&self, slug: &str, id: RequestId) -> Option<&RequestSpec> {
        self.request_entry(slug, id)?.spec()
    }

    /// The folder entry with `id` inside `slug`.
    pub fn folder(&self, slug: &str, id: FolderId) -> Option<&Folder> {
        self.collection(slug)?.folder(id)
    }

    /// Finds a request entry by identifier anywhere in the workspace.
    pub fn find_request(&self, id: RequestId) -> Option<(&str, &RequestEntry)> {
        self.collections.iter().find_map(|entry| {
            entry
                .requests
                .iter()
                .find(|request| request.id() == id)
                .map(|request| (entry.collection.id.as_str(), request))
        })
    }

    /// Reads `id` from disk unless it is already loaded, caching the document
    /// for the rest of the session.
    ///
    /// On failure the entry is removed, the problem is recorded as a storage
    /// issue, and the error is returned so the caller can clear a selection
    /// that referred to it. Other requests remain usable.
    pub fn ensure_request_loaded(&mut self, slug: &str, id: RequestId) -> Result<(), StorageError> {
        let already_loaded = match self.request_entry(slug, id) {
            Some(entry) => entry.spec().is_some(),
            None => false,
        };
        if already_loaded {
            return Ok(());
        }
        let Some(store) = self.store.clone() else {
            let error = StorageError::InvalidEntry {
                path: self
                    .store
                    .as_ref()
                    .and_then(|store| store.request_file(slug, id).ok())
                    .unwrap_or_default(),
                reason: format!(
                    "request {id} cannot be loaded because the workspace is unavailable"
                ),
            };
            self.record_load_failure(slug, id, &error);
            return Err(error);
        };
        match store.load_request(slug, id) {
            Ok(spec) => {
                if let Some(entry) = self.collection_mut(slug)
                    && let Some(request) =
                        entry.requests.iter_mut().find(|request| request.id() == id)
                {
                    *request = RequestEntry::Loaded(Box::new(spec));
                    sort_requests(&mut entry.requests);
                }
                Ok(())
            }
            Err(error) => {
                self.record_load_failure(slug, id, &error);
                Err(error)
            }
        }
    }

    fn record_load_failure(&mut self, slug: &str, id: RequestId, error: &StorageError) {
        match error.path() {
            Some(path) => self.issues.push(StorageIssue {
                path: path.to_path_buf(),
                message: error.detail(),
            }),
            None => {
                if let Some(store) = &self.store {
                    self.issues.push(StorageIssue {
                        path: store.root().to_path_buf(),
                        message: error.detail(),
                    });
                }
            }
        }
        if let Some(entry) = self.collection_mut(slug) {
            entry.requests.retain(|request| request.id() != id);
        }
        if let Some(store) = self.store.clone()
            && let Some(entry) = self.collection(slug)
        {
            store.update_collection_index(slug, &entry.requests);
            store.flush_indexes();
        }
    }

    /// The loaded environment whose file slug is `slug`.
    pub fn environment(&self, slug: &str) -> Option<&LoadedEnvironment> {
        self.environments.iter().find(|loaded| loaded.slug == slug)
    }

    /// Workspace defaults overridden by the active environment, when it loads.
    pub fn variables_for(&self, active_environment: Option<&str>) -> Variables {
        let environment = active_environment
            .and_then(|slug| self.environment(slug))
            .map(|loaded| &loaded.environment.vars);
        Variables::defaults_then(&self.workspace.vars, environment)
    }

    /// The names available to completion, built from the same two maps as
    /// [`Self::variables_for`].
    pub fn variable_catalog(&self, active_environment: Option<&str>) -> VariableCatalog {
        let environment = active_environment
            .and_then(|slug| self.environment(slug))
            .map(|loaded| &loaded.environment.vars);
        VariableCatalog::from_scopes(&self.workspace.vars, environment)
    }

    /// Replaces one stored variable map in memory without writing it.
    pub fn set_variables(&mut self, target: &VariableTarget, vars: BTreeMap<String, String>) {
        match target {
            VariableTarget::Defaults => self.workspace.vars = vars,
            VariableTarget::Environment { slug } => {
                if let Some(loaded) = self
                    .environments
                    .iter_mut()
                    .find(|loaded| loaded.slug == *slug)
                {
                    loaded.environment.vars = vars;
                }
            }
        }
    }

    /// Writes the currently stored values for `target`.
    pub fn save_variables(&mut self, target: &VariableTarget) {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return;
        };
        let result = match target {
            VariableTarget::Defaults => self.persist_workspace(&store),
            VariableTarget::Environment { slug } => {
                let environment = self
                    .environment(slug)
                    .map(|loaded| loaded.environment.clone());
                match environment {
                    Some(environment) if !self.needs_resync => {
                        store.save_environment(slug, &environment)
                    }
                    _ => self.resave(&store),
                }
            }
        };
        self.record_save(result);
    }

    fn next_order(&self) -> u32 {
        self.collections
            .iter()
            .map(|entry| entry.collection.order)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// The first unused default collection display name: `New Collection`,
    /// `New Collection 2`, `New Collection 3`, ...
    pub fn default_collection_name(&self) -> String {
        free_default_name(
            "New Collection",
            self.collections
                .iter()
                .map(|entry| entry.collection.name.as_str()),
        )
    }

    /// The first unused default environment display name: `New Environment`,
    /// `New Environment 2`, `New Environment 3`, ...
    pub fn default_environment_name(&self) -> String {
        free_default_name(
            "New Environment",
            self.environments
                .iter()
                .map(|loaded| loaded.environment.name.as_str()),
        )
    }

    /// The first unused default folder name among the children of `parent_id`
    /// (`None` is the collection root): `New Folder`, `New Folder 2`, ...
    pub fn default_folder_name(&self, slug: &str, parent_id: Option<FolderId>) -> String {
        match self.collection(slug) {
            Some(entry) => free_default_name(
                "New Folder",
                entry
                    .folders()
                    .iter()
                    .filter(|folder| folder.parent_id == parent_id)
                    .map(|folder| folder.name.as_str()),
            ),
            None => "New Folder".to_owned(),
        }
    }

    /// Whether a collection other than `except_slug` already uses `name`.
    fn collection_name_taken(&self, name: &str, except_slug: Option<&str>) -> bool {
        self.collections.iter().any(|entry| {
            Some(entry.collection.id.as_str()) != except_slug
                && same_display_name(&entry.collection.name, name)
        })
    }

    /// Whether an environment other than `except_slug` already uses `name`.
    fn environment_name_taken(&self, name: &str, except_slug: Option<&str>) -> bool {
        self.environments.iter().any(|loaded| {
            Some(loaded.slug.as_str()) != except_slug
                && same_display_name(&loaded.environment.name, name)
        })
    }

    /// Creates and persists a collection, then returns it.
    ///
    /// A name already used by another collection is rejected without writing
    /// anything. Callers that create with a default name first choose a free
    /// one with [`Self::default_collection_name`].
    pub fn create_collection(&mut self, name: &str) -> Option<Collection> {
        let name = name.trim();
        if self.collection_name_taken(name, None) {
            return None;
        }
        let store = self.store.clone()?;
        let collection = match store.create_collection(name, self.next_order()) {
            Ok(collection) => collection,
            Err(error) => {
                self.record_save(Err(error));
                return None;
            }
        };
        self.collections.push(LoadedCollection {
            collection: collection.clone(),
            requests: Vec::new(),
        });
        self.sort_collections();
        self.finish_mutation(&store);
        Some(collection)
    }

    /// Creates and persists a default request in `slug`.
    #[cfg(test)]
    pub fn create_request(&mut self, slug: &str, name: &str) -> Option<RequestSpec> {
        self.create_request_in_folder(slug, name, None)
    }

    /// Creates and persists a default request in `slug`, targeting `folder_id`
    /// or the collection root. A missing destination folder is rejected.
    pub fn create_request_in_folder(
        &mut self,
        slug: &str,
        name: &str,
        folder_id: Option<FolderId>,
    ) -> Option<RequestSpec> {
        self.collection(slug)?;
        if folder_id.is_some_and(|id| self.folder(slug, id).is_none()) {
            return None;
        }
        let store = self.store.clone()?;
        let request = match store.create_request_in(slug, name, folder_id) {
            Ok(request) => request,
            Err(error) => {
                self.record_save(Err(error));
                return None;
            }
        };
        let entry = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)?;
        entry
            .requests
            .push(RequestEntry::Loaded(Box::new(request.clone())));
        sort_requests(&mut entry.requests);
        self.sync_index(&store, slug);
        self.finish_mutation(&store);
        Some(request)
    }

    /// Saves `spec` as a request in `slug`, at the collection root or inside
    /// `folder_id`, keeping the spec's identifier.
    ///
    /// Only the new request document is written; the collection document and
    /// other requests are untouched. A missing collection or folder is
    /// rejected with a visible save error and no write. A failed write leaves
    /// the loaded requests unchanged.
    pub fn save_new_request(
        &mut self,
        slug: &str,
        folder_id: Option<FolderId>,
        spec: &RequestSpec,
    ) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        if self.collection(slug).is_none() {
            self.save_status =
                SaveStatus::Failed(i18n::msg(MessageKey::CollectionNotLoaded, &[&slug]));
            return false;
        }
        if folder_id.is_some_and(|id| self.folder(slug, id).is_none()) {
            self.save_status = SaveStatus::Failed(i18n::t(Key::SaveFolderMissing).to_owned());
            return false;
        }

        let mut request = spec.clone();
        request.folder_id = folder_id;

        // After an earlier failure the next mutation re-saves everything, so
        // the new request must join the in-memory list before that write and
        // must leave it again if the retry fails.
        if self.needs_resync {
            self.upsert_request(slug, request.clone());
            match self.resave(&store) {
                Ok(report) => {
                    self.sync_index(&store, slug);
                    self.record_save(Ok(report));
                    true
                }
                Err(error) => {
                    self.drop_request(slug, request.id);
                    self.record_save(Err(error));
                    false
                }
            }
        } else {
            match store.save_request(slug, &request) {
                Ok(report) => {
                    self.issues.extend(report.into_issues());
                    self.upsert_request(slug, request);
                    self.sync_index(&store, slug);
                    self.finish_mutation(&store);
                    true
                }
                Err(error) => {
                    self.record_save(Err(error));
                    false
                }
            }
        }
    }

    /// Duplicates a saved request into the same folder with a fresh identifier
    /// and a generated `<name> copy` name.
    ///
    /// The copy is read from the loaded workspace; a request that is not part
    /// of it is rejected without writing. Only the copy's request document is
    /// written, so the source file and the collection document are untouched.
    pub fn duplicate_request(&mut self, slug: &str, id: RequestId) -> Option<RequestSpec> {
        self.duplicate_request_with_draft(slug, id, None)
    }

    /// Duplicates a request, using `draft` as the source values when the tab
    /// holds unwritten edits and autosave is disabled.
    ///
    /// The source's identifier, folder, and stored name still decide the new
    /// identifier, destination, and generated copy name. `draft` only supplies
    /// the editable fields, so a failed or skipped source write is not needed
    /// to copy the live draft.
    pub fn duplicate_request_with_draft(
        &mut self,
        slug: &str,
        id: RequestId,
        draft: Option<&RequestSpec>,
    ) -> Option<RequestSpec> {
        let (folder_id, source_name, occupied) = {
            let entry = self.collection(slug)?;
            let source = entry.requests.iter().find(|request| request.id() == id)?;
            let occupied: Vec<String> = entry
                .folder_requests(source.folder_id())
                .into_iter()
                .filter(|request| request.id() != id)
                .map(|request| request.name().to_owned())
                .collect();
            (source.folder_id(), source.name().to_owned(), occupied)
        };
        let name = request_copy_name(&source_name, occupied.iter().map(String::as_str));

        let use_memory = !matches!(draft, Some(draft) if draft.id == id);
        if use_memory {
            self.ensure_request_loaded(slug, id).ok()?;
        }
        let mut request = match draft {
            Some(draft) if draft.id == id => (*draft).clone(),
            _ => self.request(slug, id)?.clone(),
        };
        request.id = RequestId::new();
        request.folder_id = folder_id;
        request.name = name;

        if self.save_new_request(slug, folder_id, &request) {
            Some(request)
        } else {
            None
        }
    }

    /// Duplicates a folder and its whole subtree as a sibling of the source.
    ///
    /// Every copied folder gets a fresh identifier with its parent remapped
    /// inside the subtree. The copied root is named `<name> copy`, `<name>
    /// copy 2`, ... among the destination siblings; descendant folder and
    /// request names are preserved. Every request in the subtree is copied
    /// with a fresh identifier and points at its copied folder.
    ///
    /// The collection document is written first and is the commit point. A
    /// failed collection write changes nothing. A failed request write keeps
    /// the copied folder and the requests written before it (and after it),
    /// omits the failed request, records an issue, and leaves the workspace
    /// needing a resync.
    pub fn duplicate_folder(&mut self, slug: &str, id: FolderId) -> Option<Folder> {
        self.duplicate_folder_with_drafts(slug, id, &BTreeMap::new())
    }

    /// Duplicates a folder subtree, using `drafts` for any source request that
    /// has live, unwritten values in a tab.
    pub fn duplicate_folder_with_drafts(
        &mut self,
        slug: &str,
        id: FolderId,
        drafts: &BTreeMap<RequestId, RequestSpec>,
    ) -> Option<Folder> {
        self.duplicate_folder_with_writer(slug, id, drafts, |store, slug, request| {
            store.save_request(slug, request)
        })
    }

    /// The injectable form of [`Self::duplicate_folder`]. `write_request` is
    /// called once per copied request after the collection commit, so tests
    /// can make a single write fail.
    fn duplicate_folder_with_writer<F>(
        &mut self,
        slug: &str,
        id: FolderId,
        drafts: &BTreeMap<RequestId, RequestSpec>,
        mut write_request: F,
    ) -> Option<Folder>
    where
        F: FnMut(&WorkspaceStore, &str, &RequestSpec) -> Result<SaveReport, StorageError>,
    {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return None;
        };

        // Copying needs every source document, including entries that the
        // startup scan left unloaded.
        let source_ids: Vec<RequestId> = {
            let entry = self.collection(slug)?;
            let subtree: HashSet<FolderId> = entry.folder_subtree_ids(id).into_iter().collect();
            if subtree.is_empty() {
                return None;
            }
            entry
                .requests
                .iter()
                .filter(|request| {
                    request
                        .folder_id()
                        .is_some_and(|folder_id| subtree.contains(&folder_id))
                })
                .map(|request| request.id())
                .collect()
        };
        for request_id in source_ids {
            self.ensure_request_loaded(slug, request_id).ok()?;
        }

        // Build the complete replacement before touching disk or memory.
        let (updated, copied_requests, root_copy) = {
            let entry = self.collection(slug)?;
            let source = entry.folder(id)?;
            let subtree = entry.folder_subtree_ids(id);
            if subtree.is_empty() {
                return None;
            }
            let subtree_set: HashSet<FolderId> = subtree.iter().copied().collect();
            let sibling_names: Vec<&str> = entry
                .folder_children(source.parent_id)
                .into_iter()
                .filter(|folder| folder.id != id)
                .map(|folder| folder.name.as_str())
                .collect();
            let root_name = folder_copy_name(&source.name, sibling_names);

            let mut mapping: HashMap<FolderId, FolderId> = HashMap::new();
            let mut copied_folders = Vec::with_capacity(subtree.len());
            for folder_id in &subtree {
                let source_folder = entry.folder(*folder_id)?;
                let new_id = FolderId::new();
                mapping.insert(*folder_id, new_id);
                let parent_id = if *folder_id == id {
                    source.parent_id
                } else {
                    source_folder.parent_id.map(|parent| {
                        *mapping
                            .get(&parent)
                            .expect("a subtree parent is copied before its children")
                    })
                };
                copied_folders.push(Folder {
                    id: new_id,
                    name: if *folder_id == id {
                        root_name.clone()
                    } else {
                        source_folder.name.clone()
                    },
                    parent_id,
                });
            }

            let copied_requests: Vec<RequestSpec> = entry
                .requests
                .iter()
                .filter(|request| {
                    request
                        .folder_id()
                        .is_some_and(|folder_id| subtree_set.contains(&folder_id))
                })
                .map(|request| {
                    let source_folder = request.folder_id().expect("request was filtered");
                    let mut copy = match drafts.get(&request.id()) {
                        Some(draft) => draft.clone(),
                        None => request
                            .spec()
                            .expect("the source request was loaded above")
                            .clone(),
                    };
                    copy.id = RequestId::new();
                    copy.name = request.name().to_owned();
                    copy.folder_id = Some(
                        *mapping
                            .get(&source_folder)
                            .expect("a subtree folder is mapped"),
                    );
                    copy
                })
                .collect();

            let mut updated = entry.collection.clone();
            updated.folders.extend(copied_folders.iter().cloned());
            let root_copy = copied_folders.first().cloned()?;
            (updated, copied_requests, root_copy)
        };

        // The collection document is the commit point: no request file is
        // written until the copied folder entries are on disk.
        match store.save_collection(&updated) {
            Ok(report) => self.issues.extend(report.into_issues()),
            Err(error) => {
                self.record_save(Err(error));
                return None;
            }
        }

        // The collection is committed, so the copied folders become visible
        // even when a later request write fails.
        if let Some(entry) = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
        {
            entry.collection = updated;
        }

        let mut written = Vec::with_capacity(copied_requests.len());
        let mut failed = false;
        for request in copied_requests {
            match write_request(&store, slug, &request) {
                Ok(report) => {
                    self.issues.extend(report.into_issues());
                    written.push(request);
                }
                Err(error) => {
                    failed = true;
                    self.issues.push(StorageIssue {
                        path: error.path().unwrap_or_else(|| store.root()).to_path_buf(),
                        message: i18n::msg(
                            MessageKey::StorageRequestCopyFailed,
                            &[&error.detail()],
                        ),
                    });
                }
            }
        }
        if let Some(entry) = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
        {
            entry.requests.extend(
                written
                    .into_iter()
                    .map(|request| RequestEntry::Loaded(Box::new(request))),
            );
            sort_requests(&mut entry.requests);
        }

        self.sync_index(&store, slug);
        if failed {
            store.flush_indexes();
            self.save_status = SaveStatus::Failed(i18n::t(Key::SaveRequestsCopyFailed).to_owned());
            self.needs_resync = true;
        } else {
            self.finish_mutation(&store);
        }
        Some(root_copy)
    }

    /// Inserts or replaces one request in a loaded collection, keeping the
    /// collection's name order.
    fn upsert_request(&mut self, slug: &str, request: RequestSpec) {
        let Some(entry) = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
        else {
            return;
        };
        match entry
            .requests
            .iter_mut()
            .find(|saved| saved.id() == request.id)
        {
            Some(saved) => *saved = RequestEntry::Loaded(Box::new(request)),
            None => entry.requests.push(RequestEntry::Loaded(Box::new(request))),
        }
        sort_requests(&mut entry.requests);
    }

    /// Removes one request from a loaded collection after a failed save.
    fn drop_request(&mut self, slug: &str, id: RequestId) {
        if let Some(entry) = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
        {
            entry.requests.retain(|request| request.id() != id);
        }
    }

    /// Creates a folder under `parent_id` (or the collection root) and persists
    /// the collection document. A missing collection or parent is rejected.
    /// The in-memory entry stays visible when the write fails.
    pub fn create_folder(
        &mut self,
        slug: &str,
        parent_id: Option<FolderId>,
        name: &str,
    ) -> Option<Folder> {
        let store = self.store.clone()?;
        let folder = {
            let entry = self
                .collections
                .iter_mut()
                .find(|entry| entry.collection.id == slug)?;
            if parent_id.is_some_and(|id| entry.folder(id).is_none()) {
                return None;
            }
            let name = name.trim();
            let name = if name.is_empty() { "New Folder" } else { name };
            if folder_name_taken(&entry.collection, parent_id, name, None) {
                return None;
            }
            let id = entry.collection.add_folder(name, parent_id);
            entry.collection.folder(id).cloned()
        };
        let result = match self.collection(slug) {
            Some(entry) if !self.needs_resync => store.save_collection(&entry.collection),
            _ => self.resave(&store),
        };
        self.record_save(result);
        folder
    }

    /// Creates and persists an environment, then returns the loaded entry.
    ///
    /// A name already used by another environment is rejected without writing
    /// anything. Callers that create with a default name first choose a free
    /// one with [`Self::default_environment_name`].
    pub fn create_environment(&mut self, name: &str) -> Option<LoadedEnvironment> {
        let name = name.trim();
        if self.environment_name_taken(name, None) {
            return None;
        }
        let store = self.store.clone()?;
        let (slug, environment) = match store.create_environment(name) {
            Ok(created) => created,
            Err(error) => {
                self.record_save(Err(error));
                return None;
            }
        };
        let loaded = LoadedEnvironment { slug, environment };
        self.environments.push(loaded.clone());
        self.sort_environments();
        self.finish_mutation(&store);
        Some(loaded)
    }

    /// Renames a collection after trimming, moving its directory when the slug
    /// changes.
    ///
    /// Returns the resulting slug, or `None` when the name was rejected or the
    /// collection is not loaded. The new display name stays in memory even when
    /// the write fails. A failed directory move keeps the old slug and reports
    /// an issue; a failed save after a successful move is rolled back when
    /// possible so the stored id and directory name agree again.
    pub fn rename_collection(&mut self, slug: &str, name: &str) -> Option<String> {
        let name = validated_name(name, &mut self.rename_error)?;
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return None;
        };
        let current_name = self.collection(slug)?.collection.name.clone();
        // Confirming the current name is always accepted, even when the
        // workspace already contains another collection with that name.
        if !same_display_name(&current_name, name) && self.collection_name_taken(name, Some(slug)) {
            self.rename_error = Some(i18n::t(Key::RenameCollectionTaken).to_owned());
            return None;
        }

        let target = match store.collection_slug_for(name, slug) {
            Ok(target) => target,
            Err(error) => {
                self.push_storage_issue(&store, &error, MessageKey::StorageCollectionNameFailed);
                slug.to_owned()
            }
        };

        let entry = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
            .expect("collection checked above");
        entry.collection.name = name.to_owned();
        self.sort_collections();

        if target != slug {
            match store.move_collection(slug, &target) {
                Ok(()) => {}
                Err(error) => {
                    self.push_storage_issue(
                        &store,
                        &error,
                        MessageKey::StorageCollectionMoveFailed,
                    );
                    let result = self.save_collection_document(&store, slug);
                    self.record_save(result);
                    return Some(slug.to_owned());
                }
            }
            if let Some(entry) = self
                .collections
                .iter_mut()
                .find(|entry| entry.collection.id == slug)
            {
                entry.collection.id = target.clone();
            }
            self.sort_collections();
        }

        let result = self.save_collection_document(&store, &target);
        let mut resulting = target.clone();
        if result.is_err() && target != slug {
            match store.move_collection(&target, slug) {
                Ok(()) => {
                    if let Some(entry) = self
                        .collections
                        .iter_mut()
                        .find(|entry| entry.collection.id == target)
                    {
                        entry.collection.id = slug.to_owned();
                    }
                    self.sort_collections();
                    resulting = slug.to_owned();
                }
                Err(error) => {
                    // The directory stays at the new name; the next successful
                    // save rewrites the document with the matching id.
                    self.push_storage_issue(
                        &store,
                        &error,
                        MessageKey::StorageCollectionMoveBackFailed,
                    );
                }
            }
        }
        if resulting != slug {
            store.rename_index(slug, &resulting);
        }
        self.record_save(result);
        Some(resulting)
    }

    /// Renames a folder after trimming. Returns `true` when the in-memory name
    /// was changed, even if the save failed.
    pub fn rename_folder(&mut self, slug: &str, id: FolderId, name: &str) -> bool {
        let name = match validated_name(name, &mut self.rename_error) {
            Some(name) => name,
            None => return false,
        };
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        let duplicate = {
            let Some(entry) = self
                .collections
                .iter()
                .find(|entry| entry.collection.id == slug)
            else {
                return false;
            };
            let Some(folder) = entry.folder(id) else {
                return false;
            };
            // Confirming the current name is accepted, even when the loaded
            // collection already contains another same-named sibling.
            !same_display_name(&folder.name, name)
                && folder_name_taken(&entry.collection, folder.parent_id, name, Some(id))
        };
        if duplicate {
            self.rename_error = Some(i18n::t(Key::RenameFolderTaken).to_owned());
            return false;
        }
        {
            let Some(entry) = self
                .collections
                .iter_mut()
                .find(|entry| entry.collection.id == slug)
            else {
                return false;
            };
            if !entry.collection.rename_folder(id, name) {
                return false;
            }
        }
        let result = match self.collection(slug) {
            Some(entry) if !self.needs_resync => store.save_collection(&entry.collection),
            _ => self.resave(&store),
        };
        self.record_save(result);
        true
    }

    /// Cascade-deletes a folder and everything below it. The collection
    /// document is written first, so a failed write leaves memory and every
    /// request file untouched. A request file that cannot be deleted is
    /// re-rooted at the collection root and reported.
    pub fn delete_folder(&mut self, slug: &str, id: FolderId) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        let Some(entry) = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
        else {
            return false;
        };
        let subtree = entry.folder_subtree_ids(id);
        if subtree.is_empty() {
            return false;
        }
        match store.delete_folder_subtree(entry, &subtree) {
            Ok(issues) => {
                let partial = !issues.is_empty();
                self.issues.extend(issues);
                self.sync_index(&store, slug);
                if partial {
                    store.flush_indexes();
                    self.save_status =
                        SaveStatus::Failed(i18n::t(Key::SaveRequestsDeleteFailed).to_owned());
                    self.needs_resync = true;
                } else {
                    self.finish_mutation(&store);
                }
                true
            }
            Err(error) => {
                self.record_save(Err(error));
                false
            }
        }
    }

    /// Moves a request into `parent_id` (or the collection root). A missing
    /// request, collection, or destination folder is rejected. Moving to the
    /// current parent is a no-op that writes nothing and returns `true`.
    #[cfg(test)]
    pub fn move_request(&mut self, slug: &str, id: RequestId, parent_id: Option<FolderId>) -> bool {
        self.move_request_to(slug, id, slug, parent_id)
    }

    /// Moves a request into `to_slug`'s `parent_id` (or that collection's
    /// root). A missing request, collection, or destination folder is
    /// rejected. Moving to the current parent is a no-op that writes nothing
    /// and returns `true`.
    ///
    /// A cross-collection move writes the request under the target collection
    /// before removing the source document, so a failed write leaves the
    /// original in place.
    pub fn move_request_to(
        &mut self,
        from_slug: &str,
        id: RequestId,
        to_slug: &str,
        parent_id: Option<FolderId>,
    ) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        let same_collection = from_slug == to_slug;
        {
            let Some(entry) = self.collection(from_slug) else {
                return false;
            };
            let Some(request) = entry.requests.iter().find(|request| request.id() == id) else {
                return false;
            };
            let Some(target) = self.collection(to_slug) else {
                return false;
            };
            if parent_id.is_some_and(|id| target.folder(id).is_none()) {
                return false;
            }
            if same_collection && request.folder_id() == parent_id {
                return true;
            }
        }
        if !same_collection {
            return self.move_request_across(from_slug, id, to_slug, parent_id, &store);
        }
        // Moving rewrites the document, so its full contents must be present.
        if self.ensure_request_loaded(from_slug, id).is_err() {
            return false;
        }
        let moved = {
            let Some(entry) = self.collection_mut(from_slug) else {
                return false;
            };
            let Some(request) = entry.requests.iter_mut().find(|request| request.id() == id) else {
                return false;
            };
            let Some(spec) = request.spec_mut() else {
                return false;
            };
            spec.folder_id = parent_id;
            Some(spec.clone())
        };
        let Some(request) = moved else {
            return false;
        };
        let result = if self.needs_resync {
            self.resave(&store)
        } else {
            store.save_request(from_slug, &request)
        };
        if result.is_ok() {
            self.sync_index(&store, from_slug);
        }
        self.record_save(result);
        true
    }

    /// The cross-collection half of [`Self::move_request_to`]. Both documents
    /// are settled before memory changes; a failed delete removes the target
    /// copy so the request is never left in two collections.
    fn move_request_across(
        &mut self,
        from_slug: &str,
        id: RequestId,
        to_slug: &str,
        parent_id: Option<FolderId>,
        store: &WorkspaceStore,
    ) -> bool {
        // The document is rewritten under the target, so its full contents
        // must be present.
        if self.ensure_request_loaded(from_slug, id).is_err() {
            return false;
        }
        let mut request = {
            let Some(entry) = self.collection(from_slug) else {
                return false;
            };
            let Some(spec) = entry
                .requests
                .iter()
                .find(|request| request.id() == id)
                .and_then(|request| request.spec())
            else {
                return false;
            };
            spec.clone()
        };
        request.folder_id = parent_id;
        if !self.needs_resync {
            match store.save_request(to_slug, &request) {
                Ok(report) => self.issues.extend(report.into_issues()),
                Err(error) => {
                    self.record_save(Err(error));
                    return false;
                }
            }
        }
        if let Err(error) = store.delete_request(from_slug, id) {
            let _ = store.delete_request(to_slug, id);
            self.record_save(Err(error));
            return false;
        }
        // Both collections now agree with disk, so the move is committed in
        // memory.
        self.drop_request(from_slug, id);
        self.upsert_request(to_slug, request);
        self.sync_index(store, from_slug);
        self.sync_index(store, to_slug);
        if self.needs_resync {
            self.finish_mutation(store);
        } else {
            self.record_save(Ok(SaveReport::default()));
        }
        true
    }

    /// Moves a folder under `parent_id` (or the collection root). A missing
    /// folder or destination, and any destination inside the moved folder's
    /// own subtree, is rejected. Moving to the current parent is a no-op that
    /// writes nothing and returns `true`.
    #[cfg(test)]
    pub fn move_folder(&mut self, slug: &str, id: FolderId, parent_id: Option<FolderId>) -> bool {
        self.move_folder_to(slug, id, slug, parent_id)
    }

    /// Moves a folder under `to_slug`'s `parent_id` (or that collection's
    /// root), carrying its whole subtree. A missing folder, collection, or
    /// destination, and any same-collection destination inside the moved
    /// folder's own subtree, is rejected. Moving to the current parent is a
    /// no-op that writes nothing and returns `true`.
    ///
    /// A cross-collection move writes the folder entries and every request
    /// document under the target collection before removing them from the
    /// source, so a failed write leaves the original in place.
    pub fn move_folder_to(
        &mut self,
        from_slug: &str,
        id: FolderId,
        to_slug: &str,
        parent_id: Option<FolderId>,
    ) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        let same_collection = from_slug == to_slug;
        {
            let Some(entry) = self.collection(from_slug) else {
                return false;
            };
            if entry.folder(id).is_none() {
                return false;
            }
            let Some(target) = self.collection(to_slug) else {
                return false;
            };
            if same_collection {
                if !entry.collection.can_move_folder(id, parent_id) {
                    return false;
                }
            } else if parent_id.is_some_and(|parent| target.folder(parent).is_none()) {
                return false;
            }
            if same_collection && entry.folder(id).map(|folder| folder.parent_id) == Some(parent_id)
            {
                return true;
            }
        }
        if !same_collection {
            return self.move_folder_across(from_slug, id, to_slug, parent_id, &store);
        }
        let collection = {
            let Some(entry) = self.collection_mut(from_slug) else {
                return false;
            };
            entry.collection.move_folder(id, parent_id);
            entry.collection.clone()
        };
        let result = if self.needs_resync {
            self.resave(&store)
        } else {
            store.save_collection(&collection)
        };
        self.record_save(result);
        true
    }

    /// The cross-collection half of [`Self::move_folder_to`].
    ///
    /// The target collection document and every moved request document are
    /// written before any source document is removed. A failed step rolls the
    /// target back and restores any source files already removed, leaving
    /// memory and disk as they were.
    fn move_folder_across(
        &mut self,
        from_slug: &str,
        id: FolderId,
        to_slug: &str,
        parent_id: Option<FolderId>,
        store: &WorkspaceStore,
    ) -> bool {
        let (source, target) = {
            let Some(source) = self.collection(from_slug).cloned() else {
                return false;
            };
            let Some(target) = self.collection(to_slug).cloned() else {
                return false;
            };
            (source, target)
        };
        let subtree = source.folder_subtree_ids(id);
        if subtree.is_empty() {
            return false;
        }
        let subtree_set: HashSet<FolderId> = subtree.iter().copied().collect();
        let request_ids: Vec<RequestId> = source
            .requests
            .iter()
            .filter(|request| {
                request
                    .folder_id()
                    .is_some_and(|folder| subtree_set.contains(&folder))
            })
            .map(|request| request.id())
            .collect();
        // Every moved document is rewritten under the target, so all of them
        // must be present.
        for request_id in &request_ids {
            if self.ensure_request_loaded(from_slug, *request_id).is_err() {
                return false;
            }
        }
        let (updated_source, updated_target, moved) = {
            let Some(source) = self.collection(from_slug) else {
                return false;
            };
            let mut updated_source = source.clone();
            let mut updated_target = target.clone();
            let mut folders = Vec::with_capacity(subtree.len());
            for folder_id in &subtree {
                let Some(folder) = source.folder(*folder_id) else {
                    return false;
                };
                let mut folder = folder.clone();
                if *folder_id == id {
                    folder.parent_id = parent_id;
                }
                folders.push(folder);
            }
            let mut moved = Vec::with_capacity(request_ids.len());
            for request_id in &request_ids {
                let Some(spec) = source
                    .requests
                    .iter()
                    .find(|request| request.id() == *request_id)
                    .and_then(|request| request.spec())
                else {
                    return false;
                };
                moved.push(spec.clone());
            }
            updated_source
                .collection
                .folders
                .retain(|folder| !subtree_set.contains(&folder.id));
            updated_source
                .requests
                .retain(|request| !request_ids.contains(&request.id()));
            updated_target.collection.folders.extend(folders);
            updated_target.requests.extend(
                moved
                    .iter()
                    .cloned()
                    .map(|spec| RequestEntry::Loaded(Box::new(spec))),
            );
            sort_requests(&mut updated_target.requests);
            (updated_source, updated_target, moved)
        };

        // `removed` is how many source documents had already been deleted when
        // the failure happened; those are restored from the in-memory copy.
        let roll_back = |removed: usize| {
            let _ = store.save_collection(&target.collection);
            for spec in &moved {
                let _ = store.delete_request(to_slug, spec.id);
            }
            for spec in &moved[..removed] {
                let _ = store.save_request(from_slug, spec);
            }
            let _ = store.save_collection(&source.collection);
        };

        match store.save_collection(&updated_target.collection) {
            Ok(report) => self.issues.extend(report.into_issues()),
            Err(error) => {
                self.record_save(Err(error));
                return false;
            }
        }
        for spec in &moved {
            match store.save_request(to_slug, spec) {
                Ok(report) => self.issues.extend(report.into_issues()),
                Err(error) => {
                    roll_back(0);
                    self.record_save(Err(error));
                    return false;
                }
            }
        }
        let mut removed = 0;
        for spec in &moved {
            if let Err(error) = store.delete_request(from_slug, spec.id) {
                roll_back(removed);
                self.record_save(Err(error));
                return false;
            }
            removed += 1;
        }
        if let Err(error) = store.save_collection(&updated_source.collection) {
            roll_back(removed);
            self.record_save(Err(error));
            return false;
        }

        // Every document is settled, so the move is committed in memory.
        if let Some(entry) = self.collection_mut(from_slug) {
            *entry = updated_source;
        }
        if let Some(entry) = self.collection_mut(to_slug) {
            *entry = updated_target;
        }
        self.sync_index(store, from_slug);
        self.sync_index(store, to_slug);
        self.finish_mutation(store);
        true
    }

    /// Renames a request after trimming. Returns `true` when the in-memory name
    /// was changed, even if the save failed.
    pub fn rename_request(&mut self, slug: &str, id: RequestId, name: &str) -> bool {
        let name = match validated_name(name, &mut self.rename_error) {
            Some(name) => name,
            None => return false,
        };
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        // Renaming rewrites the document, so its full contents must be present.
        if self.ensure_request_loaded(slug, id).is_err() {
            return false;
        }
        let request = {
            let Some(entry) = self.collection_mut(slug) else {
                return false;
            };
            let Some(request) = entry.requests.iter_mut().find(|request| request.id() == id) else {
                return false;
            };
            let Some(spec) = request.spec_mut() else {
                return false;
            };
            spec.name = name.to_owned();
            sort_requests(&mut entry.requests);
            entry
                .requests
                .iter()
                .find(|request| request.id() == id)
                .and_then(|request| request.spec())
                .cloned()
        };
        let result = match request {
            Some(request) if !self.needs_resync => store.save_request(slug, &request),
            _ => self.resave(&store),
        };
        if result.is_ok() {
            self.sync_index(&store, slug);
        }
        self.record_save(result);
        true
    }

    /// Renames an environment after trimming, moving its file when the slug
    /// changes.
    ///
    /// Returns the resulting slug, or `None` when the name was rejected or the
    /// environment is not loaded. Renaming the active environment moves the
    /// persisted active identifier to the new slug. The new display name stays
    /// in memory even when the write fails.
    pub fn rename_environment(&mut self, slug: &str, name: &str) -> Option<String> {
        let name = validated_name(name, &mut self.rename_error)?;
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return None;
        };
        let current_name = self.environment(slug)?.environment.name.clone();
        // Confirming the current name is always accepted, even when the
        // workspace already contains another environment with that name.
        if !same_display_name(&current_name, name) && self.environment_name_taken(name, Some(slug))
        {
            self.rename_error = Some(i18n::t(Key::RenameEnvironmentTaken).to_owned());
            return None;
        }

        let target = match store.environment_slug_for(name, slug) {
            Ok(target) => target,
            Err(error) => {
                self.push_storage_issue(&store, &error, MessageKey::StorageEnvironmentNameFailed);
                slug.to_owned()
            }
        };

        let loaded = self
            .environments
            .iter_mut()
            .find(|loaded| loaded.slug == slug)
            .expect("environment checked above");
        loaded.environment.name = name.to_owned();
        self.sort_environments();

        if target != slug {
            match store.move_environment(slug, &target) {
                Ok(()) => {}
                Err(error) => {
                    self.push_storage_issue(
                        &store,
                        &error,
                        MessageKey::StorageEnvironmentMoveFailed,
                    );
                    let result = self.save_environment_document(&store, slug);
                    self.record_save(result);
                    return Some(slug.to_owned());
                }
            }
            if let Some(loaded) = self
                .environments
                .iter_mut()
                .find(|loaded| loaded.slug == slug)
            {
                loaded.slug = target.clone();
            }
            self.sort_environments();
            if self.workspace.active_environment.as_deref() == Some(slug) {
                self.workspace.active_environment = Some(target.clone());
            }
        }

        // The active selection is session state now; the rename does not
        // rewrite `workspace.json`.
        let result = self.save_environment_document(&store, &target);
        self.record_save(result);
        Some(target)
    }

    /// Deletes a collection after the caller confirmed. Memory changes only if
    /// the filesystem deletion succeeded.
    pub fn delete_collection(&mut self, slug: &str) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        match store.delete_collection(slug) {
            Ok(()) => {
                self.collections.retain(|entry| entry.collection.id != slug);
                store.drop_index(slug);
                self.finish_mutation(&store);
                true
            }
            Err(error) => {
                self.record_save(Err(error));
                false
            }
        }
    }

    /// Deletes an environment after the caller confirmed. Deleting the active
    /// environment clears the in-memory selection; the app records that in the
    /// session document.
    pub fn delete_environment(&mut self, slug: &str) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        match store.delete_environment(slug) {
            Ok(()) => {
                self.environments.retain(|loaded| loaded.slug != slug);
                if self.workspace.active_environment.as_deref() == Some(slug) {
                    self.workspace.active_environment = None;
                }
                self.record_save(Ok(SaveReport::default()));
                true
            }
            Err(error) => {
                self.record_save(Err(error));
                false
            }
        }
    }

    /// Activates `slug` (or clears the selection).
    ///
    /// The choice is session state, not workspace data: it is written with the
    /// session document the app owns, so `workspace.json` never changes here.
    /// Returns `false` for an unknown environment.
    pub fn set_active_environment(&mut self, slug: Option<String>) -> bool {
        if let Some(slug) = slug.as_deref()
            && self.environment(slug).is_none()
        {
            return false;
        }
        self.workspace.active_environment = slug;
        true
    }

    /// Writes `workspace.json`, or performs the full re-save when a previous
    /// write failed or the file could not be loaded safely.
    fn persist_workspace(&self, store: &WorkspaceStore) -> Result<SaveReport, StorageError> {
        if self.workspace_file_ok && !self.needs_resync {
            store.save_workspace(&self.workspace)
        } else {
            self.resave(store)
        }
    }

    /// Deletes a request after the caller confirmed.
    pub fn delete_request(&mut self, slug: &str, id: RequestId) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        match store.delete_request(slug, id) {
            Ok(()) => {
                if let Some(entry) = self
                    .collections
                    .iter_mut()
                    .find(|entry| entry.collection.id == slug)
                {
                    entry.requests.retain(|request| request.id() != id);
                }
                self.sync_index(&store, slug);
                self.finish_mutation(&store);
                true
            }
            Err(error) => {
                self.record_save(Err(error));
                false
            }
        }
    }

    /// Replaces a request's saved spec with an edited one. Returns `false` when
    /// the request is not part of the loaded workspace.
    pub fn update_request(&mut self, request: &RequestSpec) -> bool {
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            return false;
        };
        let Some(slug) = self
            .find_request(request.id)
            .map(|(slug, _)| slug.to_owned())
        else {
            return false;
        };
        if self.ensure_request_loaded(&slug, request.id).is_err() {
            return false;
        }
        let Some(entry) = self.collection_mut(&slug) else {
            return false;
        };
        let Some(saved) = entry
            .requests
            .iter_mut()
            .find(|saved| saved.id() == request.id)
            .and_then(|saved| saved.spec_mut())
        else {
            return false;
        };
        saved.clone_from(request);
        let result = if self.needs_resync {
            self.resave(&store)
        } else {
            store.save_request(&slug, request)
        };
        if result.is_ok() {
            self.sync_index(&store, &slug);
        }
        self.record_save(result);
        true
    }

    /// Materializes an imported bundle as new collections and environments.
    ///
    /// Every write goes through the store's own calls, so slug derivation,
    /// atomic writes, the index cache, and the sidebar refresh keep working
    /// unchanged. Failures are reported per item instead of aborting the rest
    /// of the import; a collection whose own document cannot be written is
    /// removed again rather than left behind empty.
    pub fn import_bundle(&mut self, bundle: ImportBundle) -> ImportOutcome {
        let mut outcome = ImportOutcome {
            skipped: bundle
                .warnings
                .into_iter()
                .map(|warning| ImportSkip {
                    item: warning.item,
                    reason: warning.reason,
                })
                .collect(),
            ..ImportOutcome::default()
        };
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            outcome.error = Some(i18n::t(Key::WorkspaceUnavailable).to_owned());
            return outcome;
        };

        for import in bundle.collections {
            let name = unique_import_name(
                &import.name,
                self.collections
                    .iter()
                    .map(|entry| entry.collection.name.as_str()),
            );
            let mut collection = match store.create_collection(&name, self.next_order()) {
                Ok(collection) => collection,
                Err(error) => {
                    outcome.skipped.push(ImportSkip {
                        item: name,
                        reason: error.to_string(),
                    });
                    self.record_save(Err(error));
                    continue;
                }
            };
            let slug = collection.id.clone();
            collection.folders = import.folders;
            if let Err(error) = store.save_collection(&collection) {
                outcome.skipped.push(ImportSkip {
                    item: name,
                    reason: error.to_string(),
                });
                self.record_save(Err(error));
                if let Err(error) = store.delete_collection(&slug) {
                    self.record_save(Err(error));
                }
                continue;
            }

            outcome.collections += 1;
            outcome.folders += collection.folders.len();
            self.collections.push(LoadedCollection {
                collection,
                requests: Vec::new(),
            });

            for request in import.requests {
                match store.save_request(&slug, &request) {
                    Ok(report) => {
                        self.issues.extend(report.into_issues());
                        outcome.requests += 1;
                        if let Some(entry) = self
                            .collections
                            .iter_mut()
                            .find(|entry| entry.collection.id == slug)
                        {
                            entry.requests.push(RequestEntry::Loaded(Box::new(request)));
                        }
                    }
                    Err(error) => {
                        outcome.skipped.push(ImportSkip {
                            item: format!("{name} / {}", request.name),
                            reason: error.to_string(),
                        });
                        self.record_save(Err(error));
                    }
                }
            }
            if let Some(entry) = self
                .collections
                .iter_mut()
                .find(|entry| entry.collection.id == slug)
            {
                sort_requests(&mut entry.requests);
            }
            self.sync_index(&store, &slug);
        }

        for import in bundle.environments {
            let name = unique_import_name(
                &import.name,
                self.environments
                    .iter()
                    .map(|loaded| loaded.environment.name.as_str()),
            );
            let (slug, mut environment) = match store.create_environment(&name) {
                Ok(created) => created,
                Err(error) => {
                    outcome.skipped.push(ImportSkip {
                        item: name,
                        reason: error.to_string(),
                    });
                    self.record_save(Err(error));
                    continue;
                }
            };
            environment.vars = import.vars;
            if let Err(error) = store.save_environment(&slug, &environment) {
                outcome.skipped.push(ImportSkip {
                    item: name,
                    reason: error.to_string(),
                });
                self.record_save(Err(error));
                if let Err(error) = store.delete_environment(&slug) {
                    self.record_save(Err(error));
                }
                continue;
            }
            outcome.environments += 1;
            self.environments
                .push(LoadedEnvironment { slug, environment });
        }

        self.sort_collections();
        self.sort_environments();
        self.finish_mutation(&store);
        outcome
    }

    /// Materializes imported requests into an existing collection.
    ///
    /// Only request documents are written; the collection document and its
    /// other requests are untouched. A request that cannot be written is
    /// reported and the rest still import. The caller checks the collection is
    /// loaded before calling, so a missing slug is reported instead of
    /// silently creating anything.
    pub fn import_requests_into(
        &mut self,
        slug: &str,
        requests: Vec<RequestSpec>,
    ) -> ImportOutcome {
        let mut outcome = ImportOutcome::default();
        let Some(store) = self.store.clone() else {
            self.fail_unavailable();
            outcome.error = Some(i18n::t(Key::WorkspaceUnavailable).to_owned());
            return outcome;
        };
        let Some(collection) = self.collection(slug) else {
            outcome.error = Some(i18n::t(Key::DestinationCollectionGone).to_owned());
            return outcome;
        };
        let collection_name = collection.collection.name.clone();

        for request in requests {
            match store.save_request(slug, &request) {
                Ok(report) => {
                    self.issues.extend(report.into_issues());
                    outcome.requests += 1;
                    if let Some(entry) = self
                        .collections
                        .iter_mut()
                        .find(|entry| entry.collection.id == slug)
                    {
                        entry.requests.push(RequestEntry::Loaded(Box::new(request)));
                    }
                }
                Err(error) => {
                    outcome.skipped.push(ImportSkip {
                        item: format!("{collection_name} / {}", request.name),
                        reason: error.to_string(),
                    });
                    self.record_save(Err(error));
                }
            }
        }
        if let Some(entry) = self
            .collections
            .iter_mut()
            .find(|entry| entry.collection.id == slug)
        {
            sort_requests(&mut entry.requests);
        }
        self.sync_index(&store, slug);
        outcome
    }

    /// Renders one collection and its folders and requests as a Posthaste bundle.
    ///
    /// Requests the sidebar has not read yet are loaded from their documents,
    /// so the bundle is a complete snapshot rather than whatever happened to be
    /// in memory. Identifiers are written as they are, which is what makes the
    /// document faithful to this collection.
    pub fn export_collection_bundle(&self, slug: &str) -> Result<String, InterchangeError> {
        let collection = self.load_bundle_collection(slug)?;
        Ok(render_bundle(&[collection], &[]))
    }

    /// Renders one collection as a Postman Collection v2.1 document.
    pub fn export_collection_postman(&self, slug: &str) -> Result<String, InterchangeError> {
        let collection = self.load_bundle_collection(slug)?;
        Ok(posthaste_core::interchange::postman::render_postman(
            &collection,
        ))
    }

    /// Loads every request the sidebar has not read yet, so an export is a
    /// complete snapshot rather than whatever happened to be in memory.
    fn load_bundle_collection(&self, slug: &str) -> Result<BundleCollection, InterchangeError> {
        let Some(entry) = self.collection(slug) else {
            return Err(InterchangeError::Unavailable {
                message: i18n::msg(MessageKey::CollectionNotLoaded, &[&slug]),
            });
        };
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| InterchangeError::Unavailable {
                message: i18n::t(Key::WorkspaceUnavailable).to_owned(),
            })?;

        let mut requests = Vec::with_capacity(entry.requests.len());
        for request in &entry.requests {
            let spec = match request {
                RequestEntry::Loaded(spec) => (**spec).clone(),
                RequestEntry::Unloaded { summary, .. } => store
                    .load_request(slug, summary.id)
                    .map_err(|error| InterchangeError::Unavailable {
                        message: i18n::msg(MessageKey::RequestReadFailed, &[&summary.name, &error]),
                    })?,
            };
            requests.push(spec);
        }

        Ok(BundleCollection {
            name: entry.collection.name.clone(),
            order: entry.collection.order,
            folders: entry.folders().to_vec(),
            requests,
        })
    }

    fn finish_mutation(&mut self, store: &WorkspaceStore) {
        if self.needs_resync {
            let result = self.resave(store);
            self.record_save(result);
        } else {
            self.record_save(Ok(SaveReport::default()));
        }
    }

    fn fail_unavailable(&mut self) {
        self.save_status = SaveStatus::Failed(i18n::t(Key::WorkspaceUnavailable).to_owned());
        self.needs_resync = true;
    }

    fn resave(&self, store: &WorkspaceStore) -> Result<SaveReport, StorageError> {
        if self.workspace_file_ok {
            store.save_all(&self.workspace, &self.collections, &self.environments)
        } else {
            let mut report = store.save_collections(&self.collections);
            report.append(store.save_environments(&self.environments));
            Ok(report)
        }
    }

    /// Saves one collection after a rename, or every loaded entry when an
    /// earlier write failed.
    fn save_collection_document(
        &self,
        store: &WorkspaceStore,
        slug: &str,
    ) -> Result<SaveReport, StorageError> {
        if self.needs_resync {
            return self.resave(store);
        }
        let Some(entry) = self.collection(slug) else {
            return Err(StorageError::InvalidEntry {
                path: store.collections_dir().join(slug),
                reason: "collection is not loaded".into(),
            });
        };
        store.save_collection(&entry.collection)
    }

    /// Saves one environment after a rename, or every loaded entry when an
    /// earlier write failed.
    fn save_environment_document(
        &self,
        store: &WorkspaceStore,
        slug: &str,
    ) -> Result<SaveReport, StorageError> {
        if self.needs_resync {
            return self.resave(store);
        }
        let Some(loaded) = self.environment(slug) else {
            return Err(StorageError::InvalidEntry {
                path: store.environments_dir().join(slug),
                reason: "environment is not loaded".into(),
            });
        };
        store.save_environment(slug, &loaded.environment)
    }

    /// Adds a non-fatal issue for a rename step that failed without preventing
    /// the rest of the save.
    fn push_storage_issue(
        &mut self,
        store: &WorkspaceStore,
        error: &StorageError,
        sentence: MessageKey,
    ) {
        self.issues.push(StorageIssue {
            path: error.path().unwrap_or_else(|| store.root()).to_path_buf(),
            message: i18n::msg(sentence, &[&error.detail()]),
        });
    }

    /// Records the outcome of one save.
    ///
    /// A partial failure keeps the in-memory change and names every document
    /// that was not written, one issue per path. Those issues belong to the
    /// save, so the next save that succeeds clears them; issues from loading
    /// are never touched.
    fn record_save(&mut self, result: Result<SaveReport, StorageError>) {
        self.clear_save_issues();
        match result {
            Ok(report) => {
                if report.is_success() {
                    self.save_status = SaveStatus::Saved;
                    self.needs_resync = false;
                } else {
                    let failed = report.failures.len();
                    self.save_status = SaveStatus::Failed(i18n::msg(
                        MessageKey::SaveDocumentsFailed,
                        &[&counted::item(counted::Item::Document, failed)],
                    ));
                    self.needs_resync = true;
                    for issue in report.into_issues() {
                        self.push_save_issue(issue);
                    }
                }
            }
            Err(error) => {
                self.save_status = SaveStatus::Failed(error.to_string());
                self.needs_resync = true;
            }
        }
        // Metadata writes are best effort and never change `SaveStatus`.
        if let Some(store) = &self.store {
            store.flush_indexes();
        }
    }

    /// Adds a visible issue that belongs to the most recent save.
    fn push_save_issue(&mut self, issue: StorageIssue) {
        self.save_issue_paths.push(issue.path.clone());
        self.issues.push(issue);
    }

    /// Removes the issues a previous save produced, keeping load issues.
    fn clear_save_issues(&mut self) {
        if self.save_issue_paths.is_empty() {
            return;
        }
        self.issues
            .retain(|issue| !self.save_issue_paths.contains(&issue.path));
        self.save_issue_paths.clear();
    }

    /// Removes one reported issue for the rest of the session.
    ///
    /// Dismissing writes nothing. An issue produced by a load describes a
    /// condition that still exists on disk, so it is reported again at the next
    /// start; an issue produced by a failed save is still cleared by the next
    /// save, as before. Issues are matched by value, so two reports on one path
    /// dismiss independently.
    pub fn dismiss_issue(&mut self, issue: &StorageIssue) {
        if let Some(index) = self.issues.iter().position(|candidate| candidate == issue) {
            self.issues.remove(index);
        }
    }

    /// The preserved damaged document whose deletion is awaiting confirmation.
    pub fn pending_issue_delete(&self) -> Option<&StorageIssue> {
        self.pending_issue_delete.as_ref()
    }

    /// Asks for confirmation before deleting a preserved damaged document.
    pub fn request_issue_delete(&mut self, issue: &StorageIssue) {
        self.pending_issue_delete = Some(issue.clone());
    }

    /// Cancels the pending deletion without touching any file.
    pub fn cancel_issue_delete(&mut self) {
        self.pending_issue_delete = None;
    }

    /// Deletes the preserved damaged document named by the pending issue.
    ///
    /// The store decides whether the path is one, so a view cannot delete a live
    /// document by mistake. A failed deletion keeps the issue visible with the
    /// failure as its message; replacing the message instead of appending keeps
    /// repeated attempts from stacking rows.
    pub fn confirm_issue_delete(&mut self) {
        let Some(issue) = self.pending_issue_delete.take() else {
            return;
        };
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.delete_quarantined_document(&issue.path) {
            Ok(()) => self.dismiss_issue(&issue),
            Err(error) => {
                let message =
                    i18n::msg(MessageKey::StoragePreservedDeleteFailed, &[&error.detail()]);
                match self
                    .issues
                    .iter_mut()
                    .find(|candidate| **candidate == issue)
                {
                    Some(existing) => existing.message = message,
                    None => self.issues.push(StorageIssue {
                        path: issue.path,
                        message,
                    }),
                }
            }
        }
    }

    /// Refreshes the derived metadata for one collection from memory.
    fn sync_index(&self, store: &WorkspaceStore, slug: &str) {
        match self.collection(slug) {
            Some(entry) => store.update_collection_index(slug, &entry.requests),
            None => store.drop_index(slug),
        }
    }

    fn sort_collections(&mut self) {
        self.collections.sort_by(|a, b| {
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
    }

    fn sort_environments(&mut self) {
        self.environments.sort_by(|a, b| {
            a.environment
                .name
                .to_lowercase()
                .cmp(&b.environment.name.to_lowercase())
                .then_with(|| a.slug.cmp(&b.slug))
        });
    }

    #[cfg(test)]
    pub(crate) fn force_resync_for_test(&mut self) {
        self.save_status = SaveStatus::Failed("synthetic failure".to_owned());
        self.needs_resync = true;
    }
}

fn validated_name<'a>(name: &'a str, error: &mut Option<String>) -> Option<&'a str> {
    let name = name.trim();
    if name.is_empty() {
        *error = Some(i18n::t(Key::RenameNameRequired).to_owned());
        None
    } else {
        *error = None;
        Some(name)
    }
}

/// Whether two display names are the same after trimming surrounding
/// whitespace, ignoring case. This mirrors the case-insensitive slug scheme
/// used for file names.
fn same_display_name(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

/// The first free copy name for a request: `<source name> copy`,
/// `<source name> copy 2`, ... among the destination folder's other requests.
fn request_copy_name<'a>(source: &str, occupied: impl IntoIterator<Item = &'a str>) -> String {
    copy_name(source, occupied)
}

/// The first free copy name for a folder: `<source name> copy`,
/// `<source name> copy 2`, ... among the destination parent's other children.
fn folder_copy_name<'a>(source: &str, occupied: impl IntoIterator<Item = &'a str>) -> String {
    copy_name(source, occupied)
}

fn copy_name<'a>(source: &str, occupied: impl IntoIterator<Item = &'a str>) -> String {
    free_default_name(&format!("{} copy", source.trim()), occupied)
}

/// The first of `base`, `base (2)`, `base (3)`, ... not used by `occupied`.
///
/// Imports suffix with parentheses so an imported name reads as a copy of an
/// existing item rather than as another default name.
fn unique_import_name<'a>(base: &str, occupied: impl IntoIterator<Item = &'a str>) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "Imported" } else { base };
    let occupied: Vec<&str> = occupied.into_iter().collect();
    if !occupied.iter().any(|name| same_display_name(name, base)) {
        return base.to_owned();
    }
    for suffix in 2.. {
        let candidate = format!("{base} ({suffix})");
        if !occupied
            .iter()
            .any(|name| same_display_name(name, &candidate))
        {
            return candidate;
        }
    }
    unreachable!("suffix space is unbounded")
}

/// The first of `base`, `base 2`, `base 3`, ... not used by `occupied`.
fn free_default_name<'a>(base: &str, occupied: impl IntoIterator<Item = &'a str>) -> String {
    let occupied: Vec<&str> = occupied.into_iter().collect();
    if !occupied.iter().any(|name| same_display_name(name, base)) {
        return base.to_owned();
    }
    for suffix in 2.. {
        let candidate = format!("{base} {suffix}");
        if !occupied
            .iter()
            .any(|name| same_display_name(name, &candidate))
        {
            return candidate;
        }
    }
    unreachable!("suffix space is unbounded")
}

/// Whether a sibling of `except` under `parent_id` already uses `name`.
/// `None` for `parent_id` is the collection root.
fn folder_name_taken(
    collection: &Collection,
    parent_id: Option<FolderId>,
    name: &str,
    except: Option<FolderId>,
) -> bool {
    collection.folders.iter().any(|folder| {
        folder.parent_id == parent_id
            && Some(folder.id) != except
            && same_display_name(&folder.name, name)
    })
}

fn sort_requests(requests: &mut [RequestEntry]) {
    requests.sort_by(|a, b| {
        a.name()
            .to_lowercase()
            .cmp(&b.name().to_lowercase())
            .then_with(|| a.id().cmp(&b.id()))
    });
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn workspace() -> (TempDir, WorkspaceState) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = WorkspaceState::load(store);
        assert!(state.fatal_error.is_none());
        (dir, state)
    }

    fn loaded_environment(slug: &str, vars: &[(&str, &str)]) -> LoadedEnvironment {
        let mut environment = posthaste_core::model::Environment::new(slug);
        environment.vars = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        LoadedEnvironment {
            slug: slug.to_owned(),
            environment,
        }
    }

    #[test]
    fn variable_catalog_merges_defaults_with_the_active_environment() {
        use posthaste_core::variables::VariableScope;

        let (_dir, mut state) = workspace();
        state.workspace.vars = [
            ("token".to_owned(), "default".to_owned()),
            ("host".to_owned(), "dev.example.com".to_owned()),
        ]
        .into();
        state.environments = vec![
            loaded_environment("staging", &[("token", "staging"), ("api_key", "k")]),
            loaded_environment("prod", &[("prod_host", "p")]),
        ];

        let catalog = state.variable_catalog(Some("staging"));
        assert_eq!(catalog.scope("host"), Some(VariableScope::Defaults));
        assert_eq!(catalog.scope("token"), Some(VariableScope::Environment));
        assert!(catalog.contains("api_key"));
        assert!(
            !catalog.contains("prod_host"),
            "inactive environments are excluded"
        );
        assert_eq!(
            catalog
                .candidates("")
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["api_key", "host", "token"]
        );

        let defaults_only = state.variable_catalog(None);
        assert_eq!(
            defaults_only.scope("token"),
            Some(VariableScope::Defaults),
            "without an active environment every name is a default"
        );
        assert!(!defaults_only.contains("api_key"));
    }

    #[test]
    fn folder_creation_validates_parents_and_persists() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");

        let root = state
            .create_folder(&collection.id, None, "  Auth  ")
            .expect("root folder");
        assert_eq!(root.name, "Auth");
        assert_eq!(root.parent_id, None);

        let child = state
            .create_folder(&collection.id, Some(root.id), "")
            .expect("child folder");
        assert_eq!(child.name, "New Folder");
        assert_eq!(child.parent_id, Some(root.id));

        assert!(
            state
                .create_folder(&collection.id, Some(FolderId::new()), "Orphan")
                .is_none(),
            "a missing parent is rejected"
        );
        assert!(
            state.create_folder("missing", None, "Lost").is_none(),
            "a missing collection is rejected"
        );
        assert_eq!(state.save_status, SaveStatus::Saved);

        let loaded = state.store.as_ref().unwrap().load().unwrap();
        let stored = &loaded.collections[0].collection;
        assert_eq!(stored.folders.len(), 2);
        assert_eq!(stored.folder(root.id).unwrap().name, "Auth");
        assert_eq!(stored.folder(child.id).unwrap().parent_id, Some(root.id));
    }

    #[test]
    fn folder_rename_validates_names_and_persists() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let folder = state
            .create_folder(&collection.id, None, "Auth")
            .expect("folder");

        assert!(!state.rename_folder(&collection.id, folder.id, "   "));
        assert_eq!(
            state.rename_error.as_deref(),
            Some(Key::RenameNameRequired.text(i18n::Lang::English))
        );
        assert!(!state.rename_folder(&collection.id, FolderId::new(), "Nope"));

        assert!(state.rename_folder(&collection.id, folder.id, "  Authentication  "));
        assert_eq!(
            state.folder(&collection.id, folder.id).unwrap().name,
            "Authentication"
        );

        let loaded = state.store.as_ref().unwrap().load().unwrap();
        assert_eq!(
            loaded.collections[0]
                .collection
                .folder(folder.id)
                .unwrap()
                .name,
            "Authentication"
        );
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn collection_rename_moves_its_directory_and_keeps_requests() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Ping")
            .expect("request");
        let store = state.store.clone().expect("store");

        let renamed = state.rename_collection("demo", "  Payments  ");

        assert_eq!(renamed.as_deref(), Some("payments"));
        assert!(!store.collection_dir("demo").unwrap().exists());
        assert!(store.collection_file("payments").unwrap().is_file());
        let entry = state.collection("payments").expect("renamed collection");
        assert_eq!(entry.collection.name, "Payments");
        assert_eq!(entry.collection.id, "payments");
        assert!(entry.requests.iter().any(|saved| saved.id() == request.id));

        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].collection.id, "payments");
        assert_eq!(loaded.collections[0].collection.name, "Payments");
        assert_eq!(loaded.collections[0].requests[0].id(), request.id);
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn collection_rename_to_the_same_slug_does_not_move_the_directory() {
        let (_dir, mut state) = workspace();
        state.create_collection("Demo").expect("collection");
        let store = state.store.clone().expect("store");

        let renamed = state.rename_collection("demo", "DEMO");

        assert_eq!(renamed.as_deref(), Some("demo"));
        assert!(store.collection_file("demo").unwrap().is_file());
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.collections[0].collection.id, "demo");
        assert_eq!(loaded.collections[0].collection.name, "DEMO");
    }

    #[test]
    fn collection_rename_collision_uses_a_distinct_slug() {
        let (_dir, mut state) = workspace();
        state.create_collection("Demo").expect("demo");
        let payments = state.create_collection("Payments").expect("payments");
        let request = state
            .create_request(&payments.id, "List invoices")
            .expect("request");
        let store = state.store.clone().expect("store");

        // The display names differ, but both slugify to "payments".
        let renamed = state.rename_collection("demo", "Payments!");

        assert_eq!(renamed.as_deref(), Some("payments-2"));
        assert!(!store.collection_dir("demo").unwrap().exists());
        assert!(store.collection_file("payments").unwrap().is_file());
        assert!(store.collection_file("payments-2").unwrap().is_file());
        assert_eq!(
            state.collection("payments-2").unwrap().collection.name,
            "Payments!"
        );
        assert_eq!(
            state.collection("payments").unwrap().collection.id,
            "payments"
        );
        assert!(
            state
                .collection("payments")
                .unwrap()
                .requests
                .iter()
                .any(|saved| saved.id() == request.id),
            "the other collection's request is untouched"
        );
    }

    #[test]
    fn a_failed_collection_move_keeps_the_old_slug_and_reports_an_issue() {
        // The reported sentence is English prose, so keep a language test from
        // switching the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (_dir, mut state) = workspace();
        state.create_collection("Demo").expect("collection");
        let store = state.store.clone().expect("store");
        std::fs::remove_dir_all(store.collection_dir("demo").unwrap()).unwrap();

        let renamed = state.rename_collection("demo", "Billing");

        assert_eq!(renamed.as_deref(), Some("demo"));
        assert!(!store.collection_dir("billing").unwrap().exists());
        assert!(
            state
                .issues
                .iter()
                .any(|issue| issue.message.contains("could not be moved")),
            "the move failure is visible"
        );
        assert_eq!(state.collection("demo").unwrap().collection.name, "Billing");
        assert!(
            store.collection_file("demo").unwrap().is_file(),
            "the new display name is persisted at the old path"
        );
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn a_failed_save_after_a_collection_move_rolls_the_directory_back() {
        let (_dir, mut state) = workspace();
        state.create_collection("Demo").expect("collection");
        let store = state.store.clone().expect("store");
        let collection_path = store.collection_file("demo").unwrap();
        std::fs::remove_file(&collection_path).unwrap();
        std::fs::create_dir(&collection_path).unwrap();

        let renamed = state.rename_collection("demo", "Billing");

        assert_eq!(renamed.as_deref(), Some("demo"));
        assert!(store.collection_dir("demo").unwrap().is_dir());
        assert!(!store.collection_dir("billing").unwrap().exists());
        assert_eq!(state.collection("demo").unwrap().collection.id, "demo");
        assert_eq!(state.collection("demo").unwrap().collection.name, "Billing");
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(state.needs_resync);
    }

    #[test]
    fn a_failed_request_write_is_visible_and_the_next_save_clears_it() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Ping")
            .expect("request");
        let store = state.store.clone().expect("store");
        let request_path = store.request_file(&collection.id, request.id).unwrap();

        // A directory in place of the request file makes that one write fail.
        std::fs::remove_file(&request_path).unwrap();
        std::fs::create_dir(&request_path).unwrap();

        let result = state.resave(&store);
        state.record_save(result);

        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(state.needs_resync);
        assert!(
            state.issues.iter().any(|issue| issue.path == request_path),
            "the failed document is named: {:?}",
            state.issues
        );
        assert!(
            state.collection(&collection.id).is_some(),
            "the change stays in memory"
        );

        std::fs::remove_dir(&request_path).unwrap();
        let result = state.resave(&store);
        state.record_save(result);

        assert_eq!(state.save_status, SaveStatus::Saved);
        assert!(!state.needs_resync);
        assert!(!state.issues.iter().any(|issue| issue.path == request_path));
    }

    #[test]
    fn dismissing_an_issue_removes_only_that_row() {
        let (_dir, mut state) = workspace();
        let first = StorageIssue {
            path: PathBuf::from("same-path"),
            message: "first".into(),
        };
        let second = StorageIssue {
            path: PathBuf::from("same-path"),
            message: "second".into(),
        };
        state.issues.push(first.clone());
        state.issues.push(second.clone());

        state.dismiss_issue(&first);

        assert_eq!(state.issues, vec![second]);
    }

    #[test]
    fn a_dismissed_save_issue_is_reported_again_by_the_next_failed_save() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Ping")
            .expect("request");
        let store = state.store.clone().expect("store");
        let request_path = store.request_file(&collection.id, request.id).unwrap();
        std::fs::remove_file(&request_path).unwrap();
        std::fs::create_dir(&request_path).unwrap();

        let result = state.resave(&store);
        state.record_save(result);
        let issue = state
            .issues
            .iter()
            .find(|issue| issue.path == request_path)
            .cloned()
            .expect("the failed write is reported");
        state.dismiss_issue(&issue);

        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(
            state.needs_resync,
            "dismissing does not make the save succeed"
        );
        assert!(!state.issues.iter().any(|issue| issue.path == request_path));

        let result = state.resave(&store);
        state.record_save(result);

        assert!(
            state.issues.iter().any(|issue| issue.path == request_path),
            "a later failure is reported again: {:?}",
            state.issues
        );
    }

    #[test]
    fn confirming_a_preserved_document_delete_removes_file_and_issue() {
        let (_dir, mut state) = workspace();
        let store = state.store.clone().expect("store");
        let preserved = store.root().join("workspace.json.corrupt-1");
        std::fs::write(&preserved, b"damaged").unwrap();
        let issue = StorageIssue {
            path: preserved.clone(),
            message: "damaged workspace document preserved".into(),
        };
        state.issues.push(issue.clone());

        state.request_issue_delete(&issue);
        state.confirm_issue_delete();

        assert!(!preserved.exists());
        assert!(!state.issues.iter().any(|candidate| candidate == &issue));
        assert!(state.pending_issue_delete().is_none());
    }

    #[test]
    fn cancelling_an_issue_delete_keeps_the_file_and_row() {
        let (_dir, mut state) = workspace();
        let store = state.store.clone().expect("store");
        let preserved = store.root().join("workspace.json.corrupt-2");
        std::fs::write(&preserved, b"damaged").unwrap();
        let issue = StorageIssue {
            path: preserved.clone(),
            message: "damaged workspace document preserved".into(),
        };
        state.issues.push(issue.clone());

        state.request_issue_delete(&issue);
        state.cancel_issue_delete();

        assert!(preserved.is_file());
        assert!(state.issues.contains(&issue));
        assert!(state.pending_issue_delete().is_none());
    }

    #[test]
    fn a_live_document_cannot_be_deleted_from_an_issue() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Ping")
            .expect("request");
        let store = state.store.clone().expect("store");
        let live = store.request_file(&collection.id, request.id).unwrap();
        let before = std::fs::read(&live).unwrap();
        let issue = StorageIssue {
            path: live.clone(),
            message: "the document could not be written".into(),
        };
        state.issues.push(issue.clone());

        state.request_issue_delete(&issue);
        state.confirm_issue_delete();

        assert_eq!(std::fs::read(&live).unwrap(), before);
        assert!(state.issues.iter().any(|candidate| candidate.path == live));
        assert!(state.pending_issue_delete().is_none());
    }

    #[test]
    fn a_failed_issue_delete_keeps_one_row_naming_the_failure() {
        // The reported sentence is English prose, so keep a language test from
        // switching the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (_dir, mut state) = workspace();
        let store = state.store.clone().expect("store");
        // A directory in place of the preserved file makes removal fail on
        // every platform.
        let preserved = store.root().join("workspace.json.corrupt-9");
        std::fs::create_dir(&preserved).unwrap();
        let issue = StorageIssue {
            path: preserved.clone(),
            message: "damaged workspace document preserved".into(),
        };
        state.issues.push(issue.clone());

        state.request_issue_delete(&issue);
        state.confirm_issue_delete();

        assert!(preserved.is_dir());
        let rows: Vec<_> = state
            .issues
            .iter()
            .filter(|candidate| candidate.path == preserved)
            .collect();
        assert_eq!(rows.len(), 1, "no stacked rows: {:?}", state.issues);
        assert!(
            rows[0]
                .message
                .contains("preserved document could not be deleted"),
            "{}",
            rows[0].message
        );
        assert!(state.pending_issue_delete().is_none());
    }

    #[test]
    fn environment_rename_moves_its_file() {
        let (_dir, mut state) = workspace();
        let loaded = state.create_environment("Dev").expect("environment");
        let store = state.store.clone().expect("store");

        let renamed = state.rename_environment(&loaded.slug, "Development");

        assert_eq!(renamed.as_deref(), Some("development"));
        assert!(!store.environment_file("dev").unwrap().exists());
        assert!(store.environment_file("development").unwrap().is_file());
        assert_eq!(
            state.environment("development").unwrap().environment.name,
            "Development"
        );
        let loaded = store.load().expect("reload");
        assert_eq!(loaded.environments[0].slug, "development");
        assert_eq!(loaded.environments[0].environment.name, "Development");
    }

    #[test]
    fn renaming_the_active_environment_moves_the_selection() {
        let (_dir, mut state) = workspace();
        state.create_environment("Dev").expect("environment");
        assert!(state.set_active_environment(Some("dev".into())));
        let store = state.store.clone().expect("store");
        let workspace_document =
            std::fs::read_to_string(store.workspace_file()).expect("workspace document");

        let renamed = state.rename_environment("dev", "Development");

        assert_eq!(renamed.as_deref(), Some("development"));
        assert_eq!(
            state.workspace.active_environment.as_deref(),
            Some("development")
        );
        assert_eq!(
            std::fs::read_to_string(store.workspace_file()).expect("workspace document"),
            workspace_document,
            "the selection is session state; a rename leaves workspace.json alone"
        );
    }

    #[test]
    fn environment_rename_collision_uses_a_distinct_slug() {
        let (_dir, mut state) = workspace();
        state.create_environment("Dev").expect("dev");
        state.create_environment("Prod").expect("prod");
        let store = state.store.clone().expect("store");

        // The display names differ, but both slugify to "prod".
        let renamed = state.rename_environment("dev", "Prod!");

        assert_eq!(renamed.as_deref(), Some("prod-2"));
        assert!(store.environment_file("prod").unwrap().is_file());
        assert!(store.environment_file("prod-2").unwrap().is_file());
        assert_eq!(state.environment("prod").unwrap().environment.name, "Prod");
        assert_eq!(
            state.environment("prod-2").unwrap().environment.name,
            "Prod!"
        );
    }

    #[test]
    fn a_failed_environment_move_keeps_the_old_slug_and_reports_an_issue() {
        // The reported sentence is English prose, so keep a language test from
        // switching the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (_dir, mut state) = workspace();
        state.create_environment("Dev").expect("environment");
        let store = state.store.clone().expect("store");
        let path = store.environment_file("dev").unwrap();
        std::fs::remove_file(&path).unwrap();

        let renamed = state.rename_environment("dev", "Development");

        assert_eq!(renamed.as_deref(), Some("dev"));
        assert!(!store.environment_file("development").unwrap().exists());
        assert!(
            state
                .issues
                .iter()
                .any(|issue| issue.message.contains("could not be moved")),
            "the move failure is visible"
        );
        assert_eq!(
            state.environment("dev").unwrap().environment.name,
            "Development"
        );
        assert!(
            path.is_file(),
            "the new display name is persisted at the old path"
        );
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn a_failed_save_after_an_environment_move_keeps_the_new_slug() {
        let (_dir, mut state) = workspace();
        state.create_environment("Dev").expect("environment");
        let store = state.store.clone().expect("store");
        let path = store.environment_file("dev").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        let renamed = state.rename_environment("dev", "Development");

        assert_eq!(renamed.as_deref(), Some("development"));
        assert!(state.environment("development").is_some());
        assert!(state.environment("dev").is_none());
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
    }

    #[test]
    fn folder_deletion_cascades_and_keeps_root_requests() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let billing = state
            .create_folder(&collection.id, None, "Billing")
            .expect("billing");
        let mut login = state
            .create_request(&collection.id, "Login")
            .expect("login");
        login.folder_id = Some(tokens.id);
        assert!(state.update_request(&login));
        let mut status = state
            .create_request(&collection.id, "Status")
            .expect("status");
        status.folder_id = Some(auth.id);
        assert!(state.update_request(&status));
        let ping = state.create_request(&collection.id, "Ping").expect("ping");

        assert!(!state.delete_folder(&collection.id, FolderId::new()));
        assert!(state.delete_folder(&collection.id, auth.id));

        let entry = state.collection(&collection.id).expect("collection");
        assert!(entry.folder(auth.id).is_none());
        assert!(entry.folder(tokens.id).is_none());
        assert!(entry.folder(billing.id).is_some());
        assert_eq!(entry.requests.len(), 1);
        assert_eq!(entry.requests[0].id(), ping.id);
        assert_eq!(state.save_status, SaveStatus::Saved);

        let store = state.store.as_ref().unwrap();
        assert!(
            !store
                .request_file(&collection.id, login.id)
                .unwrap()
                .exists()
        );
        assert!(
            !store
                .request_file(&collection.id, status.id)
                .unwrap()
                .exists()
        );
        assert!(
            store
                .request_file(&collection.id, ping.id)
                .unwrap()
                .is_file()
        );

        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.collections[0].collection.folders.len(), 1);
        assert_eq!(loaded.collections[0].requests.len(), 1);
    }

    #[test]
    fn request_moves_accept_valid_destinations_and_persist() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let request = state
            .create_request(&collection.id, "Login")
            .expect("request");

        assert!(state.move_request(&collection.id, request.id, Some(auth.id)));
        assert_eq!(
            state.request(&collection.id, request.id).unwrap().folder_id,
            Some(auth.id)
        );
        assert!(state.move_request(&collection.id, request.id, Some(tokens.id)));
        let loaded = state.store.as_ref().unwrap().load().unwrap();
        assert_eq!(
            loaded.collections[0]
                .requests
                .iter()
                .find(|saved| saved.id() == request.id)
                .unwrap()
                .folder_id(),
            Some(tokens.id),
            "a move rewrites only the request document"
        );

        // A move to the current parent is a no-op that writes nothing.
        state.force_resync_for_test();
        assert!(state.move_request(&collection.id, request.id, Some(tokens.id)));
        assert!(state.needs_resync, "no write happened for the no-op move");
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));

        assert!(!state.move_request(&collection.id, RequestId::new(), None));
        assert!(!state.move_request(&collection.id, request.id, Some(FolderId::new())));
        assert!(!state.move_request("missing", request.id, None));

        assert!(state.move_request(&collection.id, request.id, None));
        assert_eq!(
            state.request(&collection.id, request.id).unwrap().folder_id,
            None
        );
        assert!(!state.needs_resync);
    }

    #[test]
    fn folder_moves_reject_self_descendants_and_unknown_destinations() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let deeply = state
            .create_folder(&collection.id, Some(tokens.id), "Deep")
            .expect("deep");
        let billing = state
            .create_folder(&collection.id, None, "Billing")
            .expect("billing");
        let request = state
            .create_request(&collection.id, "Login")
            .expect("request");
        assert!(state.move_request(&collection.id, request.id, Some(tokens.id)));

        assert!(state.move_folder(&collection.id, billing.id, Some(auth.id)));
        assert_eq!(
            state.folder(&collection.id, billing.id).unwrap().parent_id,
            Some(auth.id)
        );
        assert!(!state.move_folder(&collection.id, auth.id, Some(auth.id)));
        assert!(!state.move_folder(&collection.id, auth.id, Some(deeply.id)));
        assert!(!state.move_folder(&collection.id, auth.id, Some(tokens.id)));
        assert!(!state.move_folder(&collection.id, auth.id, Some(FolderId::new())));
        assert!(!state.move_folder(&collection.id, FolderId::new(), None));
        assert!(!state.move_folder("missing", auth.id, None));

        assert!(state.move_folder(&collection.id, tokens.id, None));
        assert_eq!(
            state.folder(&collection.id, tokens.id).unwrap().parent_id,
            None
        );
        assert!(
            state
                .request(&collection.id, request.id)
                .is_some_and(|request| request.folder_id == Some(tokens.id)),
            "a folder move keeps its requests attached"
        );

        // A move to the current parent writes nothing.
        state.force_resync_for_test();
        assert!(state.move_folder(&collection.id, billing.id, Some(auth.id)));
        assert!(state.needs_resync);
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));

        let loaded = state.store.as_ref().unwrap().load().unwrap();
        let stored = &loaded.collections[0].collection;
        assert_eq!(stored.folder(billing.id).unwrap().parent_id, Some(auth.id));
        assert_eq!(stored.folder(tokens.id).unwrap().parent_id, None);
        assert_eq!(stored.folder(deeply.id).unwrap().parent_id, Some(tokens.id));
    }

    #[test]
    fn requests_move_between_collections_with_ids_and_names() {
        let (_dir, mut state) = workspace();
        let source = state.create_collection("Source").expect("source");
        let target = state.create_collection("Target").expect("target");
        let auth = state.create_folder(&target.id, None, "Auth").expect("auth");
        let request = state.create_request(&source.id, "Login").expect("request");
        let id = request.id;

        assert!(state.move_request_to(&source.id, id, &target.id, Some(auth.id)));

        assert!(state.request(&source.id, id).is_none());
        let moved = state.request(&target.id, id).expect("moved request");
        assert_eq!(moved.name, "Login");
        assert_eq!(moved.folder_id, Some(auth.id));
        assert_eq!(state.save_status, SaveStatus::Saved);

        let store = state.store.as_ref().unwrap();
        assert!(!store.request_file(&source.id, id).unwrap().exists());
        assert!(store.request_file(&target.id, id).unwrap().is_file());
        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        let source_entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == source.id)
            .unwrap();
        assert!(source_entry.requests.is_empty());
        let target_entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == target.id)
            .unwrap();
        assert_eq!(target_entry.requests.len(), 1);
        assert_eq!(target_entry.requests[0].id(), id);
        assert_eq!(target_entry.requests[0].folder_id(), Some(auth.id));

        // A missing request, target collection, or destination folder is
        // rejected, and the folder must belong to the target collection.
        assert!(!state.move_request_to(&source.id, RequestId::new(), &target.id, None));
        assert!(!state.move_request_to(&source.id, id, "missing", None));
        let elsewhere = state
            .create_folder(&source.id, None, "Elsewhere")
            .expect("elsewhere");
        assert!(!state.move_request_to(&source.id, id, &target.id, Some(elsewhere.id)));
    }

    #[test]
    fn folders_move_between_collections_with_their_subtree() {
        let (_dir, mut state) = workspace();
        let source = state.create_collection("Source").expect("source");
        let target = state.create_collection("Target").expect("target");
        let landing = state
            .create_folder(&target.id, None, "Landing")
            .expect("landing");
        let auth = state.create_folder(&source.id, None, "Auth").expect("auth");
        let tokens = state
            .create_folder(&source.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let login = state
            .create_request_in_folder(&source.id, "Login", Some(tokens.id))
            .expect("login");
        let ping = state
            .create_request_in_folder(&source.id, "Ping", Some(auth.id))
            .expect("ping");
        let loose = state.create_request(&source.id, "Loose").expect("loose");

        assert!(state.move_folder_to(&source.id, auth.id, &target.id, Some(landing.id)));

        assert!(state.folder(&source.id, auth.id).is_none());
        assert_eq!(
            state.folder(&target.id, auth.id).unwrap().parent_id,
            Some(landing.id),
            "the moved root lands under the chosen folder"
        );
        assert_eq!(
            state.folder(&target.id, tokens.id).unwrap().parent_id,
            Some(auth.id),
            "the subtree keeps its shape"
        );
        assert_eq!(state.folder(&target.id, tokens.id).unwrap().name, "Tokens");
        assert!(state.request(&source.id, login.id).is_none());
        assert!(state.request(&source.id, ping.id).is_none());
        assert!(state.request(&target.id, login.id).is_some());
        assert!(state.request(&target.id, ping.id).is_some());
        assert!(
            state.request(&source.id, loose.id).is_some(),
            "requests outside the subtree stay in the source collection"
        );
        assert_eq!(state.save_status, SaveStatus::Saved);

        let store = state.store.as_ref().unwrap();
        for (slug, id, exists) in [
            (&source.id, login.id, false),
            (&source.id, ping.id, false),
            (&target.id, login.id, true),
            (&target.id, ping.id, true),
            (&source.id, loose.id, true),
        ] {
            assert_eq!(
                store.request_file(slug, id).unwrap().is_file(),
                exists,
                "request {id:?} in {slug}"
            );
        }
        let loaded = store.load().unwrap();
        assert!(loaded.issues.is_empty());
        let source_entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == source.id)
            .unwrap();
        assert!(source_entry.collection.folder(auth.id).is_none());
        assert!(source_entry.collection.folder(tokens.id).is_none());
        assert_eq!(source_entry.requests.len(), 1);
        let target_entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == target.id)
            .unwrap();
        assert_eq!(target_entry.collection.folders.len(), 3);
        assert_eq!(target_entry.requests.len(), 2);

        // A destination folder in the source collection is not a valid target,
        // and unknown folders or collections are rejected.
        let outside = state
            .create_folder(&source.id, None, "Outside")
            .expect("outside");
        assert!(!state.move_folder_to(&source.id, outside.id, &target.id, Some(outside.id)));
        assert!(!state.move_folder_to(&source.id, FolderId::new(), &target.id, None));
        assert!(!state.move_folder_to(&source.id, outside.id, "missing", None));
    }

    #[test]
    fn request_creation_edits_and_renames_preserve_folder_references() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let folder = state
            .create_folder(&collection.id, None, "Auth")
            .expect("folder");

        let in_folder = state
            .create_request_in_folder(&collection.id, "Login", Some(folder.id))
            .expect("folder request");
        assert_eq!(in_folder.folder_id, Some(folder.id));
        let root = state
            .create_request(&collection.id, "Ping")
            .expect("root request");
        assert_eq!(root.folder_id, None);
        assert!(
            state
                .create_request_in_folder(&collection.id, "Lost", Some(FolderId::new()))
                .is_none(),
            "a missing destination folder is rejected"
        );

        let mut edited = in_folder.clone();
        edited.url = "https://example.com/login".into();
        edited.method = posthaste_core::model::HttpMethod::Post;
        assert!(state.update_request(&edited));
        assert!(state.rename_request(&collection.id, in_folder.id, "Sign in"));

        let store = state.store.as_ref().unwrap();
        let loaded = store.load().unwrap();
        let saved = loaded.collections[0]
            .requests
            .iter()
            .find(|saved| saved.id() == in_folder.id)
            .unwrap();
        assert_eq!(saved.name(), "Sign in");
        assert_eq!(saved.url(), "https://example.com/login");
        assert_eq!(saved.folder_id(), Some(folder.id));
        let saved_root = loaded.collections[0]
            .requests
            .iter()
            .find(|saved| saved.id() == root.id)
            .unwrap();
        assert_eq!(saved_root.folder_id(), None);

        let folder_document =
            std::fs::read_to_string(store.request_file(&collection.id, in_folder.id).unwrap())
                .unwrap();
        assert!(folder_document.contains("folder_id"));
        let root_document =
            std::fs::read_to_string(store.request_file(&collection.id, root.id).unwrap()).unwrap();
        assert!(!root_document.contains("folder_id"));
    }

    #[test]
    fn failed_folder_deletion_restores_the_folder_list() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let request = state
            .create_request(&collection.id, "Login")
            .expect("request");
        let collection_path = state
            .store
            .as_ref()
            .unwrap()
            .collection_file(&collection.id)
            .unwrap();
        std::fs::remove_file(&collection_path).unwrap();
        std::fs::create_dir(&collection_path).unwrap();

        assert!(!state.delete_folder(&collection.id, auth.id));

        let entry = state.collection(&collection.id).expect("collection");
        assert!(entry.folder(auth.id).is_some());
        assert_eq!(entry.requests.len(), 1);
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        let store = state.store.as_ref().unwrap();
        assert!(
            store
                .request_file(&collection.id, request.id)
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn saving_a_draft_writes_only_its_request_document() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let existing = state
            .create_request(&collection.id, "Ping")
            .expect("request");
        let store = state.store.clone().expect("store");
        let collection_document =
            std::fs::read_to_string(store.collection_file(&collection.id).unwrap()).unwrap();
        let existing_document =
            std::fs::read_to_string(store.request_file(&collection.id, existing.id).unwrap())
                .unwrap();

        let mut draft = RequestSpec::new("Draft");
        draft.method = posthaste_core::model::HttpMethod::Post;
        draft.url = "https://example.test/items".into();
        draft.timeout_ms = Some(2_500);

        assert!(state.save_new_request(&collection.id, Some(auth.id), &draft));
        assert_eq!(state.save_status, SaveStatus::Saved);

        let saved = state
            .request(&collection.id, draft.id)
            .expect("saved request");
        assert_eq!(saved.name, "Draft");
        assert_eq!(saved.method, posthaste_core::model::HttpMethod::Post);
        assert_eq!(saved.url, "https://example.test/items");
        assert_eq!(saved.folder_id, Some(auth.id));

        let loaded = store.load().expect("reload");
        let stored = loaded.collections[0]
            .requests
            .iter()
            .find(|request| request.id() == draft.id)
            .expect("loaded request");
        assert_eq!(stored.folder_id(), Some(auth.id));
        assert_eq!(stored.spec().expect("loaded").timeout_ms, Some(2_500));
        assert!(
            store
                .request_file(&collection.id, draft.id)
                .unwrap()
                .is_file(),
            "the file name is the draft identifier"
        );
        assert_eq!(
            std::fs::read_to_string(store.collection_file(&collection.id).unwrap()).unwrap(),
            collection_document,
            "the collection document is not rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(store.request_file(&collection.id, existing.id).unwrap())
                .unwrap(),
            existing_document,
            "other requests are not rewritten"
        );
    }

    #[test]
    fn saving_a_draft_at_the_root_omits_the_folder_reference() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let draft = RequestSpec::new("Draft");

        assert!(state.save_new_request(&collection.id, None, &draft));

        let store = state.store.clone().expect("store");
        let document =
            std::fs::read_to_string(store.request_file(&collection.id, draft.id).unwrap()).unwrap();
        assert!(!document.contains("folder_id"));
        assert_eq!(
            state.request(&collection.id, draft.id).unwrap().folder_id,
            None
        );
    }

    #[test]
    fn saving_a_draft_rejects_a_missing_destination() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let draft = RequestSpec::new("Draft");
        let store = state.store.clone().expect("store");

        assert!(!state.save_new_request("missing", None, &draft));
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(
            !state.save_new_request(&collection.id, Some(FolderId::new()), &draft),
            "a folder that did not load is rejected"
        );
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(
            !store
                .request_file(&collection.id, draft.id)
                .unwrap()
                .exists()
        );
        assert!(state.request(&collection.id, draft.id).is_none());
    }

    #[test]
    fn failed_draft_save_leaves_the_loaded_requests_unchanged() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let existing = state
            .create_request(&collection.id, "Ping")
            .expect("request");
        let draft = RequestSpec::new("Draft");
        let store = state.store.clone().expect("store");
        let path = store.request_file(&collection.id, draft.id).unwrap();
        // A directory cannot be replaced by the atomic rename, so the write
        // fails after the destination is resolved.
        std::fs::create_dir(&path).unwrap();

        assert!(!state.save_new_request(&collection.id, None, &draft));
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));

        let entry = state.collection(&collection.id).expect("collection");
        assert_eq!(entry.requests.len(), 1);
        assert_eq!(entry.requests[0].id(), existing.id);
        assert!(state.request(&collection.id, draft.id).is_none());
        assert!(path.is_dir(), "the destination is untouched");
    }

    #[test]
    fn default_names_skip_occupied_names_case_insensitively() {
        let (_dir, mut state) = workspace();

        assert_eq!(state.default_collection_name(), "New Collection");
        state.create_collection("New Collection").expect("first");
        assert_eq!(state.default_collection_name(), "New Collection 2");
        state.create_collection("new collection 2").expect("second");
        assert_eq!(state.default_collection_name(), "New Collection 3");
        state
            .create_collection(" NEW COLLECTION 3 ")
            .expect("third");
        assert_eq!(state.default_collection_name(), "New Collection 4");

        assert_eq!(state.default_environment_name(), "New Environment");
        state.create_environment("New Environment").expect("first");
        assert_eq!(state.default_environment_name(), "New Environment 2");
        state
            .create_environment("NEW ENVIRONMENT 2")
            .expect("second");
        assert_eq!(state.default_environment_name(), "New Environment 3");

        let folders = state.create_collection("Folders").expect("folders");
        let other = state.create_collection("Other").expect("other");
        let parent = state
            .create_folder(&folders.id, None, "Parent")
            .expect("parent");
        assert_eq!(state.default_folder_name(&folders.id, None), "New Folder");
        state
            .create_folder(&folders.id, None, "new folder")
            .expect("root folder");
        assert_eq!(state.default_folder_name(&folders.id, None), "New Folder 2");
        assert_eq!(
            state.default_folder_name(&folders.id, Some(parent.id)),
            "New Folder"
        );
        assert_eq!(state.default_folder_name(&other.id, None), "New Folder");
    }

    #[test]
    fn collection_names_must_be_unique() {
        let (_dir, mut state) = workspace();
        state.create_collection("Billing").expect("billing");
        let payments = state.create_collection("Payments").expect("payments");

        assert!(state.create_collection(" billing ").is_none());
        assert!(state.create_collection("PAYMENTS").is_none());
        assert_eq!(state.collections.len(), 2);

        assert!(state.rename_collection(&payments.id, " Billing ").is_none());
        assert_eq!(
            state.rename_error.as_deref(),
            Some(Key::RenameCollectionTaken.text(i18n::Lang::English))
        );
        assert_eq!(
            state.collection(&payments.id).unwrap().collection.name,
            "Payments"
        );
        assert_eq!(
            state.collection("billing").unwrap().collection.name,
            "Billing"
        );

        // Submitting the current name or a case-only variation is accepted.
        assert_eq!(
            state.rename_collection("billing", " BILLING ").as_deref(),
            Some("billing")
        );
        assert_eq!(
            state.collection("billing").unwrap().collection.name,
            "BILLING"
        );
        assert_eq!(state.rename_error, None);
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn existing_duplicate_collection_names_still_load() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_collection("Staging", 0).expect("first");
        store.create_collection("Staging", 1).expect("second");

        let mut state = WorkspaceState::load(store.clone());
        assert_eq!(state.collections.len(), 2);
        assert!(
            state
                .collections
                .iter()
                .all(|entry| entry.collection.name == "Staging")
        );

        // Confirming one duplicate's own name is a no-op that is accepted.
        let second = state.collections[1].collection.id.clone();
        assert_eq!(
            state.rename_collection(&second, "Staging").as_deref(),
            Some(second.as_str())
        );
    }

    #[test]
    fn request_names_may_repeat() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let first = state.create_request(&collection.id, "Ping").expect("first");
        let second = state
            .create_request(&collection.id, "Ping")
            .expect("second");

        assert_ne!(first.id, second.id);
        assert!(state.rename_request(&collection.id, second.id, "ping"));

        let store = state.store.clone().expect("store");
        assert!(
            store
                .request_file(&collection.id, first.id)
                .unwrap()
                .is_file()
        );
        assert!(
            store
                .request_file(&collection.id, second.id)
                .unwrap()
                .is_file()
        );
        let entry = state.collection(&collection.id).expect("collection");
        assert_eq!(entry.requests.len(), 2);
        assert!(
            entry
                .requests
                .iter()
                .all(|request| request.name().eq_ignore_ascii_case("ping"))
        );
    }

    #[test]
    fn environment_names_must_be_unique() {
        let (_dir, mut state) = workspace();
        state.create_environment("Dev").expect("dev");
        let prod = state.create_environment("Prod").expect("prod");
        assert!(state.set_active_environment(Some(prod.slug.clone())));
        let active = state.workspace.active_environment.clone();
        assert_eq!(active.as_deref(), Some("prod"));

        assert!(state.create_environment(" dev ").is_none());
        assert!(state.create_environment("PROD").is_none());
        assert_eq!(state.environments.len(), 2);
        assert_eq!(state.workspace.active_environment, active);

        assert!(state.rename_environment(&prod.slug, " DEV ").is_none());
        assert_eq!(
            state.rename_error.as_deref(),
            Some(Key::RenameEnvironmentTaken.text(i18n::Lang::English))
        );
        assert_eq!(state.environment("prod").unwrap().environment.name, "Prod");
        assert_eq!(state.environment("dev").unwrap().environment.name, "Dev");

        // Submitting the current name or a case-only variation is accepted.
        assert_eq!(
            state.rename_environment(&prod.slug, " PROD ").as_deref(),
            Some(prod.slug.as_str())
        );
        assert_eq!(
            state.environment(&prod.slug).unwrap().environment.name,
            "PROD"
        );
        assert_eq!(state.rename_error, None);
    }

    #[test]
    fn a_rejected_environment_rename_writes_nothing() {
        let (_dir, mut state) = workspace();
        state.create_environment("Dev").expect("dev");
        state.create_environment("Prod").expect("prod");
        assert!(state.set_active_environment(Some("dev".to_owned())));
        let store = state.store.clone().expect("store");
        let workspace_document = std::fs::read_to_string(store.workspace_file()).unwrap();
        let dev_document = std::fs::read_to_string(store.environment_file("dev").unwrap()).unwrap();
        let prod_document =
            std::fs::read_to_string(store.environment_file("prod").unwrap()).unwrap();

        assert!(state.rename_environment("dev", "Prod").is_none());

        assert_eq!(state.workspace.active_environment.as_deref(), Some("dev"));
        assert_eq!(
            std::fs::read_to_string(store.workspace_file()).unwrap(),
            workspace_document
        );
        assert_eq!(
            std::fs::read_to_string(store.environment_file("dev").unwrap()).unwrap(),
            dev_document
        );
        assert_eq!(
            std::fs::read_to_string(store.environment_file("prod").unwrap()).unwrap(),
            prod_document
        );
        assert!(!store.environment_file("prod-2").unwrap().exists());
    }

    #[test]
    fn existing_duplicate_environment_names_still_load() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        store.create_environment("Staging").expect("first");
        store.create_environment("Staging").expect("second");

        let mut state = WorkspaceState::load(store.clone());
        assert_eq!(state.environments.len(), 2);
        assert!(
            state
                .environments
                .iter()
                .all(|loaded| loaded.environment.name == "Staging")
        );

        let second = state.environments[1].slug.clone();
        assert_eq!(
            state.rename_environment(&second, "staging").as_deref(),
            Some(second.as_str())
        );
    }

    #[test]
    fn folder_sibling_names_must_be_unique() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, None, "Tokens")
            .expect("tokens");

        assert!(
            state
                .create_folder(&collection.id, None, " auth ")
                .is_none()
        );
        assert_eq!(state.collection(&collection.id).unwrap().folders().len(), 2);

        assert!(!state.rename_folder(&collection.id, tokens.id, "AUTH"));
        assert_eq!(
            state.rename_error.as_deref(),
            Some(Key::RenameFolderTaken.text(i18n::Lang::English))
        );
        assert_eq!(
            state.folder(&collection.id, tokens.id).unwrap().name,
            "Tokens"
        );

        // Confirming the current name or a case-only variation is accepted.
        assert!(state.rename_folder(&collection.id, tokens.id, " TOKENS "));
        assert_eq!(
            state.folder(&collection.id, tokens.id).unwrap().name,
            "TOKENS"
        );
        assert_eq!(state.rename_error, None);

        // The same name is allowed under a different parent.
        let nested = state
            .create_folder(&collection.id, Some(auth.id), "TOKENS")
            .expect("nested folder");
        assert_eq!(nested.parent_id, Some(auth.id));
    }

    #[test]
    fn existing_duplicate_folder_names_still_load() {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        store.load().expect("bootstrap");
        let mut collection = store.create_collection("Demo", 0).expect("collection");
        let first = collection.add_folder("Auth", None);
        let second = collection.add_folder("Auth", None);
        store.save_collection(&collection).expect("save");

        let mut state = WorkspaceState::load(store.clone());
        let entry = state.collection(&collection.id).expect("collection");
        assert_eq!(entry.folders().len(), 2);
        assert!(entry.folders().iter().all(|folder| folder.name == "Auth"));

        assert!(state.rename_folder(&collection.id, second, "auth"));
        assert!(state.folder(&collection.id, first).is_some());
        assert_eq!(state.folder(&collection.id, second).unwrap().name, "auth");
    }

    #[test]
    fn rejected_folder_names_write_nothing() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let billing = state
            .create_folder(&collection.id, None, "Billing")
            .expect("billing");
        let store = state.store.clone().expect("store");
        let before =
            std::fs::read_to_string(store.collection_file(&collection.id).unwrap()).unwrap();
        let order: Vec<FolderId> = state
            .collection(&collection.id)
            .unwrap()
            .folders()
            .iter()
            .map(|folder| folder.id)
            .collect();

        assert!(state.create_folder(&collection.id, None, "auth").is_none());
        assert!(!state.rename_folder(&collection.id, billing.id, "AUTH"));

        assert_eq!(
            std::fs::read_to_string(store.collection_file(&collection.id).unwrap()).unwrap(),
            before
        );
        let after: Vec<FolderId> = state
            .collection(&collection.id)
            .unwrap()
            .folders()
            .iter()
            .map(|folder| folder.id)
            .collect();
        assert_eq!(after, order);
    }

    fn request_document_count(store: &WorkspaceStore, slug: &str) -> usize {
        std::fs::read_dir(store.requests_dir(slug).expect("requests directory"))
            .expect("read requests directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .count()
    }

    #[test]
    fn copy_names_advance_past_existing_siblings_ignoring_case() {
        assert_eq!(request_copy_name("Login", [] as [&str; 0]), "Login copy");
        assert_eq!(request_copy_name(" Login ", ["login COPY"]), "Login copy 2");
        assert_eq!(
            request_copy_name("Login", ["Login copy", "Login copy 2"]),
            "Login copy 3"
        );
        assert_eq!(folder_copy_name("Auth", [" auth copy "]), "Auth copy 2");
    }

    #[test]
    fn request_duplication_copies_every_field_and_advances_the_name() {
        use posthaste_core::model::{HttpMethod, KeyValue, RequestAuth, RequestBody};

        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let folder = state
            .create_folder(&collection.id, None, "Auth")
            .expect("folder");
        let mut source = state
            .create_request_in_folder(&collection.id, "Login", Some(folder.id))
            .expect("source request");
        source.method = HttpMethod::Post;
        source.url = "https://example.test/login".to_owned();
        source.headers = vec![KeyValue::new("x-token", "{{token}}")];
        source.query = vec![KeyValue::new("page", "1")];
        source.body = RequestBody::Json("{\"name\":\"Ada\"}".to_owned());
        source.timeout_ms = Some(2_500);
        source.auth = RequestAuth::Bearer {
            token: "{{token}}".to_owned(),
        };
        assert!(state.update_request(&source));

        let store = state.store.clone().expect("store");
        let source_path = store
            .request_file(&collection.id, source.id)
            .expect("source path");
        let source_document = std::fs::read_to_string(&source_path).unwrap();
        let collection_path = store
            .collection_file(&collection.id)
            .expect("collection path");
        let collection_document = std::fs::read_to_string(&collection_path).unwrap();

        let copy = state
            .duplicate_request(&collection.id, source.id)
            .expect("copied request");

        assert_ne!(copy.id, source.id);
        assert_eq!(copy.name, "Login copy");
        assert_eq!(copy.folder_id, Some(folder.id));
        assert_eq!(copy.method, source.method);
        assert_eq!(copy.url, source.url);
        assert_eq!(copy.headers, source.headers);
        assert_eq!(copy.query, source.query);
        assert_eq!(copy.body, source.body);
        assert_eq!(copy.timeout_ms, source.timeout_ms);
        assert_eq!(copy.auth, source.auth);

        assert_eq!(state.request(&collection.id, copy.id), Some(&copy));
        assert!(
            store
                .request_file(&collection.id, copy.id)
                .expect("copy path")
                .is_file()
        );
        assert_eq!(request_document_count(&store, &collection.id), 2);
        assert_eq!(
            std::fs::read_to_string(&source_path).unwrap(),
            source_document,
            "the source request document is not rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&collection_path).unwrap(),
            collection_document,
            "the collection document is not rewritten"
        );

        let second = state
            .duplicate_request(&collection.id, source.id)
            .expect("second copy");
        assert_eq!(second.name, "Login copy 2");
        assert_eq!(request_document_count(&store, &collection.id), 3);

        let before = request_document_count(&store, &collection.id);
        assert!(
            state
                .duplicate_request(&collection.id, RequestId::new())
                .is_none(),
            "a missing source is rejected"
        );
        assert_eq!(request_document_count(&store, &collection.id), before);
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn folder_duplication_remaps_the_subtree_and_places_it_beside_the_source() {
        use posthaste_core::model::RequestBody;

        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let mut login = state
            .create_request_in_folder(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let mut refresh = state
            .create_request_in_folder(&collection.id, "Refresh", Some(tokens.id))
            .expect("refresh");
        let _ping = state
            .create_request_in_folder(&collection.id, "Ping", None)
            .expect("ping");
        login.url = "https://example.test/login".to_owned();
        refresh.body = RequestBody::Json("{\"grant\":\"refresh\"}".to_owned());
        assert!(state.update_request(&login));
        assert!(state.update_request(&refresh));

        let store = state.store.clone().expect("store");
        let login_path = store
            .request_file(&collection.id, login.id)
            .expect("login path");
        let refresh_path = store
            .request_file(&collection.id, refresh.id)
            .expect("refresh path");
        let login_document = std::fs::read_to_string(&login_path).unwrap();
        let refresh_document = std::fs::read_to_string(&refresh_path).unwrap();
        let requests_before = request_document_count(&store, &collection.id);

        let copy = state
            .duplicate_folder(&collection.id, auth.id)
            .expect("folder copy");

        assert_eq!(copy.name, "Auth copy");
        assert_eq!(copy.parent_id, None);
        assert_ne!(copy.id, auth.id);

        let (tokens_copy, login_copy, refresh_copy) = {
            let entry = state.collection(&collection.id).expect("collection");
            let tokens_copy = entry
                .folders()
                .iter()
                .find(|folder| folder.name == "Tokens" && folder.parent_id == Some(copy.id))
                .expect("tokens copy")
                .clone();
            let login_copy = entry
                .requests
                .iter()
                .find(|request| request.name() == "Login" && request.folder_id() == Some(copy.id))
                .expect("login copy")
                .clone();
            let refresh_copy = entry
                .requests
                .iter()
                .find(|request| {
                    request.name() == "Refresh" && request.folder_id() == Some(tokens_copy.id)
                })
                .expect("refresh copy")
                .clone();
            (tokens_copy, login_copy, refresh_copy)
        };
        assert_ne!(tokens_copy.id, tokens.id);
        assert_eq!(tokens_copy.parent_id, Some(copy.id));
        assert_ne!(login_copy.id(), login.id);
        assert_eq!(login_copy.url(), login.url);
        assert_ne!(refresh_copy.id(), refresh.id);
        assert_eq!(refresh_copy.spec().expect("loaded").body, refresh.body);

        // The source subtree keeps its identifiers and shape.
        assert_eq!(
            state.folder(&collection.id, auth.id).unwrap().parent_id,
            None
        );
        assert_eq!(
            state.folder(&collection.id, tokens.id).unwrap().parent_id,
            Some(auth.id)
        );
        assert_eq!(
            state.request(&collection.id, login.id).unwrap().folder_id,
            Some(auth.id)
        );
        assert_eq!(
            state.request(&collection.id, refresh.id).unwrap().folder_id,
            Some(tokens.id)
        );
        assert_eq!(
            std::fs::read_to_string(&login_path).unwrap(),
            login_document,
            "source request documents are not rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&refresh_path).unwrap(),
            refresh_document
        );
        assert_eq!(
            request_document_count(&store, &collection.id),
            requests_before + 2
        );
        assert!(state.issues.is_empty());
        assert_eq!(state.save_status, SaveStatus::Saved);

        let loaded = store.load().expect("reload");
        let entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == collection.id)
            .expect("reloaded collection");
        assert!(entry.collection.folder(copy.id).is_some());
        assert!(entry.collection.folder(tokens_copy.id).is_some());
        assert!(
            entry
                .requests
                .iter()
                .any(|request| request.id() == login_copy.id())
        );
        assert!(
            entry
                .requests
                .iter()
                .any(|request| request.id() == refresh_copy.id()
                    && request.folder_id() == Some(tokens_copy.id))
        );
    }

    #[test]
    fn folder_duplication_handles_empty_nested_and_repeated_sources() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let empty = state
            .create_folder(&collection.id, None, "Empty")
            .expect("empty");
        let store = state.store.clone().expect("store");
        let requests_before = request_document_count(&store, &collection.id);

        let empty_copy = state
            .duplicate_folder(&collection.id, empty.id)
            .expect("empty copy");
        assert_eq!(empty_copy.name, "Empty copy");
        assert_eq!(empty_copy.parent_id, None);
        assert_eq!(
            request_document_count(&store, &collection.id),
            requests_before
        );

        let nested_copy = state
            .duplicate_folder(&collection.id, tokens.id)
            .expect("nested copy");
        assert_eq!(nested_copy.name, "Tokens copy");
        assert_eq!(nested_copy.parent_id, Some(auth.id));
        assert_ne!(nested_copy.id, tokens.id);

        let auth_copy = state
            .duplicate_folder(&collection.id, auth.id)
            .expect("auth copy");
        assert_eq!(auth_copy.name, "Auth copy");
        assert_eq!(auth_copy.parent_id, None);
        let second = state
            .duplicate_folder(&collection.id, auth.id)
            .expect("second auth copy");
        assert_eq!(second.name, "Auth copy 2");
        assert_eq!(second.parent_id, None);

        assert_eq!(
            state.folder(&collection.id, tokens.id).unwrap().parent_id,
            Some(auth.id),
            "the source keeps its parent"
        );
        assert_eq!(
            state.folder(&collection.id, empty.id).unwrap().name,
            "Empty"
        );
        assert!(
            state
                .duplicate_folder(&collection.id, FolderId::new())
                .is_none()
        );
        assert_eq!(state.save_status, SaveStatus::Saved);
    }

    #[test]
    fn a_failed_collection_write_leaves_no_folder_copy() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let _login = state
            .create_request_in_folder(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let store = state.store.clone().expect("store");
        let collection_path = store
            .collection_file(&collection.id)
            .expect("collection path");
        std::fs::remove_file(&collection_path).unwrap();
        std::fs::create_dir(&collection_path).unwrap();

        let folders_before = state
            .collection(&collection.id)
            .expect("collection")
            .collection
            .folders
            .clone();
        let requests_before = state
            .collection(&collection.id)
            .expect("collection")
            .requests
            .clone();
        let files_before = request_document_count(&store, &collection.id);

        assert!(state.duplicate_folder(&collection.id, auth.id).is_none());

        assert_eq!(
            state
                .collection(&collection.id)
                .expect("collection")
                .collection
                .folders,
            folders_before
        );
        assert_eq!(
            state
                .collection(&collection.id)
                .expect("collection")
                .requests,
            requests_before
        );
        assert_eq!(request_document_count(&store, &collection.id), files_before);
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(state.needs_resync);
    }

    #[test]
    fn a_failed_request_write_reports_a_partial_folder_copy() {
        // The reported sentence is English prose, so keep a language test from
        // switching the interface while this one asserts it.
        let _guard = crate::test_globals::InterfaceGuard::take();
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let tokens = state
            .create_folder(&collection.id, Some(auth.id), "Tokens")
            .expect("tokens");
        let login = state
            .create_request_in_folder(&collection.id, "Login", Some(auth.id))
            .expect("login");
        let refresh = state
            .create_request_in_folder(&collection.id, "Refresh", Some(tokens.id))
            .expect("refresh");
        let store = state.store.clone().expect("store");
        let files_before = request_document_count(&store, &collection.id);
        let drafts = BTreeMap::new();

        let copy = state
            .duplicate_folder_with_writer(
                &collection.id,
                auth.id,
                &drafts,
                |store, slug, request| {
                    if request.name == "Refresh" {
                        Err(StorageError::InvalidEntry {
                            path: store
                                .request_file(slug, request.id)
                                .expect("failed copy path"),
                            reason: "synthetic request write failure".to_owned(),
                        })
                    } else {
                        store.save_request(slug, request)
                    }
                },
            )
            .expect("partial copy");

        assert_eq!(copy.name, "Auth copy");
        let (tokens_copy, login_copy) = {
            let entry = state.collection(&collection.id).expect("collection");
            assert!(entry.folders().iter().any(|folder| folder.id == copy.id));
            let tokens_copy = entry
                .folders()
                .iter()
                .find(|folder| folder.name == "Tokens" && folder.parent_id == Some(copy.id))
                .expect("tokens copy")
                .id;
            let login_copy = entry
                .requests
                .iter()
                .find(|request| request.name() == "Login" && request.folder_id() == Some(copy.id))
                .expect("login copy")
                .clone();
            assert!(
                !entry
                    .requests
                    .iter()
                    .any(|request| request.name() == "Refresh"
                        && request.folder_id() == Some(tokens_copy)),
                "the failed request copy is not kept in memory"
            );
            (tokens_copy, login_copy)
        };
        assert_ne!(login_copy.id(), login.id);
        assert_eq!(
            request_document_count(&store, &collection.id),
            files_before + 1,
            "only the successful request copy is written"
        );
        assert!(
            store
                .request_file(&collection.id, login_copy.id())
                .expect("login copy path")
                .is_file()
        );
        assert!(
            state
                .issues
                .iter()
                .any(|issue| issue.message.contains("request copy could not be written")),
            "the partial copy is reported"
        );
        assert!(matches!(state.save_status, SaveStatus::Failed(_)));
        assert!(state.needs_resync);

        // The source requests keep their folders and files.
        assert_eq!(
            state.request(&collection.id, login.id).unwrap().folder_id,
            Some(auth.id)
        );
        assert_eq!(
            state.request(&collection.id, refresh.id).unwrap().folder_id,
            Some(tokens.id)
        );

        // The committed collection document already contains the copied folder
        // and the successful request's folder mapping.
        let loaded = store.load().expect("reload");
        let entry = loaded
            .collections
            .iter()
            .find(|entry| entry.collection.id == collection.id)
            .expect("reloaded collection");
        assert!(entry.collection.folder(copy.id).is_some());
        assert!(entry.collection.folder(tokens_copy).is_some());
    }

    fn index_document(store: &WorkspaceStore, slug: &str) -> serde_json::Value {
        let path = store.index_file(slug).expect("cache index path");
        serde_json::from_str(&std::fs::read_to_string(path).expect("index document"))
            .expect("valid index json")
    }

    fn cached_workspace() -> (TempDir, TempDir) {
        (
            tempfile::tempdir().expect("temp workspace"),
            tempfile::tempdir().expect("temp cache"),
        )
    }

    #[test]
    fn ensure_request_loaded_reads_once_and_keeps_the_entry_loaded() {
        let (root, cache) = cached_workspace();
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        let mut state = WorkspaceState::load(store.clone());
        let collection = state.create_collection("Demo").expect("collection");
        let first = state
            .create_request(&collection.id, "First")
            .expect("first");
        let _second = state
            .create_request(&collection.id, "Second")
            .expect("second");

        let mut state = WorkspaceState::load(store.clone());
        let entry = state
            .request_entry(&collection.id, first.id)
            .expect("unloaded entry");
        assert!(entry.spec().is_none(), "startup leaves the document unread");

        assert!(
            state
                .ensure_request_loaded(&collection.id, first.id)
                .is_ok(),
            "the first selection reads the document"
        );
        assert_eq!(
            state
                .request(&collection.id, first.id)
                .map(|spec| spec.name.as_str()),
            Some("First")
        );

        // A loaded entry is reused even when the file can no longer be read.
        let path = store.request_file(&collection.id, first.id).expect("path");
        std::fs::write(&path, "not json").expect("corrupt the file");
        assert!(
            state
                .ensure_request_loaded(&collection.id, first.id)
                .is_ok(),
            "a second selection must not read the file again"
        );
        assert_eq!(
            state
                .request(&collection.id, first.id)
                .map(|spec| spec.name.as_str()),
            Some("First")
        );
        assert!(
            !state.issues.iter().any(|issue| issue.path == path),
            "no issue is recorded for a cached document"
        );
    }

    #[test]
    fn a_failed_load_records_an_issue_and_leaves_other_requests_usable() {
        let (root, cache) = cached_workspace();
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        let mut state = WorkspaceState::load(store.clone());
        let collection = state.create_collection("Demo").expect("collection");
        let broken = state
            .create_request(&collection.id, "Broken")
            .expect("broken");
        let healthy = state
            .create_request(&collection.id, "Healthy")
            .expect("healthy");
        let mut state = WorkspaceState::load(store.clone());
        assert!(
            state
                .request_entry(&collection.id, broken.id)
                .expect("broken entry")
                .spec()
                .is_none()
        );

        let path = store.request_file(&collection.id, broken.id).expect("path");
        std::fs::write(&path, "{").expect("corrupt the file");
        assert!(
            state
                .ensure_request_loaded(&collection.id, broken.id)
                .is_err(),
            "a corrupt document cannot be loaded"
        );
        assert!(
            state.request_entry(&collection.id, broken.id).is_none(),
            "the failed entry is dropped"
        );
        assert!(
            state
                .issues
                .iter()
                .any(|issue| issue.path == path && !issue.message.is_empty()),
            "the failure is reported as a storage issue"
        );
        assert!(
            state
                .ensure_request_loaded(&collection.id, healthy.id)
                .is_ok(),
            "other requests remain usable"
        );
        assert_eq!(
            state
                .request(&collection.id, healthy.id)
                .map(|spec| spec.name.as_str()),
            Some("Healthy")
        );
    }

    #[test]
    fn unloaded_renames_and_moves_preserve_untouched_fields() {
        use posthaste_core::model::{HttpMethod, KeyValue, RequestBody};

        let (root, cache) = cached_workspace();
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        let mut state = WorkspaceState::load(store.clone());
        let collection = state.create_collection("Demo").expect("collection");
        let auth = state
            .create_folder(&collection.id, None, "Auth")
            .expect("auth");
        let archive = state
            .create_folder(&collection.id, None, "Archive")
            .expect("archive");
        let mut request = state
            .create_request_in_folder(&collection.id, "Login", Some(auth.id))
            .expect("request");
        request.method = HttpMethod::Post;
        request.url = "https://example.test/login".into();
        request.headers = vec![KeyValue::new("X-Trace", "1")];
        request.query = vec![KeyValue::new("expand", "lines")];
        request.body = RequestBody::Json(r#"{"user":"ann"}"#.into());
        request.timeout_ms = Some(4_200);
        assert!(state.update_request(&request));

        // Reloading through the index leaves the document unread.
        let mut state = WorkspaceState::load(store.clone());
        assert!(
            state
                .request_entry(&collection.id, request.id)
                .expect("entry")
                .spec()
                .is_none()
        );

        assert!(state.rename_request(&collection.id, request.id, "Sign in"));
        assert_eq!(
            state
                .request(&collection.id, request.id)
                .map(|spec| spec.name.as_str()),
            Some("Sign in"),
            "rename loads the document first"
        );
        assert!(state.move_request(&collection.id, request.id, Some(archive.id)));

        let stored = store
            .load_request(&collection.id, request.id)
            .expect("stored request");
        assert_eq!(stored.name, "Sign in");
        assert_eq!(stored.folder_id, Some(archive.id));
        assert_eq!(stored.method, HttpMethod::Post);
        assert_eq!(stored.url, "https://example.test/login");
        assert_eq!(stored.headers, vec![KeyValue::new("X-Trace", "1")]);
        assert_eq!(stored.query, vec![KeyValue::new("expand", "lines")]);
        assert_eq!(stored.body, RequestBody::Json(r#"{"user":"ann"}"#.into()));
        assert_eq!(stored.timeout_ms, Some(4_200));
    }

    #[test]
    fn deleting_an_unreadable_request_does_not_load_it() {
        let (root, cache) = cached_workspace();
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        let mut state = WorkspaceState::load(store.clone());
        let collection = state.create_collection("Demo").expect("collection");
        let doomed = state
            .create_request(&collection.id, "Doomed")
            .expect("doomed");
        let kept = state.create_request(&collection.id, "Kept").expect("kept");

        let mut state = WorkspaceState::load(store.clone());
        let path = store.request_file(&collection.id, doomed.id).expect("path");
        std::fs::write(&path, "not json").expect("corrupt the file");
        assert!(
            state.delete_request(&collection.id, doomed.id),
            "delete does not need to parse the document"
        );
        assert!(!path.exists(), "the request file is removed");
        assert!(state.request_entry(&collection.id, doomed.id).is_none());
        assert!(
            state.issues.iter().all(|issue| issue.path != path),
            "deleting an unreadable document records no load issue"
        );
        assert!(
            state
                .request_entry(&collection.id, kept.id)
                .expect("kept entry")
                .spec()
                .is_none(),
            "unrelated requests stay unloaded"
        );
    }

    #[test]
    fn created_requests_are_immediately_loaded() {
        let (root, cache) = cached_workspace();
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        let mut state = WorkspaceState::load(store);
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Fresh")
            .expect("request");

        let entry = state
            .request_entry(&collection.id, request.id)
            .expect("created entry");
        assert_eq!(
            entry.spec().map(|spec| spec.name.as_str()),
            Some("Fresh"),
            "a created request is inserted loaded"
        );
    }

    #[test]
    fn successful_mutations_update_the_cached_index() {
        let (root, cache) = cached_workspace();
        let store = WorkspaceStore::at_cached(root.path(), cache.path());
        let mut state = WorkspaceState::load(store.clone());
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Ping")
            .expect("request");

        assert!(state.rename_request(&collection.id, request.id, "Pong"));
        let index = index_document(&store, &collection.id);
        let entries = index["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "Pong", "the rename reaches the index");
        assert_eq!(entries[0]["url"], "");

        assert!(state.delete_request(&collection.id, request.id));
        let index = index_document(&store, &collection.id);
        assert!(
            index["entries"].as_array().expect("entries").is_empty(),
            "delete drops the index entry"
        );
    }

    #[test]
    fn an_unwritable_cache_does_not_change_save_status() {
        let root = tempfile::tempdir().expect("temp workspace");
        let cache_file = tempfile::NamedTempFile::new().expect("cache path file");
        let store = WorkspaceStore::at_cached(root.path(), cache_file.path());
        let mut state = WorkspaceState::load(store.clone());
        let collection = state.create_collection("Demo").expect("collection");
        let request = state
            .create_request(&collection.id, "Ping")
            .expect("request");

        assert_eq!(state.save_status, SaveStatus::Saved);
        assert!(!state.needs_resync);
        let stored = store
            .load_request(&collection.id, request.id)
            .expect("the request file was written");
        assert_eq!(stored.name, "Ping");
        assert!(
            store
                .index_file(&collection.id)
                .is_some_and(|path| !path.is_file()),
            "the cache write failed silently"
        );
    }
}

#[cfg(test)]
mod import_export_tests {
    use tempfile::TempDir;

    use super::*;
    use posthaste_core::interchange::{
        ImportBundle, ImportCollection, ImportFormat, ImportWarning, parse, sniff,
    };
    use posthaste_core::model::{Environment, KeyValue};

    fn workspace() -> (TempDir, WorkspaceState) {
        let dir = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::at(dir.path());
        let state = WorkspaceState::load(store);
        assert!(state.fatal_error.is_none());
        (dir, state)
    }

    fn sample_bundle() -> ImportBundle {
        let animals = Folder::new("Animals", None);
        let dogs = Folder::new("Dogs", Some(animals.id));
        let mut request = RequestSpec::new("List dogs");
        request.url = "https://api.example.com/dogs".to_owned();
        request.folder_id = Some(dogs.id);
        request.headers = vec![KeyValue::new("Accept", "application/json")];
        request.query = vec![KeyValue {
            key: "limit".to_owned(),
            value: "10".to_owned(),
            enabled: false,
        }];

        let mut environment = Environment::new("staging");
        environment.vars.insert(
            "base_url".to_owned(),
            "https://staging.example.com".to_owned(),
        );

        ImportBundle {
            collections: vec![ImportCollection {
                name: "Petstore".to_owned(),
                order: 0,
                folders: vec![animals, dogs],
                requests: vec![request],
            }],
            environments: vec![environment],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn an_import_creates_a_collection_with_its_tree_values_and_environment() {
        let (dir, mut state) = workspace();
        let outcome = state.import_bundle(sample_bundle());

        assert_eq!(outcome.collections, 1);
        assert_eq!(outcome.folders, 2);
        assert_eq!(outcome.requests, 1);
        assert_eq!(outcome.environments, 1);
        assert!(outcome.skipped.is_empty());
        assert!(outcome.error.is_none());

        // Everything above went through the store, so a fresh load sees it.
        let reopened = WorkspaceState::load(WorkspaceStore::at(dir.path()));
        assert!(reopened.environments.iter().any(|loaded| {
            loaded.environment.name == "staging"
                && loaded.environment.vars.get("base_url").map(String::as_str)
                    == Some("https://staging.example.com")
        }));

        let entry = reopened
            .collections
            .iter()
            .find(|entry| entry.collection.name == "Petstore")
            .expect("imported collection");
        let animals = entry
            .folders()
            .iter()
            .find(|folder| folder.name == "Animals")
            .expect("root folder");
        let dogs = entry
            .folders()
            .iter()
            .find(|folder| folder.name == "Dogs")
            .expect("nested folder");
        assert_eq!(dogs.parent_id, Some(animals.id));

        let request = entry
            .requests
            .iter()
            .find(|request| request.name() == "List dogs")
            .expect("imported request");
        let RequestEntry::Loaded(spec) = request else {
            panic!("an imported request is loaded");
        };
        assert_eq!(spec.url, "https://api.example.com/dogs");
        assert_eq!(spec.folder_id, Some(dogs.id));
        assert_eq!(
            spec.headers,
            vec![KeyValue::new("Accept", "application/json")]
        );
        assert_eq!(spec.query.len(), 1);
        assert!(!spec.query[0].enabled, "the disabled flag is preserved");
    }

    #[test]
    fn importing_the_same_bundle_twice_suffixes_the_name_and_keeps_ids_apart() {
        let (_dir, mut state) = workspace();
        state.import_bundle(sample_bundle());
        let outcome = state.import_bundle(sample_bundle());
        assert_eq!(outcome.collections, 1);

        let names: Vec<&str> = state
            .collections
            .iter()
            .map(|entry| entry.collection.name.as_str())
            .collect();
        assert!(names.contains(&"Petstore"));
        assert!(names.contains(&"Petstore (2)"));

        let request_ids: HashSet<RequestId> = state
            .collections
            .iter()
            .flat_map(|entry| entry.requests.iter().map(|request| request.id()))
            .collect();
        assert_eq!(request_ids.len(), 2, "each import has its own request id");

        let environment_names: Vec<&str> = state
            .environments
            .iter()
            .map(|loaded| loaded.environment.name.as_str())
            .collect();
        assert!(environment_names.contains(&"staging (2)"));
    }

    #[test]
    fn an_import_does_not_change_the_active_environment() {
        let (_dir, mut state) = workspace();
        let first = state.create_environment("staging").expect("environment");
        assert!(state.set_active_environment(Some(first.slug.clone())));
        let active = state.workspace.active_environment.clone();

        let outcome = state.import_bundle(ImportBundle {
            collections: Vec::new(),
            environments: vec![Environment::new("staging")],
            warnings: Vec::new(),
        });

        assert_eq!(outcome.environments, 1);
        assert_eq!(state.workspace.active_environment, active);
        assert!(
            state
                .environments
                .iter()
                .any(|loaded| loaded.environment.name == "staging (2)")
        );
    }

    #[test]
    fn parse_warnings_and_write_skips_reach_the_report() {
        let (_dir, mut state) = workspace();
        let outcome = state.import_bundle(ImportBundle {
            collections: vec![ImportCollection {
                name: "Petstore".to_owned(),
                order: 0,
                folders: Vec::new(),
                requests: Vec::new(),
            }],
            environments: Vec::new(),
            warnings: vec![ImportWarning {
                item: "Petstore / request 2".to_owned(),
                reason: "the request could not be read".to_owned(),
            }],
        });

        assert_eq!(outcome.collections, 1);
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].item, "Petstore / request 2");
        assert!(outcome.skipped[0].reason.contains("could not be read"));
    }

    #[test]
    fn an_export_round_trips_back_into_an_equivalent_collection() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Petstore").expect("collection");
        let dogs = state
            .create_folder(&collection.id, None, "Dogs")
            .expect("folder");
        state
            .create_folder(&collection.id, Some(dogs.id), "Puppies")
            .expect("nested folder");

        let mut request = RequestSpec::new("Create dog");
        request.method = posthaste_core::model::HttpMethod::Post;
        request.url = "https://api.example.com/dogs".to_owned();
        request.folder_id = Some(dogs.id);
        request.headers = vec![KeyValue {
            key: "X-Debug".to_owned(),
            value: "1".to_owned(),
            enabled: false,
        }];
        request.body = posthaste_core::model::RequestBody::Json(r#"{"name":"Rex"}"#.to_owned());
        assert!(state.save_new_request(&collection.id, Some(dogs.id), &request));

        let json = state
            .export_collection_bundle(&collection.id)
            .expect("rendered bundle");
        assert_eq!(
            sniff("petstore.posthaste.json", json.as_bytes()),
            Some(ImportFormat::PosthasteBundle)
        );
        let bundle = parse(ImportFormat::PosthasteBundle, json.as_bytes()).expect("parsed bundle");
        assert_eq!(bundle.collections[0].name, "Petstore");
        assert_eq!(bundle.collections[0].requests.len(), 1);

        let target_dir = tempfile::tempdir().expect("second workspace");
        let mut target = WorkspaceState::load(WorkspaceStore::at(target_dir.path()));
        let outcome = target.import_bundle(bundle);
        assert_eq!(outcome.collections, 1);
        assert_eq!(outcome.folders, 2);
        assert_eq!(outcome.requests, 1);

        let entry = target
            .collections
            .iter()
            .find(|entry| entry.collection.name == "Petstore")
            .expect("re-imported collection");
        let dogs = entry
            .folders()
            .iter()
            .find(|folder| folder.name == "Dogs")
            .expect("folder");
        let puppies = entry
            .folders()
            .iter()
            .find(|folder| folder.name == "Puppies")
            .expect("nested folder");
        assert_eq!(puppies.parent_id, Some(dogs.id));

        let RequestEntry::Loaded(spec) = entry
            .requests
            .iter()
            .find(|request| request.name() == "Create dog")
            .expect("request")
        else {
            panic!("an imported request is loaded");
        };
        assert_eq!(spec.method, posthaste_core::model::HttpMethod::Post);
        assert_eq!(spec.folder_id, Some(dogs.id));
        assert_eq!(spec.headers[0].key, "X-Debug");
        assert!(!spec.headers[0].enabled);
        assert_eq!(
            spec.body,
            posthaste_core::model::RequestBody::Json(r#"{"name":"Rex"}"#.to_owned())
        );
    }

    #[test]
    fn a_websocket_request_round_trips_through_a_bundle() {
        let (_dir, mut state) = workspace();
        let collection = state.create_collection("Sockets").expect("collection");
        let mut request = RequestSpec::new("Feed");
        request.protocol = posthaste_core::model::RequestProtocol::WebSocket;
        request.url = "wss://api.example.com/feed".to_owned();
        request.subprotocols = vec!["graphql-ws".to_owned()];
        assert!(state.save_new_request(&collection.id, None, &request));

        let json = state
            .export_collection_bundle(&collection.id)
            .expect("rendered bundle");
        let bundle = parse(ImportFormat::PosthasteBundle, json.as_bytes()).expect("parsed bundle");

        let target_dir = tempfile::tempdir().expect("second workspace");
        let mut target = WorkspaceState::load(WorkspaceStore::at(target_dir.path()));
        let outcome = target.import_bundle(bundle);
        assert_eq!(outcome.requests, 1);

        let entry = target
            .collections
            .iter()
            .find(|entry| entry.collection.name == "Sockets")
            .expect("re-imported collection");
        let RequestEntry::Loaded(spec) = entry
            .requests
            .iter()
            .find(|request| request.name() == "Feed")
            .expect("request")
        else {
            panic!("an imported request is loaded");
        };
        assert_eq!(
            spec.protocol,
            posthaste_core::model::RequestProtocol::WebSocket
        );
        assert_eq!(spec.url, "wss://api.example.com/feed");
        assert_eq!(spec.subprotocols, vec!["graphql-ws".to_owned()]);
    }

    #[test]
    fn a_postman_import_suffixes_its_environment_on_a_second_import() {
        let (dir, mut state) = workspace();
        let document = r#"{
          "info": {
            "name": "Petstore",
            "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
          },
          "item": [],
          "variable": [{ "key": "base_url", "value": "https://api.example.com" }]
        }"#;

        let first = parse(ImportFormat::PostmanCollection, document.as_bytes())
            .expect("Postman fixture parses");
        let outcome = state.import_bundle(first);
        assert_eq!(outcome.collections, 1);
        assert_eq!(outcome.environments, 1);

        let second = parse(ImportFormat::PostmanCollection, document.as_bytes())
            .expect("Postman fixture parses twice");
        state.import_bundle(second);

        let names: Vec<&str> = state
            .environments
            .iter()
            .map(|loaded| loaded.environment.name.as_str())
            .collect();
        assert!(names.contains(&"Petstore"));
        assert!(names.contains(&"Petstore (2)"));

        // A fresh load sees the environment content that was written.
        let reopened = WorkspaceState::load(WorkspaceStore::at(dir.path()));
        assert!(reopened.environments.iter().any(|loaded| {
            loaded.environment.vars.get("base_url").map(String::as_str)
                == Some("https://api.example.com")
        }));
    }
}
