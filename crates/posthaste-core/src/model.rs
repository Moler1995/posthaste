//! Persisted request, collection, and environment models.
//!
//! These types are UI-agnostic and serialize to JSON. Every persisted type
//! carries a `schema_version` so future file formats can be migrated.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 1;

const fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

const fn default_true() -> bool {
    true
}

/// Stable identifier for a request. Also used as its filename stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(Uuid);

impl RequestId {
    /// Generates a time-sortable UUID v7 identifier.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for a folder inside one collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FolderId(Uuid);

impl FolderId {
    /// Generates a time-sortable UUID v7 identifier.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for FolderId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// One folder inside a collection's folder list. A missing `parent_id` means
/// the folder sits at the collection root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<FolderId>,
}

impl Folder {
    pub fn new(name: impl Into<String>, parent_id: Option<FolderId>) -> Self {
        Self {
            id: FolderId::new(),
            name: name.into(),
            parent_id,
        }
    }
}

/// HTTP methods supported by the MVP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    pub const ALL: [Self; 7] = [
        Self::Get,
        Self::Post,
        Self::Put,
        Self::Patch,
        Self::Delete,
        Self::Head,
        Self::Options,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The transport a request uses.
///
/// A request stored before this field existed loads as [`Self::Http`], and a
/// document naming a protocol this build does not know fails to parse rather
/// than being sent as HTTP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestProtocol {
    #[default]
    Http,
    WebSocket,
}

impl RequestProtocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::WebSocket => "websocket",
        }
    }

    /// The protocol a URL's scheme implies, when the scheme is one this app
    /// accepts for a WebSocket handshake.
    pub fn from_websocket_scheme(scheme: &str) -> Option<Self> {
        matches!(scheme, "ws" | "wss").then_some(Self::WebSocket)
    }
}

impl fmt::Display for RequestProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single header, query parameter, or form field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl KeyValue {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
            enabled: true,
        }
    }
}

/// Request body variants. `None` means the request has no body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RequestBody {
    #[default]
    None,
    Json(String),
    Text(String),
    Form(Vec<KeyValue>),
    /// A single file, sent as its raw bytes.
    ///
    /// `path` is relative to the workspace root with forward slashes; documents
    /// written before this variant may still hold an absolute path, which is
    /// used as given. Only the path is stored, never the contents.
    File {
        path: String,
    },
    Multipart(Vec<MultipartPart>),
}

/// One field of a `multipart/form-data` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MultipartPart {
    Text {
        key: String,
        value: String,
        #[serde(default = "default_true")]
        enabled: bool,
    },
    /// A file part. `path` follows the same rules as [`RequestBody::File`].
    File {
        key: String,
        path: String,
        #[serde(default = "default_true")]
        enabled: bool,
    },
}

impl MultipartPart {
    pub fn text(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Text {
            key: key.into(),
            value: value.into(),
            enabled: true,
        }
    }

    pub fn file(key: impl Into<String>, path: impl Into<String>) -> Self {
        Self::File {
            key: key.into(),
            path: path.into(),
            enabled: true,
        }
    }

    pub fn key(&self) -> &str {
        match self {
            Self::Text { key, .. } | Self::File { key, .. } => key,
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            Self::Text { enabled, .. } | Self::File { enabled, .. } => *enabled,
        }
    }
}

/// Where an API key credential is sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    /// A request header named by the credential's key.
    #[default]
    Header,
    /// A query parameter named by the credential's key.
    Query,
}

/// Request-level authentication applied when the request is composed.
///
/// The variant carries the values the user entered; they may still contain
/// `{{name}}` placeholders until variable resolution runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum RequestAuth {
    #[default]
    None,
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    ApiKey {
        key: String,
        value: String,
        location: ApiKeyLocation,
    },
}

/// A reusable request definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSpec {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub id: RequestId,
    pub name: String,
    pub method: HttpMethod,
    pub url: String,
    /// The transport Send uses. Defaults to HTTP for documents written before
    /// the field existed.
    #[serde(default)]
    pub protocol: RequestProtocol,
    /// Subprotocols offered in a WebSocket handshake, in order. The method and
    /// body stay on the request while it is WebSocket, so switching protocols
    /// preserves them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subprotocols: Vec<String>,
    #[serde(default)]
    pub headers: Vec<KeyValue>,
    #[serde(default)]
    pub query: Vec<KeyValue>,
    #[serde(default)]
    pub body: RequestBody,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub auth: RequestAuth,
    /// Folder this request belongs to, or `None` for the collection root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<FolderId>,
}

