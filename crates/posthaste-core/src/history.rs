//! Request-history model and local file store.
//!
//! History is a bounded, append-only log of executed requests. Each entry is a
//! single JSON file in a machine-local directory keyed by the workspace root,
//! never inside the workspace itself: an entry holds response bytes, so every
//! send would otherwise dirty a workspace under version control. Entries store
//! the resolved request snapshot and a capped response body prefix; sensitive
//! header values are replaced before the entry is written.
//!
//! A workspace written by an earlier version kept `history/` in the workspace.
//! Loading moves those entries to the machine-local directory once, leaving
//! behind anything that cannot move and reporting it.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::binary::{SNIFF_LIMIT, classify};
use crate::model::{
    HttpMethod, KeyValue, RequestBody, RequestId, RequestProtocol, RequestSpec, ResponseCookie,
    ResponseRecord, SCHEMA_VERSION,
};
use crate::session::canonical_root;
use crate::storage::{
    DOCUMENT_EXTENSION, StorageError, StorageIssue, Versioned, cleanup_temporary_files,
    documents_by_stem, ensure_dir, io_error, issue_for, read_versioned, remove_document,
    write_document_atomic,
};
use crate::url_query::join_url_query;
use crate::variables::{Variables, resolve_request};

/// Maximum number of entries retained in history.
pub const HISTORY_CAP: usize = 500;

/// Maximum response body bytes stored for one history entry.
pub const HISTORY_BODY_LIMIT: usize = 512 * 1024;

/// Replacement written in place of a sensitive header value.
pub const REDACTED_VALUE: &str = "***";

const HISTORY_SCHEMA_VERSION: u32 = SCHEMA_VERSION;

const fn default_schema_version() -> u32 {
    HISTORY_SCHEMA_VERSION
}

/// Stable identifier for one history entry. Also used as its filename stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HistoryId(Uuid);

impl HistoryId {
    /// Generates a time-sortable UUID v7 identifier.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for HistoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for HistoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The resolved request fields that were sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSnapshot {
    pub method: HttpMethod,
    pub url: String,
    /// The transport the entry was recorded from. An entry written before this
    /// field existed replays as HTTP.
    #[serde(default)]
    pub protocol: RequestProtocol,
    #[serde(default)]
    pub subprotocols: Vec<String>,
    #[serde(default)]
    pub headers: Vec<KeyValue>,
    #[serde(default)]
    pub query: Vec<KeyValue>,
    #[serde(default)]
    pub body: RequestBody,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl RequestSnapshot {
    /// Copies the resolved fields from `spec`, excluding its identity.
    ///
    /// The recorded URL is what the row shows and what was sent: the base URL
    /// with its enabled query rows composed onto it. The rows are stored too,
    /// so a replay keeps the table it came from.
    pub fn from_spec(spec: &RequestSpec) -> Self {
        Self {
            method: spec.method,
            url: join_url_query(&spec.url, &spec.query),
            protocol: spec.protocol,
            subprotocols: spec.subprotocols.clone(),
            headers: spec.headers.clone(),
            query: spec.query.clone(),
            body: spec.body.clone(),
            timeout_ms: spec.timeout_ms,
        }
    }

    /// Builds a fresh draft request. Sensitive header rows are disabled so a
    /// redacted `***` placeholder can never be sent accidentally.
    pub fn replay_spec(&self) -> RequestSpec {
        let mut spec = RequestSpec::new("Replayed request");
        spec.method = self.method;
        spec.url = self.url.clone();
        spec.protocol = self.protocol;
        spec.subprotocols = self.subprotocols.clone();
        spec.headers = self.headers.clone();
        for header in &mut spec.headers {
            if is_sensitive_request_header(&header.key) {
                header.enabled = false;
            }
        }
        spec.query = self.query.clone();
        // Entries written before this change can carry the query in the URL
        // while also holding rows. Split it once so the replay sends each
        // parameter a single time.
        spec.normalize_query();
        spec.body = self.body.clone();
        spec.timeout_ms = self.timeout_ms;
        spec
    }

    /// Whether `spec`, resolved with `vars` and redacted like a history write,
    /// describes the same request as this snapshot.
    ///
    /// Name, identifier, and folder are ignored on purpose: only the fields a
    /// send actually uses decide whether the entry belongs to the request.
    pub fn matches_resolved(&self, spec: &RequestSpec, vars: &Variables) -> bool {
        let Ok(resolved) = resolve_request(spec, vars) else {
            return false;
        };
        let mut candidate = Self::from_spec(&resolved);
        redact_request_headers(&mut candidate.headers);
        self.normalized() == candidate.normalized()
    }

    /// The same snapshot with its URL and rows normalized, so an entry recorded
    /// before the two shared one surface still compares equal to today's
    /// requests.
    fn normalized(&self) -> Self {
        let mut probe = RequestSpec::new("snapshot");
        probe.url = self.url.clone();
        probe.query = self.query.clone();
        probe.normalize_query();
        Self {
            url: probe.url,
            query: probe.query,
            ..self.clone()
        }
    }
}

/// Returns whether a request header's value must be redacted in history.
pub fn is_sensitive_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie"
    )
}

/// Returns whether a response header's value must be redacted in history.
pub fn is_sensitive_response_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("set-cookie")
}

fn redact_request_headers(headers: &mut [KeyValue]) {
    for header in headers {
        if is_sensitive_request_header(&header.key) {
            header.value = REDACTED_VALUE.to_owned();
        }
    }
}

fn redact_response_headers(headers: &mut [(String, String)]) {
    for (name, value) in headers {
        if is_sensitive_response_header(name) {
            *value = REDACTED_VALUE.to_owned();
        }
    }
}

/// Metadata for a binary response kept in history. The bytes themselves are
/// never stored, so a history entry can describe a binary body but not replay
/// or save it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredBinary {
    #[serde(default)]
    pub media_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