impl RequestSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            id: RequestId::new(),
            name: name.into(),
            method: HttpMethod::Get,
            url: String::new(),
            protocol: RequestProtocol::Http,
            subprotocols: Vec::new(),
            headers: Vec::new(),
            query: Vec::new(),
            body: RequestBody::None,
            timeout_ms: None,
            auth: RequestAuth::None,
            folder_id: None,
        }
    }

    /// Moves any query string carried in `url` into `query` rows.
    ///
    /// The URL is the base URL — scheme, authority, path, and an optional
    /// fragment — while the query rows are the only stored query. Rows the URL
    /// cannot express keep their position. Normalizing twice changes nothing.
    pub fn normalize_query(&mut self) {
        if !self.url.contains('?') {
            return;
        }
        let (base, parsed) = crate::url_query::split_url_query(&self.url);
        self.query = crate::url_query::union_query_rows(parsed, &self.query);
        self.url = base;
    }
}

/// The metadata needed to render a request in the sidebar without reading its
/// document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSummary {
    pub id: RequestId,
    pub name: String,
    pub method: HttpMethod,
    pub url: String,
    /// Which badge the sidebar draws in place of a method.
    #[serde(default)]
    pub protocol: RequestProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<FolderId>,
}

/// Identifies one on-disk request document closely enough to decide whether a
/// previously cached summary is still current.
///
/// The file name is compared as well as the timestamp and size so that replacing
/// a file with a different one that happens to reuse its timestamp cannot be
/// mistaken for the same document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    /// File name including the extension.
    pub file_name: String,
    /// Seconds since the Unix epoch.
    pub modified_s: u64,
    /// Nanoseconds within `modified_s`.
    pub modified_ns: u32,
    /// File size in bytes.
    pub size: u64,
}

impl FileFingerprint {
    /// Captures the fingerprint of an entry returned by `read_dir`.
    ///
    /// Returns `None` when the file name is not UTF-8 or when its metadata
    /// cannot be read; callers then read the document instead of trusting a
    /// cached summary.
    pub fn from_dir_entry(entry: &fs::DirEntry) -> Option<Self> {
        let metadata = entry.metadata().ok()?;
        let file_name = entry.file_name().to_str()?.to_owned();
        Some(Self::from_metadata(file_name, &metadata))
    }

    /// Captures the fingerprint of a path's current file.
    pub fn from_path(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let file_name = path.file_name()?.to_str()?.to_owned();
        Some(Self::from_metadata(file_name, &metadata))
    }

    fn from_metadata(file_name: String, metadata: &fs::Metadata) -> Self {
        let (modified_s, modified_ns) = match metadata.modified() {
            Ok(modified) => match modified.duration_since(std::time::UNIX_EPOCH) {
                Ok(elapsed) => (elapsed.as_secs(), elapsed.subsec_nanos()),
                Err(_) => (0, 0),
            },
            Err(_) => (0, 0),
        };
        Self {
            file_name,
            modified_s,
            modified_ns,
            size: metadata.len(),
        }
    }

    /// Whether `other` describes the same file contents and identity.
    pub fn matches(&self, other: &Self) -> bool {
        self.file_name == other.file_name
            && self.modified_s == other.modified_s
            && self.modified_ns == other.modified_ns
            && self.size == other.size
    }
}

/// Collection metadata. Requests live in sibling files under `requests/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collection {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub order: u32,
    /// Flat folder registry for this collection. Children reference parents by
    /// identifier, so a move rewrites one entry rather than re-indenting a
    /// subtree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<Folder>,
}