/// A completed response as stored in history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub body_bytes: u64,
    #[serde(default)]
    pub body_limited: bool,
    #[serde(default)]
    pub truncated: bool,
    /// Metadata for a binary body. When present, `body` is empty: history
    /// keeps no binary bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<StoredBinary>,
    /// Metadata for the cookies the response set. Values are never kept; see
    /// [`Self::from_record`].
    #[serde(default)]
    pub cookies: Vec<ResponseCookie>,
}

impl StoredResponse {
    /// Captures the status, redacted headers, and at most the first 512 KiB of
    /// the retained body as lossy UTF-8. A binary body is recorded as metadata
    /// only, so no bytes are written to history.
    pub fn from_record(record: &ResponseRecord) -> Self {
        let mut headers = record.headers.clone();
        redact_response_headers(&mut headers);
        let binary = classify(
            &headers,
            &record.body[..record.body.len().min(SNIFF_LIMIT)],
            record.body.len(),
        );
        let (body, body_limited) = if binary.is_some() {
            (String::new(), false)
        } else {
            let stored_len = record.body.len().min(HISTORY_BODY_LIMIT);
            (
                String::from_utf8_lossy(&record.body[..stored_len]).into_owned(),
                record.body.len() > HISTORY_BODY_LIMIT,
            )
        };
        // A cookie value is exactly what the jar requirement keeps out of
        // workspace documents, so only the attributes are recorded.
        let cookies = record
            .cookies
            .iter()
            .map(|cookie| ResponseCookie {
                value: None,
                ..cookie.clone()
            })
            .collect();
        Self {
            status: record.status,
            headers,
            body,
            body_bytes: record.body.len() as u64,
            body_limited,
            truncated: record.truncated,
            binary: binary.map(|meta| StoredBinary {
                media_type: meta.media_type,
                file_name: meta.file_name,
            }),
            cookies,
        }
    }

    /// Captures a WebSocket handshake response: its status and redacted
    /// headers, with no body. `set-cookie` values are stripped exactly as they
    /// are for an HTTP response, so a session entry never keeps a cookie value.
    pub fn from_handshake(status: u16, headers: Vec<(String, String)>) -> Self {
        let mut headers = headers;
        redact_response_headers(&mut headers);
        Self {
            status,
            headers,
            body: String::new(),
            body_bytes: 0,
            body_limited: false,
            truncated: false,
            binary: None,
            cookies: Vec::new(),
        }
    }
}

/// How a session ended, as stored in history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SessionEnd {
    /// A closing handshake completed, from either side.
    Closed {
        by: crate::ws::WsCloseBy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The connection ended without a closing handshake.
    Abnormal { message: String },
    /// The user cancelled the attempt before it ended.
    Cancelled,
}

/// A WebSocket session as stored in history.
///
/// It keeps the handshake response (status, redacted headers, cookie
/// attributes), the end, and frame counts. Frame payloads are never stored, so
/// a history entry describes a session without replaying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake: Option<StoredResponse>,
    pub end: SessionEnd,
    #[serde(default)]
    pub frames_sent: u64,
    #[serde(default)]
    pub frames_received: u64,
    #[serde(default)]
    pub bytes_sent: u64,
    #[serde(default)]
    pub bytes_received: u64,
}

/// How an executed request ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum HistoryOutcome {
    Response(StoredResponse),
    Session(StoredSession),
    Failed { message: String },
    Cancelled,
}

impl HistoryOutcome {
    /// Short label for a history row.
    pub fn label(&self) -> String {
        match self {
            Self::Response(response) => response.status.to_string(),
            Self::Session(session) => match &session.end {
                SessionEnd::Closed {
                    code: Some(code), ..
                } => code.to_string(),
                SessionEnd::Closed { code: None, .. } => "Closed".to_owned(),
                SessionEnd::Abnormal { .. } => "Failed".to_owned(),
                SessionEnd::Cancelled => "Cancelled".to_owned(),
            },
            Self::Failed { .. } => "Failed".to_owned(),
            Self::Cancelled => "Cancelled".to_owned(),
        }
    }

    pub fn response(&self) -> Option<&StoredResponse> {
        match self {
            Self::Response(response) => Some(response),
            _ => None,
        }
    }

    /// The handshake a session presented, for replaying its status and headers.
    pub fn handshake(&self) -> Option<&StoredResponse> {
        match self {
            Self::Session(session) => session.handshake.as_ref(),
            _ => None,
        }
    }

    pub fn session(&self) -> Option<&StoredSession> {
        match self {
            Self::Session(session) => Some(session),
            _ => None,
        }
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Failed { message } => Some(message),
            Self::Session(StoredSession {
                end: SessionEnd::Abnormal { message },
                ..
            }) => Some(message),
            _ => None,
        }
    }
}

/// One recorded request execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub id: HistoryId,
    /// Unix epoch milliseconds when the request started.
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    pub request: RequestSnapshot,
    /// The saved request this execution was sent from, when it had one.
    ///
    /// Older entries and drafts that were not saved carry no source; the field
    /// is omitted from disk in that case so their files stay unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_request_id: Option<RequestId>,
    pub outcome: HistoryOutcome,
}

impl HistoryEntry {
    /// Creates an entry, redacting sensitive request headers in the snapshot.
    pub fn new(
        timestamp_ms: u64,
        duration_ms: u64,
        spec: &RequestSpec,
        outcome: HistoryOutcome,
    ) -> Self {
        let mut request = RequestSnapshot::from_spec(spec);
        redact_request_headers(&mut request.headers);
        Self {
            schema_version: HISTORY_SCHEMA_VERSION,
            id: HistoryId::new(),
            timestamp_ms,
            duration_ms,
            request,
            source_request_id: None,
            outcome,
        }
    }

    /// Records the saved request the execution was sent from. `None` (an
    /// unsaved draft or a replay) leaves the entry unlinked.
    pub fn with_source(mut self, source: Option<RequestId>) -> Self {
        self.source_request_id = source;
        self
    }

    pub fn completed(timestamp_ms: u64, spec: &RequestSpec, response: &ResponseRecord) -> Self {
        let duration_ms = u64::try_from(response.duration.as_millis()).unwrap_or(u64::MAX);
        Self::new(
            timestamp_ms,
            duration_ms,
            spec,
            HistoryOutcome::Response(StoredResponse::from_record(response)),
        )
    }

    pub fn failed(
        timestamp_ms: u64,
        duration_ms: u64,
        spec: &RequestSpec,
        message: String,
    ) -> Self {
        Self::new(
            timestamp_ms,
            duration_ms,
            spec,
            HistoryOutcome::Failed { message },
        )
    }

    pub fn cancelled(timestamp_ms: u64, duration_ms: u64, spec: &RequestSpec) -> Self {
        Self::new(timestamp_ms, duration_ms, spec, HistoryOutcome::Cancelled)
    }

    /// Records a WebSocket session. The summary is built from events the
    /// transport reported, never from frame payloads.
    pub fn session(
        timestamp_ms: u64,
        duration_ms: u64,
        spec: &RequestSpec,
        session: StoredSession,
    ) -> Self {
        Self::new(
            timestamp_ms,
            duration_ms,
            spec,
            HistoryOutcome::Session(session),
        )
    }
}

impl Versioned for HistoryEntry {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

/// The result of loading the history directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedHistory {
    /// Entries sorted by timestamp descending, then id descending.
    pub entries: Vec<HistoryEntry>,
    /// Per-file problems; affected entries are absent from `entries`.
    pub issues: Vec<StorageIssue>,
}

/// Directory below the platform data directory that holds history, relative to
/// the Posthaste root.
const HISTORY_DIR: &str = "history";

/// The directory name an earlier version kept inside the workspace.
const LEGACY_HISTORY_DIR: &str = "history";

/// The machine-local history directory for one workspace.
///
/// The path mirrors the session document: a UUID v5 of the workspace's
/// canonical root under `Posthaste/history/`. Because the directory is keyed by
/// the root, moving a workspace keeps its history separate from the ones
/// already recorded, and two workspaces never share entries.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    root: PathBuf,
    /// The workspace whose in-workspace `history/` directory is migrated when
    /// this store loads. `None` for a store placed directly at a path, which
    /// is how tests keep everything inside their own temporary directory.
    legacy_root: Option<PathBuf>,
}

impl HistoryStore {
    /// Resolves the machine-local directory for `workspace_root`, or `None`
    /// when the platform has no per-user data directory.
    pub fn resolve(workspace_root: &Path) -> Option<Self> {
        Self::resolve_in(workspace_root, crate::paths::home())
    }

    /// The injectable form of [`Self::resolve`], taking the already-resolved
    /// product directory under the platform data directory.
    pub(crate) fn resolve_in(workspace_root: &Path, data_home: Option<PathBuf>) -> Option<Self> {
        let data_home = data_home?;
        let key = canonical_root(workspace_root);
        let name = Uuid::new_v5(&Uuid::NAMESPACE_URL, key.as_bytes());
        Some(Self {
            root: data_home
                .join(HISTORY_DIR)
                .join(format!("workspace-{name}")),
            legacy_root: Some(workspace_root.to_path_buf()),
        })
    }

    /// A store rooted at `directory` with no legacy directory to migrate.
    pub fn at(directory: impl Into<PathBuf>) -> Self {
        Self {
            root: directory.into(),
            legacy_root: None,
        }
    }