impl Collection {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            id: id.into(),
            name: name.into(),
            order: 0,
            folders: Vec::new(),
        }
    }

    /// The folder entry with `id`, if the collection has one.
    pub fn folder(&self, id: FolderId) -> Option<&Folder> {
        self.folders.iter().find(|folder| folder.id == id)
    }

    pub fn folder_mut(&mut self, id: FolderId) -> Option<&mut Folder> {
        self.folders.iter_mut().find(|folder| folder.id == id)
    }

    /// Appends a folder entry to the collection document and returns its new
    /// identifier.
    pub fn add_folder(&mut self, name: impl Into<String>, parent_id: Option<FolderId>) -> FolderId {
        let folder = Folder::new(name, parent_id);
        let id = folder.id;
        self.folders.push(folder);
        id
    }

    /// Renames one folder entry. Returns `false` when the folder is absent.
    pub fn rename_folder(&mut self, id: FolderId, name: impl Into<String>) -> bool {
        match self.folder_mut(id) {
            Some(folder) => {
                folder.name = name.into();
                true
            }
            None => false,
        }
    }

    /// Reparents one folder entry without moving any file. Returns `false` when
    /// the folder or its new parent is absent, or the move would make the
    /// folder its own ancestor.
    pub fn move_folder(&mut self, id: FolderId, parent_id: Option<FolderId>) -> bool {
        if !self.can_move_folder(id, parent_id) {
            return false;
        }
        match self.folder_mut(id) {
            Some(folder) => {
                folder.parent_id = parent_id;
                true
            }
            None => false,
        }
    }

    /// Returns whether `move_folder(id, parent_id)` would be accepted.
    pub fn can_move_folder(&self, id: FolderId, parent_id: Option<FolderId>) -> bool {
        if self.folder(id).is_none() {
            return false;
        }
        let Some(parent_id) = parent_id else {
            return true;
        };
        if self.folder(parent_id).is_none() || parent_id == id {
            return false;
        }
        // Walking up from the new parent must never reach the moved folder.
        let mut current = Some(parent_id);
        let mut visited = HashSet::new();
        while let Some(folder_id) = current {
            if folder_id == id || !visited.insert(folder_id) {
                return false;
            }
            current = self.folder(folder_id).and_then(|folder| folder.parent_id);
        }
        true
    }

    /// Removes one folder entry. Descendants are left in place with their
    /// parent reference dangling; callers removing a subtree remove every
    /// descendant identifier.
    pub fn remove_folder(&mut self, id: FolderId) -> bool {
        let before = self.folders.len();
        self.folders.retain(|folder| folder.id != id);
        self.folders.len() != before
    }
}

/// Named set of variables used for `{{variable}}` substitution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

impl Environment {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            name: name.into(),
            vars: BTreeMap::new(),
        }
    }
}

/// Root workspace metadata (`workspace.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    /// The environment an earlier version recorded here. It still
    /// deserializes so a workspace written before the move can adopt it once,
    /// and it is never serialized again: the selection lives in the session
    /// document, which is machine-local.
    #[serde(default, skip_serializing)]
    pub active_environment: Option<String>,
    /// Variables available to every request, overridden by the active
    /// environment.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

impl Workspace {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            name: name.into(),
            active_environment: None,
            vars: BTreeMap::new(),
        }
    }
}

/// A recorded HTTP response. Not persisted yet; history storage comes later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub duration: Duration,
    pub truncated: bool,
    /// The cookies the final response set, described against its URL.
    #[serde(default)]
    pub cookies: Vec<ResponseCookie>,
}