    /// A store at `directory` that also migrates the entries in
    /// `workspace_root/history`.
    ///
    /// Tests use it to exercise migration inside a `TempDir`; production uses
    /// [`Self::resolve`] instead, which derives both paths.
    pub fn at_with_legacy(
        directory: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            root: directory.into(),
            legacy_root: Some(workspace_root.into()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The in-workspace directory an earlier version wrote, when this store
    /// knows its workspace.
    fn legacy_dir(&self) -> Option<PathBuf> {
        self.legacy_root
            .as_ref()
            .map(|root| root.join(LEGACY_HISTORY_DIR))
    }

    pub fn entry_file(&self, id: HistoryId) -> PathBuf {
        self.root.join(format!("{id}.{DOCUMENT_EXTENSION}"))
    }

    /// Loads every valid entry, creating the directory when missing.
    ///
    /// A legacy in-workspace directory is migrated first, so the entries it
    /// held are part of this load.
    pub fn load(&self) -> Result<LoadedHistory, StorageError> {
        ensure_dir(&self.root)?;
        cleanup_temporary_files(&self.root);

        let mut issues = Vec::new();
        if let Some(legacy) = self.legacy_dir() {
            self.migrate(&legacy, &mut issues);
        }

        let read_dir = fs::read_dir(&self.root).map_err(|error| io_error(&self.root, error))?;
        let mut entries = Vec::new();
        for (_, path) in documents_by_stem(read_dir, &self.root, "history", &mut issues) {
            match read_versioned::<HistoryEntry>(&path) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    if let Some(issue) = StorageIssue::from_error(&error) {
                        issues.push(issue);
                    }
                }
            }
        }
        entries.sort_by(|a, b| {
            b.timestamp_ms
                .cmp(&a.timestamp_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(LoadedHistory { entries, issues })
    }

    /// Moves the entries of a legacy in-workspace `history/` directory into
    /// this store.
    ///
    /// Per file and never overwriting: a name already present at the
    /// destination is left where it is and reported. The legacy directory is
    /// removed only once it is empty, so a partial migration stays visible
    /// instead of losing an entry.
    fn migrate(&self, legacy: &Path, issues: &mut Vec<StorageIssue>) {
        if !legacy.is_dir() {
            return;
        }
        // Interrupted atomic writes of the old version can leave temp files
        // here; they are residue, not entries, and would otherwise keep the
        // directory alive forever.
        cleanup_temporary_files(legacy);
        let read_dir = match fs::read_dir(legacy) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                issues.push(issue_for(
                    legacy,
                    format!("request history was not migrated: {error}"),
                ));
                return;
            }
        };
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    issues.push(issue_for(legacy, error.to_string()));
                    continue;
                }
            };
            let path = entry.path();
            let is_entry = path.is_file()
                && path.extension().and_then(|extension| extension.to_str())
                    == Some(DOCUMENT_EXTENSION);
            if !is_entry {
                issues.push(issue_for(
                    &path,
                    "left in the workspace: not a request-history entry",
                ));
                continue;
            }
            let destination = self.root.join(entry.file_name());
            if destination.exists() {
                issues.push(issue_for(
                    &path,
                    format!(
                        "left in the workspace: {} already exists",
                        destination.display()
                    ),
                ));
                continue;
            }
            if let Err(error) = move_file(&path, &destination) {
                issues.push(issue_for(
                    &path,
                    format!("request-history entry was not moved: {error}"),
                ));
            }
        }
        // Anything left behind keeps the directory; only a fully migrated one
        // disappears.
        match fs::remove_dir(legacy) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) => {}
            Err(error) => issues.push(issue_for(legacy, error.to_string())),
        }
    }

    /// Writes one entry atomically as JSON.
    pub fn append(&self, entry: &HistoryEntry) -> Result<(), StorageError> {
        write_document_atomic(&self.entry_file(entry.id), entry).map(|_| ())
    }

    /// Deletes one entry; a file that is already gone is treated as success.
    pub fn remove(&self, id: HistoryId) -> Result<(), StorageError> {
        remove_document(&self.entry_file(id))
    }

    /// Deletes every history entry. Other files in
    /// the directory are left alone, and the first removal failure is returned
    /// after attempting the rest.
    pub fn clear(&self) -> Result<(), StorageError> {
        if !self.root.is_dir() {
            return Ok(());
        }
        let read_dir = fs::read_dir(&self.root).map_err(|error| io_error(&self.root, error))?;
        let mut first_error = None;
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    first_error.get_or_insert((self.root.clone(), error));
                    continue;
                }
            };
            let path = entry.path();
            let is_entry = path.extension().and_then(|extension| extension.to_str())
                == Some(DOCUMENT_EXTENSION);
            if !path.is_file() || !is_entry {
                continue;
            }
            if let Err(error) = fs::remove_file(&path) {
                first_error.get_or_insert((path, error));
            }
        }
        match first_error {
            Some((path, error)) => Err(io_error(&path, error)),
            None => Ok(()),
        }
    }
}

impl HistoryEntry {
    /// Duration in milliseconds for display and storage.
    pub fn duration(&self) -> Duration {
        Duration::from_millis(self.duration_ms)
    }
}

/// Moves one file, falling back to copy-then-delete.
///
/// A rename cannot cross volumes, and a workspace on another drive than the
/// user profile is the normal Windows case.
fn move_file(source: &Path, destination: &Path) -> Result<(), StorageError> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(source, destination).map_err(|error| io_error(source, error))?;
            fs::remove_file(source).map_err(|error| io_error(source, error))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiKeyLocation, RequestAuth};
    use bytes::Bytes;
    use std::collections::BTreeMap;

    fn spec() -> RequestSpec {
        let mut spec = RequestSpec::new("History sample");
        spec.method = HttpMethod::Post;
        spec.url = "https://example.test/users".to_owned();
        spec.headers = vec![
            KeyValue::new("Content-Type", "application/json"),
            KeyValue::new("Authorization", "Bearer secret"),
            KeyValue::new("cookie", "session=abc"),
            KeyValue::new("X-Trace", "keep-me"),
        ];
        spec.query = vec![KeyValue::new("page", "2")];
        spec.body = RequestBody::Json(r#"{"name":"posthaste"}"#.to_owned());
        spec.timeout_ms = Some(1500);
        spec
    }

    fn record(body: &[u8], truncated: bool) -> ResponseRecord {
        ResponseRecord {
            status: 201,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("Set-Cookie".into(), "session=secret".into()),
            ],
            body: Bytes::copy_from_slice(body),
            duration: Duration::from_millis(42),
            truncated,
            cookies: Vec::new(),
        }
    }

    fn entry(timestamp_ms: u64) -> HistoryEntry {
        HistoryEntry::completed(timestamp_ms, &spec(), &record(br#"{"ok":true}"#, false))
    }

    #[test]
    fn entry_round_trips_through_json() {
        let entry = entry(1_700_000_000_000);
        let json = serde_json::to_string(&entry).expect("serialize");
        let parsed: HistoryEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed, entry);
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
        assert_eq!(parsed.request.method, HttpMethod::Post);
        assert_eq!(parsed.request.timeout_ms, Some(1500));
        assert_eq!(parsed.duration_ms, 42);
    }

    fn vars() -> Variables {
        Variables::defaults_then(
            &BTreeMap::from([("host".to_owned(), "example.test".to_owned())]),
            None,
        )
    }

    #[test]
    fn source_request_id_round_trips_and_stays_optional() {
        let source = RequestId::new();
        let linked = entry(1).with_source(Some(source));
        let json = serde_json::to_string(&linked).expect("serialize");
        let parsed: HistoryEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.source_request_id, Some(source));

        let json = serde_json::to_string(&entry(2)).expect("serialize");
        assert!(!json.contains("source_request_id"));
        let parsed: HistoryEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.source_request_id, None);
    }

    #[test]
    fn a_stored_response_without_cookie_metadata_still_loads() {
        let stored = StoredResponse::from_record(&record(b"{}", false));
        let mut document = serde_json::to_value(&stored).expect("encode the response");
        let removed = document
            .as_object_mut()
            .expect("a response is an object")
            .remove("cookies");
        assert!(removed.is_some(), "this test removes the new field");

        let loaded: StoredResponse =
            serde_json::from_value(document).expect("an older response loads");
        assert!(loaded.cookies.is_empty());
        assert_eq!(loaded.status, stored.status);
        assert_eq!(loaded.body, stored.body);
    }

    fn session() -> StoredSession {
        StoredSession {
            handshake: Some(StoredResponse::from_handshake(
                101,
                vec![
                    ("upgrade".to_owned(), "websocket".to_owned()),
                    ("Set-Cookie".to_owned(), "session=secret".to_owned()),
                ],
            )),
            end: SessionEnd::Closed {
                by: crate::ws::WsCloseBy::Server,
                code: Some(1001),
                reason: Some("going away".to_owned()),
            },
            frames_sent: 2,
            frames_received: 3,
            bytes_sent: 20,
            bytes_received: 30,
        }
    }

    #[test]
    fn a_session_entry_round_trips_without_payloads() {
        let mut websocket = spec();
        websocket.protocol = RequestProtocol::WebSocket;
        websocket.url = "wss://example.test/socket".to_owned();
        websocket.subprotocols = vec!["graphql-ws".to_owned()];
        let entry = HistoryEntry::session(10, 500, &websocket, session());

        let json = serde_json::to_string(&entry).expect("serialize");
        let parsed: HistoryEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed, entry);
        assert_eq!(parsed.request.protocol, RequestProtocol::WebSocket);
        assert_eq!(parsed.request.subprotocols, vec!["graphql-ws".to_owned()]);
        assert!(!json.contains("payload"), "no frame payload is serialized");
        assert!(
            !json.contains("session=secret"),
            "the handshake's cookie value is redacted"
        );
    }

    #[test]
    fn session_labels_name_their_end() {
        let mut websocket = spec();
        websocket.protocol = RequestProtocol::WebSocket;
        let closed = HistoryEntry::session(10, 500, &websocket, session());
        assert_eq!(closed.outcome.label(), "1001", "a close code names the end");

        let mut no_code = session();
        no_code.end = SessionEnd::Closed {
            by: crate::ws::WsCloseBy::User,
            code: None,
            reason: None,
        };
        let entry = HistoryEntry::session(10, 500, &websocket, no_code);
        assert_eq!(entry.outcome.label(), "Closed");

        let mut abnormal = session();
        abnormal.end = SessionEnd::Abnormal {
            message: "connection reset".to_owned(),
        };
        let entry = HistoryEntry::session(10, 500, &websocket, abnormal);
        assert_eq!(entry.outcome.label(), "Failed");
        assert_eq!(entry.outcome.error_message(), Some("connection reset"));
        assert_eq!(entry.outcome.session().map(|s| s.frames_received), Some(3));
    }

    #[test]
    fn a_websocket_snapshot_replays_as_websocket() {
        let mut websocket = spec();
        websocket.protocol = RequestProtocol::WebSocket;
        websocket.url = "wss://example.test/socket".to_owned();
        websocket.subprotocols = vec!["graphql-ws".to_owned(), " json ".to_owned()];

        let snapshot = RequestSnapshot::from_spec(&websocket);
        let replayed = snapshot.replay_spec();

        assert_eq!(replayed.protocol, RequestProtocol::WebSocket);
        assert_eq!(replayed.subprotocols, websocket.subprotocols);
    }

    #[test]
    fn a_websocket_snapshot_does_not_match_the_same_http_request() {
        let mut websocket = spec();
        websocket.protocol = RequestProtocol::WebSocket;
        websocket.url = "wss://example.test/socket".to_owned();
        // A stored entry redacts credentials, so leave them out of the
        // comparison and let the protocol be the only difference.
        websocket.headers.clear();
        let snapshot = RequestSnapshot::from_spec(&websocket);

        let mut same_shape = websocket.clone();
        same_shape.protocol = RequestProtocol::Http;

        assert!(!snapshot.matches_resolved(&same_shape, &Variables::default()));
        assert!(snapshot.matches_resolved(&websocket, &Variables::default()));
    }

    #[test]
    fn a_binary_response_is_stored_as_metadata_only() {
        let mut response = record(&[0x89, b'P', b'N', b'G', 0, 1, 2, 3], false);
        response.headers[0] = ("content-type".into(), "image/png".into());
        response.headers.push((
            "content-disposition".into(),
            "attachment; filename=\"plot.png\"".into(),
        ));

        let stored = StoredResponse::from_record(&response);

        assert!(stored.body.is_empty(), "no binary bytes enter history");
        assert!(!stored.body_limited);
        assert_eq!(stored.body_bytes, 8, "the retained size is recorded");
        let binary = stored.binary.clone().expect("binary metadata");
        assert_eq!(binary.media_type.as_deref(), Some("image/png"));
        assert_eq!(binary.file_name.as_deref(), Some("plot.png"));

        let json = serde_json::to_string(&stored).expect("serialize");
        assert!(json.contains("\"binary\""));
        assert!(!json.contains("0,1,2,3"), "the entry holds no body bytes");
    }

    #[test]
    fn a_response_written_before_binary_metadata_existed_still_loads() {
        let loaded: StoredResponse = serde_json::from_value(serde_json::json!({
            "status": 200,
            "headers": [],
            "body": "ok",
            "body_bytes": 2
        }))
        .expect("an older response loads");

        assert_eq!(loaded.binary, None);
        assert_eq!(loaded.body, "ok");
        assert!(!loaded.body_limited);
        assert!(!loaded.truncated);
    }

    #[test]
    fn stored_cookie_metadata_round_trips_without_values() {
        const VALUE: &str = "super-secret-value";
        let mut response = record(b"{}", false);
        response.cookies = vec![ResponseCookie {
            name: "session".to_owned(),
            value: Some(VALUE.to_owned()),
            domain: "example.test".to_owned(),
            host_only: false,
            path: "/app".to_owned(),
            expires_at_ms: Some(1_700_000_000_000),
            secure: true,
            http_only: true,
            same_site: Some("Lax".to_owned()),
            stored: true,
        }];
        let entry = HistoryEntry::completed(10, &spec(), &response);

        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(
            !json.contains(VALUE),
            "a document never keeps a cookie value"
        );

        let loaded: HistoryEntry = serde_json::from_str(&json).expect("deserialize");
        let cookies = &loaded
            .outcome
            .response()
            .expect("a completed response")
            .cookies;
        assert_eq!(cookies.len(), 1);
        let cookie = &cookies[0];
        assert_eq!(cookie.name, "session");
        assert_eq!(cookie.value, None);
        assert_eq!(cookie.domain, "example.test");
        assert!(!cookie.host_only);
        assert_eq!(cookie.path, "/app");
        assert_eq!(cookie.expires_at_ms, Some(1_700_000_000_000));
        assert!(cookie.secure);
        assert!(cookie.http_only);
        assert_eq!(cookie.same_site.as_deref(), Some("Lax"));
        assert!(cookie.stored);
    }

    #[test]
    fn matching_ignores_name_identifier_and_folder() {
        let entry = entry(1);
        let mut renamed = spec();
        renamed.id = RequestId::new();
        renamed.name = "Renamed".to_owned();
        renamed.folder_id = Some(crate::model::FolderId::new());

        assert!(entry.request.matches_resolved(&spec(), &vars()));
        assert!(entry.request.matches_resolved(&renamed, &vars()));
    }

    #[test]
    fn matching_resolves_placeholders_before_comparing() {
        let mut templated = spec();
        templated.url = "https://{{host}}/users".to_owned();

        assert!(entry(1).request.matches_resolved(&templated, &vars()));

        templated.url = "https://{{missing}}/users".to_owned();
        assert!(!entry(1).request.matches_resolved(&templated, &vars()));
    }

    #[test]
    fn matching_redacts_sensitive_values_on_both_sides() {
        let entry = entry(1);
        let mut rotated = spec();
        rotated.headers[1].value = "Bearer rotated".to_owned();
        rotated.headers[2].value = "session=def".to_owned();

        // Both sides store `***`, so the value itself cannot break the link.
        assert!(entry.request.matches_resolved(&rotated, &vars()));
    }

    #[test]
    fn matching_detects_any_changed_sent_field() {
        let entry = entry(1);
        let mut changes: Vec<(&str, RequestSpec)> = Vec::new();

        let mut method = spec();
        method.method = HttpMethod::Get;
        changes.push(("method", method));

        let mut url = spec();
        url.url.push_str("/2");
        changes.push(("url", url));

        let mut header = spec();
        header.headers[0].value = "text/plain".to_owned();
        changes.push(("header value", header));

        let mut disabled = spec();
        disabled.headers[3].enabled = false;
        changes.push(("header enabled", disabled));

        let mut reordered = spec();
        reordered.headers.swap(0, 1);
        changes.push(("header order", reordered));

        let mut query = spec();
        query.query[0].value = "3".to_owned();
        changes.push(("query", query));

        let mut body = spec();
        body.body = RequestBody::Json("{}".to_owned());
        changes.push(("body", body));

        let mut timeout = spec();
        timeout.timeout_ms = None;
        changes.push(("timeout", timeout));

        for (label, candidate) in changes {
            assert!(
                !entry.request.matches_resolved(&candidate, &vars()),
                "{label} should not match"
            );
        }
    }

    #[test]
    fn a_snapshot_records_the_composed_url() {
        let mut spec = spec();
        spec.query = vec![KeyValue::new("page", "2"), KeyValue::new("debug", "on")];
        spec.query[1].enabled = false;

        let snapshot = RequestSnapshot::from_spec(&spec);

        assert_eq!(snapshot.url, "https://example.test/users?page=2");
        assert_eq!(snapshot.query, spec.query, "the rows are kept for replay");
    }

    #[test]
    fn a_query_snapshot_does_not_record_api_key_credentials() {
        // The composed URL carries only the request's own rows; a query-located
        // API key is added at send time and never enters the snapshot.
        let mut spec = spec();
        spec.auth = RequestAuth::ApiKey {
            key: "api_key".to_owned(),
            value: "secret".to_owned(),
            location: ApiKeyLocation::Query,
        };

        let snapshot = RequestSnapshot::from_spec(&spec);

        assert_eq!(snapshot.url, "https://example.test/users?page=2");
    }

    #[test]
    fn replaying_a_new_entry_does_not_duplicate_its_rows() {
        let mut spec = spec();
        spec.query = vec![KeyValue::new("page", "2")];
        let snapshot = RequestSnapshot::from_spec(&spec);

        let replayed = snapshot.replay_spec();

        assert_eq!(replayed.url, "https://example.test/users");
        assert_eq!(replayed.query, vec![KeyValue::new("page", "2")]);
    }

    #[test]
    fn replaying_a_legacy_entry_splits_its_url_query() {
        let legacy = RequestSnapshot {
            url: "https://example.test/users?page=2".to_owned(),
            query: vec![KeyValue::new("page", "2"), KeyValue::new("debug", "on")],
            ..RequestSnapshot::from_spec(&spec())
        };

        let replayed = legacy.replay_spec();

        assert_eq!(replayed.url, "https://example.test/users");
        assert_eq!(
            replayed.query,
            vec![KeyValue::new("page", "2"), KeyValue::new("debug", "on")],
            "the URL's pair fills the place its stored row had"
        );
    }

    #[test]
    fn a_legacy_entry_links_to_its_normalized_request() {
        let mut legacy = entry(1).request;
        legacy.url = "https://example.test/users?page=2".to_owned();
        legacy.query = Vec::new();

        assert!(legacy.matches_resolved(&spec(), &vars()));
    }

    #[test]
    fn a_legacy_entry_still_notices_a_changed_value() {
        let legacy = RequestSnapshot {
            url: "https://example.test/users?page=2".to_owned(),
            query: Vec::new(),
            ..RequestSnapshot::from_spec(&spec())
        };
        let mut changed = spec();
        changed.query[0].value = "3".to_owned();

        assert!(!legacy.matches_resolved(&changed, &vars()));
    }

    #[test]
    fn response_body_is_capped_at_512_kib() {
        let body = vec![b'a'; HISTORY_BODY_LIMIT + 10];
        let stored = StoredResponse::from_record(&record(&body, false));

        assert_eq!(stored.body.len(), HISTORY_BODY_LIMIT);
        assert_eq!(stored.body_bytes, (HISTORY_BODY_LIMIT + 10) as u64);
        assert!(stored.body_limited);
        assert!(!stored.truncated);
    }

    #[test]
    fn response_truncation_is_preserved() {
        let stored = StoredResponse::from_record(&record(b"partial", true));

        assert!(!stored.body_limited);
        assert!(stored.truncated);
    }

    #[test]
    fn sensitive_headers_are_redacted_case_insensitively() {
        let entry = entry(1);
        let values: Vec<(&str, &str)> = entry
            .request
            .headers
            .iter()
            .map(|header| (header.key.as_str(), header.value.as_str()))
            .collect();

        assert!(values.contains(&("Authorization", REDACTED_VALUE)));
        assert!(values.contains(&("cookie", REDACTED_VALUE)));
        assert!(values.contains(&("X-Trace", "keep-me")));
        let response = entry.outcome.response().expect("response");
        assert!(
            response
                .headers
                .contains(&("Set-Cookie".to_owned(), REDACTED_VALUE.to_owned()))
        );
        assert!(
            response
                .headers
                .contains(&("content-type".to_owned(), "application/json".to_owned()))
        );
    }

    #[test]
    fn replay_disables_redacted_sensitive_rows() {
        let entry = entry(1);
        let replay = entry.request.replay_spec();

        let authorization = replay
            .headers
            .iter()
            .find(|header| header.key == "Authorization")
            .expect("authorization row");
        assert_eq!(authorization.value, REDACTED_VALUE);
        assert!(!authorization.enabled);
        assert!(
            replay
                .headers
                .iter()
                .find(|header| header.key == "X-Trace")
                .expect("trace row")
                .enabled
        );
        assert_eq!(replay.body, entry.request.body);
        assert_eq!(replay.timeout_ms, Some(1500));
    }

    #[test]
    fn fresh_replays_get_new_identities() {
        let entry = entry(1);
        let first = entry.request.replay_spec();
        let second = entry.request.replay_spec();

        assert_ne!(first.id, second.id);
    }

    #[test]
    fn load_creates_the_directory_and_sorts_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::at(dir.path().join("history"));
        assert!(!store.root().exists());

        for timestamp in [10, 30, 20] {
            store.append(&entry(timestamp)).expect("append");
        }
        let loaded = store.load().expect("load");

        assert!(store.root().is_dir());
        assert!(loaded.issues.is_empty());
        assert_eq!(
            loaded
                .entries
                .iter()
                .map(|entry| entry.timestamp_ms)
                .collect::<Vec<_>>(),
            vec![30, 20, 10]
        );
    }

    #[test]
    fn corrupt_and_newer_entries_are_reported_and_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::at(dir.path().join("history"));
        store.append(&entry(10)).expect("append");

        fs::write(store.root().join("corrupt.json"), "not: [valid").expect("corrupt file");
        let mut newer = entry(20);
        newer.schema_version = SCHEMA_VERSION + 1;
        fs::write(
            store.root().join("newer.json"),
            serde_json::to_string(&newer).expect("serialize"),
        )
        .expect("newer file");

        let loaded = store.load().expect("load");

        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.issues.len(), 2);
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path.ends_with("corrupt.json")
                    && issue.message.contains("invalid JSON"))
        );
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path.ends_with("newer.json"))
        );
    }

    #[test]
    fn append_uses_uuid_filenames_and_remove_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::at(dir.path().join("history"));
        let entry = entry(10);

        store.append(&entry).expect("append");
        let path = store.entry_file(entry.id);
        assert!(path.is_file());
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("json")
        );
        assert!(Uuid::parse_str(path.file_stem().unwrap().to_str().unwrap()).is_ok());

        store.remove(entry.id).expect("remove");
        assert!(!path.exists());
        store.remove(entry.id).expect("remove missing is ok");
    }

    #[test]
    fn clear_removes_only_history_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::at(dir.path().join("history"));
        let entry = entry(10);
        store.append(&entry).expect("append");
        fs::write(store.root().join("keep.txt"), "not history").expect("keep file");

        let collections = dir.path().join("collections");
        fs::create_dir_all(&collections).expect("collections dir");
        fs::write(collections.join("keep.bin"), "workspace data").expect("collection file");

        store.clear().expect("clear");

        assert!(!store.entry_file(entry.id).exists());
        assert!(store.root().join("keep.txt").is_file());
        assert!(collections.join("keep.bin").is_file());
        assert!(store.load().expect("load").entries.is_empty());
    }

    #[test]
    fn the_same_root_maps_to_the_same_history_directory_and_two_roots_differ() {
        let first = tempfile::tempdir().expect("first root");
        let second = tempfile::tempdir().expect("second root");
        let data = tempfile::tempdir().expect("data dir");

        let store = HistoryStore::resolve_in(
            first.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        )
        .expect("store");
        let again = HistoryStore::resolve_in(
            first.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        )
        .expect("store");
        let other = HistoryStore::resolve_in(
            second.path(),
            crate::paths::home_in(Some(data.path().to_path_buf())),
        )
        .expect("store");

        assert_eq!(
            store.root(),
            again.root(),
            "one workspace keeps one history"
        );
        assert_ne!(store.root(), other.root(), "another root gets its own");
        assert!(
            store
                .root()
                .starts_with(data.path().join("Posthaste").join("history"))
        );
        assert!(
            store
                .root()
                .file_name()
                .expect("directory name")
                .to_string_lossy()
                .starts_with("workspace-")
        );
        assert!(
            !store.root().starts_with(first.path()),
            "history never lives under the workspace root"
        );
        assert!(
            HistoryStore::resolve_in(first.path(), None).is_none(),
            "no data directory means no machine-local history"
        );
    }

    /// The old layout kept `history/` inside the workspace. Loading a resolved
    /// store moves those entries out, once, and removes the directory.
    #[test]
    fn loading_removes_leftover_temporary_files_and_keeps_the_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HistoryStore::at(dir.path().join("history"));
        let kept = entry(10);
        store.append(&kept).expect("entry");
        let root = store.root().to_path_buf();
        let residue = root.join(".posthaste-abc123.posthaste-tmp");
        fs::write(&residue, "{").expect("write residue");
        let unrelated = root.join("notes.txt");
        fs::write(&unrelated, "keep me").expect("write unrelated file");

        let loaded = store.load().expect("load");

        assert_eq!(loaded.entries, vec![kept]);
        assert!(
            !residue.exists(),
            "residue from an interrupted write is removed"
        );
        assert_eq!(
            fs::read_to_string(&unrelated).expect("unrelated file"),
            "keep me"
        );
    }

    #[test]
    fn a_legacy_history_directory_is_migrated_and_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let legacy = workspace.join("history");
        fs::create_dir_all(&legacy).expect("legacy dir");
        let store = HistoryStore::at_with_legacy(dir.path().join("history"), &workspace);
        let entry = entry(10);
        fs::write(
            legacy.join(format!("{}.json", entry.id)),
            serde_json::to_vec(&entry).expect("encode entry"),
        )
        .expect("write legacy entry");

        let loaded = store.load().expect("load");

        assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
        assert_eq!(loaded.entries, vec![entry.clone()]);
        assert!(store.entry_file(entry.id).is_file());
        assert!(!legacy.exists(), "a fully migrated directory is removed");
    }

    #[test]
    fn a_duplicate_at_the_destination_is_left_in_place_with_an_issue() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let legacy = workspace.join("history");
        fs::create_dir_all(&legacy).expect("legacy dir");
        let store = HistoryStore::at_with_legacy(dir.path().join("history"), &workspace);
        let kept = entry(10);
        store.append(&kept).expect("existing entry");
        // The same entry name exists on both sides.
        let source = legacy.join(format!("{}.json", kept.id));
        fs::write(&source, serde_json::to_vec(&kept).expect("encode entry"))
            .expect("write legacy entry");

        let loaded = store.load().expect("load");

        assert_eq!(loaded.entries, vec![kept.clone()]);
        assert!(
            source.is_file(),
            "the duplicate is not overwritten or deleted"
        );
        assert!(legacy.is_dir(), "the leftover keeps the directory");
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.message.contains("left in the workspace")),
            "{:?}",
            loaded.issues
        );
    }

    #[test]
    fn a_non_entry_file_keeps_the_legacy_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let legacy = workspace.join("history");
        fs::create_dir_all(&legacy).expect("legacy dir");
        let store = HistoryStore::at_with_legacy(dir.path().join("history"), &workspace);
        let entry = entry(10);
        fs::write(
            legacy.join(format!("{}.json", entry.id)),
            serde_json::to_vec(&entry).expect("encode entry"),
        )
        .expect("write legacy entry");
        fs::write(legacy.join("notes.txt"), "keep me").expect("non-entry file");

        let loaded = store.load().expect("load");

        assert!(store.entry_file(entry.id).is_file());
        assert_eq!(
            fs::read_to_string(legacy.join("notes.txt")).expect("read"),
            "keep me",
            "a file that is not an entry is never deleted"
        );
        assert!(
            legacy.is_dir(),
            "the directory stays while it holds leftovers"
        );
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path.ends_with("notes.txt")),
            "{:?}",
            loaded.issues
        );
    }

    /// An entry the filesystem refuses to move must not be lost: it stays where
    /// it was and the load reports it.
    #[cfg(windows)]
    #[test]
    fn an_entry_that_cannot_be_moved_stays_readable_in_the_workspace() {
        use std::os::windows::fs::OpenOptionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let legacy = workspace.join("history");
        fs::create_dir_all(&legacy).expect("legacy dir");
        let store = HistoryStore::at_with_legacy(dir.path().join("history"), &workspace);
        let entry = entry(10);
        let source = legacy.join(format!("{}.json", entry.id));
        fs::write(&source, serde_json::to_vec(&entry).expect("encode entry"))
            .expect("write legacy entry");
        // A handle that shares nothing denies the rename.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&source)
            .expect("hold the entry open");

        let loaded = store.load().expect("load");

        assert!(source.is_file(), "the entry is left where it was");
        assert!(legacy.is_dir(), "a failed move keeps the directory");
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.path == source && issue.message.contains("not moved")),
            "{:?}",
            loaded.issues
        );
        drop(held);
        let parsed: HistoryEntry = serde_json::from_slice(&fs::read(&source).expect("read"))
            .expect("the entry still parses");
        assert_eq!(parsed, entry);
    }
}