/// One cookie a response set, with the attributes the jar resolved.
///
/// `value` is present for a live response and absent once the metadata is
/// stored in history, which never keeps cookie values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseCookie {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub domain: String,
    #[serde(default)]
    pub host_only: bool,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    /// Whether the session jar kept the cookie. A cookie rejected by the jar's
    /// rules is still reported, with this flag false.
    #[serde(default)]
    pub stored: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(spec: &RequestSpec) -> RequestSpec {
        let json = serde_json::to_string(spec).expect("serialize request spec");
        serde_json::from_str(&json).expect("deserialize request spec")
    }

    #[test]
    fn request_spec_round_trips_for_every_method_and_body() {
        let bodies = [
            RequestBody::None,
            RequestBody::Json(r#"{"ok":true}"#.to_owned()),
            RequestBody::Text("hello".to_owned()),
            RequestBody::Form(vec![KeyValue::new("a", "b")]),
            RequestBody::File {
                path: "files/0123-cover.png".to_owned(),
            },
            RequestBody::Multipart(vec![
                MultipartPart::text("caption", "hello"),
                MultipartPart::file("image", "files/0123-cover.png"),
            ]),
        ];

        for method in HttpMethod::ALL {
            for body in &bodies {
                let mut spec = RequestSpec::new("round trip");
                spec.method = method;
                spec.url = "https://example.com/api".to_owned();
                spec.headers = vec![KeyValue::new("accept", "application/json")];
                spec.query = vec![KeyValue::new("page", "1")];
                spec.body = body.clone();
                spec.timeout_ms = Some(5_000);

                assert_eq!(round_trip(&spec), spec);
            }
        }
    }

    #[test]
    fn a_request_without_a_protocol_loads_as_http() {
        let json = serde_json::to_string(&RequestSpec::new("legacy")).expect("serialize");
        let json = json.replace(r#""protocol":"http","#, "");
        assert!(
            !json.contains(r#""protocol""#),
            "the fixture must omit the field: {json}"
        );
        let spec: RequestSpec = serde_json::from_str(&json).expect("deserialize a legacy spec");
        assert_eq!(spec.protocol, RequestProtocol::Http);
        assert!(spec.subprotocols.is_empty());
    }

    #[test]
    fn request_protocols_round_trip() {
        for protocol in [RequestProtocol::Http, RequestProtocol::WebSocket] {
            let mut spec = RequestSpec::new("protocol");
            spec.protocol = protocol;
            if protocol == RequestProtocol::WebSocket {
                spec.subprotocols = vec!["graphql-ws".to_owned(), "json".to_owned()];
            }
            let restored = round_trip(&spec);
            assert_eq!(restored.protocol, protocol);
            assert_eq!(restored.subprotocols, spec.subprotocols);
        }
    }

    #[test]
    fn an_unknown_protocol_fails_to_parse() {
        let json = serde_json::to_string(&RequestSpec::new("protocol")).expect("serialize");
        let json = json.replace(r#""protocol":"http""#, r#""protocol":"quic""#);
        assert!(
            serde_json::from_str::<RequestSpec>(&json).is_err(),
            "an unknown protocol is never silently read as HTTP"
        );
    }

    #[test]
    fn upload_bodies_round_trip_without_losing_paths_or_values() {
        let body = RequestBody::Multipart(vec![
            MultipartPart::text("note", "一年一度的报告"),
            MultipartPart::File {
                key: "report".to_owned(),
                path: "files/9f5b-report.pdf".to_owned(),
                enabled: false,
            },
            MultipartPart::file("cover", "files/cover.png"),
        ]);

        let json = serde_json::to_string(&body).expect("serialize a multipart body");
        assert!(
            json.contains(r#""enabled":true"#) && json.contains(r#""enabled":false"#),
            "every part records its enabled flag: {json}"
        );
        assert_eq!(
            serde_json::from_str::<RequestBody>(&json).expect("deserialize a multipart body"),
            body
        );

        let file = RequestBody::File {
            path: "files/2024-annual report.csv".to_owned(),
        };
        let json = serde_json::to_string(&file).expect("serialize a file body");
        assert!(
            json.contains("2024-annual report.csv"),
            "the recorded path survives verbatim: {json}"
        );
        assert_eq!(
            serde_json::from_str::<RequestBody>(&json).expect("deserialize a file body"),
            file
        );
    }

    #[test]
    fn an_uploaded_part_without_an_enabled_flag_still_loads() {
        let part: MultipartPart = serde_json::from_str(r#"{"type":"file","key":"a","path":"b"}"#)
            .expect("an older part loads");
        assert_eq!(part, MultipartPart::file("a", "b"));
        assert!(part.enabled());
    }

    #[test]
    fn documents_written_before_upload_bodies_existed_still_load() {
        let text: RequestBody = serde_json::from_str(r#"{"type":"text","content":"hello"}"#)
            .expect("a text body written before this change loads");
        assert_eq!(text, RequestBody::Text("hello".to_owned()));

        let form: RequestBody =
            serde_json::from_str(r#"{"type":"form","content":[{"key":"a","value":"b"}]}"#)
                .expect("a form body written before this change loads");
        assert_eq!(
            form,
            RequestBody::Form(vec![KeyValue {
                key: "a".to_owned(),
                value: "b".to_owned(),
                enabled: true,
            }])
        );
    }

    #[test]
    fn collection_and_environment_round_trip() {
        let collection = Collection::new("demo", "Demo Collection");
        let json = serde_json::to_string(&collection).unwrap();
        assert_eq!(
            serde_json::from_str::<Collection>(&json).unwrap(),
            collection
        );

        let mut environment = Environment::new("dev");
        environment
            .vars
            .insert("base_url".into(), "https://example.com".into());
        let json = serde_json::to_string(&environment).unwrap();
        assert_eq!(
            serde_json::from_str::<Environment>(&json).unwrap(),
            environment
        );
    }

    #[test]
    fn folder_and_nested_folder_round_trip() {
        let parent = Folder::new("Auth", None);
        let child = Folder::new("Tokens", Some(parent.id));

        let json = serde_json::to_string(&child).expect("serialize folder");
        assert!(
            json.contains("parent_id"),
            "a nested folder stores its parent"
        );
        assert_eq!(
            serde_json::from_str::<Folder>(&json).expect("deserialize folder"),
            child
        );

        let root_json = serde_json::to_string(&parent).expect("serialize root folder");
        assert!(
            !root_json.contains("parent_id"),
            "a root folder omits the parent key"
        );
        assert_eq!(
            serde_json::from_str::<Folder>(&root_json).expect("deserialize root folder"),
            parent
        );
    }

    #[test]
    fn a_response_record_without_a_cookies_field_still_loads() {
        let record = ResponseRecord {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: Bytes::from_static(b"ok"),
            duration: Duration::from_millis(5),
            truncated: false,
            cookies: Vec::new(),
        };
        let mut document = serde_json::to_value(&record).expect("encode the record");
        let removed = document
            .as_object_mut()
            .expect("a record is an object")
            .remove("cookies");
        assert!(removed.is_some(), "this test removes the new field");

        let loaded: ResponseRecord =
            serde_json::from_value(document).expect("an older record loads");
        assert_eq!(loaded, record);
        assert!(loaded.cookies.is_empty());
    }

    #[test]
    fn request_folder_reference_is_optional_and_omitted_when_absent() {
        let mut spec = RequestSpec::new("Login");
        let json = serde_json::to_string(&spec).expect("serialize request");
        assert!(
            !json.contains("folder_id"),
            "a root request omits folder_id"
        );
        assert_eq!(round_trip(&spec), spec);

        spec.folder_id = Some(FolderId::new());
        let json = serde_json::to_string(&spec).expect("serialize request");
        assert!(
            json.contains("folder_id"),
            "a folder request stores folder_id"
        );
        assert_eq!(round_trip(&spec), spec);
    }

    #[test]
    fn request_auth_round_trips_and_defaults_to_none() {
        let mut spec = RequestSpec::new("auth");
        assert_eq!(spec.auth, RequestAuth::None);
        assert_eq!(round_trip(&spec), spec);

        for auth in [
            RequestAuth::Bearer {
                token: "abc".into(),
            },
            RequestAuth::Basic {
                username: "user".into(),
                password: "pass".into(),
            },
            RequestAuth::ApiKey {
                key: "X-Api-Key".into(),
                value: "secret".into(),
                location: ApiKeyLocation::Header,
            },
            RequestAuth::ApiKey {
                key: "api_key".into(),
                value: "secret".into(),
                location: ApiKeyLocation::Query,
            },
        ] {
            spec.auth = auth;
            assert_eq!(round_trip(&spec), spec);
        }
    }

    #[test]
    fn a_request_without_auth_loads_as_none() {
        let json = r#"{
  "id": "01931c9f-7a2b-7c5e-8f00-123456789abc",
  "name": "minimal",
  "method": "GET",
  "url": "https://example.com"
}"#;
        let spec: RequestSpec = serde_json::from_str(json).expect("minimal request parses");
        assert_eq!(spec.auth, RequestAuth::None);
    }

    #[test]
    fn a_request_without_a_folder_loads_at_the_root() {
        let json = r#"{
  "id": "01931c9f-7a2b-7c5e-8f00-123456789abc",
  "name": "minimal",
  "method": "GET",
  "url": "https://example.com"
}"#;
        let spec: RequestSpec = serde_json::from_str(json).expect("minimal request parses");
        assert_eq!(spec.folder_id, None);
    }

    #[test]
    fn unknown_folder_fields_are_ignored() {
        let json = r#"{
  "schema_version": 1,
  "id": "demo",
  "name": "Demo",
  "order": 0,
  "folders": [
    {
      "id": "01931c9f-7a2b-7c5e-8f00-123456789abc",
      "name": "Auth",
      "parent_id": null,
      "future_field": "yes-please"
    }
  ],
  "future_collection_field": 7
}"#;
        let collection: Collection = serde_json::from_str(json).expect("collection parses");
        assert_eq!(collection.folders.len(), 1);
        assert_eq!(collection.folders[0].name, "Auth");
        assert_eq!(collection.folders[0].parent_id, None);
    }

    #[test]
    fn folderless_collection_omits_the_folder_list() {
        let collection = Collection::new("demo", "Demo Collection");
        let json = serde_json::to_string(&collection).expect("serialize collection");
        assert!(
            !json.contains("folders"),
            "an empty folder list leaves the document unchanged:\n{json}"
        );
        assert_eq!(
            serde_json::from_str::<Collection>(&json).expect("deserialize collection"),
            collection
        );
    }

    #[test]
    fn collection_with_folders_round_trips() {
        let mut collection = Collection::new("demo", "Demo Collection");
        let auth = Folder::new("Auth", None);
        let tokens = Folder::new("Tokens", Some(auth.id));
        collection.folders = vec![auth.clone(), tokens.clone()];

        let json = serde_json::to_string(&collection).expect("serialize collection");
        let parsed: Collection = serde_json::from_str(&json).expect("deserialize collection");
        assert_eq!(parsed, collection);
        assert_eq!(parsed.folders[1].parent_id, Some(parsed.folders[0].id));
    }

    #[test]
    fn collection_folder_operations_keep_identifiers_stable() {
        let mut collection = Collection::new("demo", "Demo");
        let root = collection.add_folder("Auth", None);
        let child = collection.add_folder("Tokens", Some(root));

        assert!(collection.rename_folder(root, "Authentication"));
        assert_eq!(collection.folder(root).unwrap().id, root);
        assert_eq!(collection.folder(root).unwrap().name, "Authentication");

        assert!(collection.move_folder(child, None));
        assert_eq!(collection.folder(child).unwrap().id, child);
        assert_eq!(collection.folder(child).unwrap().parent_id, None);
        assert_eq!(collection.folders.len(), 2);
    }

    #[test]
    fn folder_moves_reject_missing_folders_and_cycles() {
        let mut collection = Collection::new("demo", "Demo");
        let root = collection.add_folder("Auth", None);
        let child = collection.add_folder("Tokens", Some(root));
        let missing = FolderId::new();

        assert!(!collection.can_move_folder(root, Some(root)));
        assert!(!collection.can_move_folder(root, Some(child)));
        assert!(!collection.can_move_folder(root, Some(missing)));
        assert!(!collection.can_move_folder(missing, None));
        assert!(collection.can_move_folder(child, None));
        assert!(collection.can_move_folder(child, Some(root)));

        assert!(!collection.move_folder(root, Some(child)));
        assert_eq!(collection.folder(root).unwrap().parent_id, None);

        assert!(collection.rename_folder(root, "Renamed"));
        assert!(!collection.rename_folder(missing, "Nope"));
        assert!(collection.remove_folder(child));
        assert!(!collection.remove_folder(child));
        assert_eq!(collection.folders.len(), 1);
    }

    #[test]
    fn schema_version_defaults_when_missing() {
        let json = r#"{
  "id": "01931c9f-7a2b-7c5e-8f00-123456789abc",
  "name": "minimal",
  "method": "GET",
  "url": "https://example.com"
}"#;
        let spec: RequestSpec = serde_json::from_str(json).expect("minimal document parses");
        assert_eq!(spec.schema_version, SCHEMA_VERSION);
        assert_eq!(spec.method, HttpMethod::Get);
        assert_eq!(spec.body, RequestBody::None);
    }

    #[test]
    fn workspace_round_trips_with_defaults_and_omits_the_legacy_environment() {
        let mut workspace = Workspace::new("demo");
        workspace.active_environment = Some("staging".to_owned());
        workspace
            .vars
            .insert("host".to_owned(), "dev.example.com".to_owned());

        let json = serde_json::to_string(&workspace).expect("serialize workspace");

        assert!(
            !json.contains("active_environment"),
            "the selection lives in the session, not in workspace.json: {json}"
        );
        let parsed: Workspace = serde_json::from_str(&json).expect("deserialize workspace");
        assert_eq!(parsed.vars, workspace.vars);
        assert_eq!(parsed.active_environment, None);
    }

    #[test]
    fn a_legacy_active_environment_still_deserializes() {
        let json = r#"{"schema_version": 1, "name": "old", "active_environment": "staging"}"#;

        let workspace: Workspace = serde_json::from_str(json).expect("legacy workspace parses");

        assert_eq!(workspace.active_environment.as_deref(), Some("staging"));
    }

    #[test]
    fn a_workspace_without_vars_loads_empty_defaults() {
        let json = r#"{
  "schema_version": 1,
  "name": "minimal"
}"#;
        let workspace: Workspace = serde_json::from_str(json).expect("minimal workspace parses");
        assert!(workspace.vars.is_empty());
        assert_eq!(workspace.active_environment, None);
    }

    #[test]
    fn future_keys_and_schema_versions_are_tolerated() {
        let json = r#"{
  "schema_version": 99,
  "id": "01931c9f-7a2b-7c5e-8f00-123456789abc",
  "name": "future",
  "method": "POST",
  "url": "https://example.com",
  "future_field": 42
}"#;
        let spec: RequestSpec = serde_json::from_str(json).expect("future document parses");
        assert_eq!(spec.name, "future");
        assert_eq!(spec.method, HttpMethod::Post);
    }

    #[test]
    fn fingerprints_compare_name_time_and_size() {
        let base = FileFingerprint {
            file_name: "request.json".into(),
            modified_s: 10,
            modified_ns: 20,
            size: 30,
        };
        assert!(base.matches(&base.clone()));

        let mut changed_size = base.clone();
        changed_size.size = 31;
        assert!(!base.matches(&changed_size));

        let mut changed_time = base.clone();
        changed_time.modified_s = 11;
        assert!(!base.matches(&changed_time));
        changed_time.modified_s = 10;
        changed_time.modified_ns = 21;
        assert!(!base.matches(&changed_time));

        let mut changed_name = base.clone();
        changed_name.file_name = "other.json".into();
        assert!(!base.matches(&changed_name));
    }

    #[test]
    fn fingerprint_captures_the_read_dir_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("request.json");
        std::fs::write(&path, b"contents").expect("write request");

        let entry = std::fs::read_dir(dir.path())
            .expect("read dir")
            .next()
            .expect("one entry")
            .expect("valid entry");
        let fingerprint = FileFingerprint::from_dir_entry(&entry).expect("fingerprint");

        assert_eq!(fingerprint.file_name, "request.json");
        assert_eq!(fingerprint.size, 8);
        assert!(fingerprint.matches(&FileFingerprint::from_path(&path).expect("path fingerprint")));
    }

    #[test]
    fn normalize_query_moves_a_url_query_into_rows() {
        let mut spec = RequestSpec::new("legacy");
        spec.url = "https://api.example.com/items?page=2".to_owned();

        spec.normalize_query();

        assert_eq!(spec.url, "https://api.example.com/items");
        assert_eq!(spec.query, vec![KeyValue::new("page", "2")]);
    }

    #[test]
    fn normalize_query_is_idempotent() {
        let mut spec = RequestSpec::new("legacy");
        spec.url = "https://api.example.com/items?page=2".to_owned();
        spec.normalize_query();
        let once = spec.clone();

        spec.normalize_query();

        assert_eq!(spec, once);
    }

    #[test]
    fn normalize_query_keeps_rows_the_url_cannot_express() {
        let mut spec = RequestSpec::new("mixed");
        spec.url = "https://api.example.com/items?a=1".to_owned();
        spec.query = vec![
            KeyValue {
                key: "debug".to_owned(),
                value: "verbose".to_owned(),
                enabled: false,
            },
            KeyValue::new("b", "2"),
        ];

        spec.normalize_query();

        assert_eq!(spec.url, "https://api.example.com/items");
        assert_eq!(
            spec.query,
            vec![
                KeyValue {
                    key: "debug".to_owned(),
                    value: "verbose".to_owned(),
                    enabled: false,
                },
                KeyValue::new("b", "2"),
                KeyValue::new("a", "1"),
            ]
        );
    }

    #[test]
    fn normalize_query_drops_a_row_the_url_also_contains() {
        let mut spec = RequestSpec::new("duplicated");
        spec.url = "https://api.example.com/items?page=2".to_owned();
        spec.query = vec![KeyValue::new("page", "2")];

        spec.normalize_query();

        assert_eq!(spec.url, "https://api.example.com/items");
        assert_eq!(spec.query, vec![KeyValue::new("page", "2")]);
    }
}
