//! Portable documents that carry workspace content in and out of Posthaste.
//!
//! Every format converts into one intermediate shape, [`ImportBundle`], and
//! the app materializes that shape through the workspace APIs. This module owns
//! only parsing and rendering: it never writes to the workspace directory, so a
//! document that cannot be understood changes nothing.
//!
//! The first format is the native Posthaste bundle: one UTF-8 JSON document whose
//! folders and requests are the same model structs the workspace stores. That
//! keeps the bundle readable by the same rules as a stored request, and means a
//! future format only needs a parser and a renderer. JSON is also what the
//! workspace itself writes, so a bundle is written and read by the same codec
//! as the documents it came from.

pub mod curl;
pub mod postman;

pub use curl::{CurlImport, MAX_CURL_TEXT_BYTES, parse_curl_requests, parse_curl_text};

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::{Environment, Folder, FolderId, RequestId, RequestSpec, SCHEMA_VERSION};

/// The bundle format version this build writes and accepts.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// The top-level `kind` marker of a Posthaste bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundleKind {
    /// The marker this build writes.
    #[serde(rename = "posthaste-bundle")]
    PosthasteBundle,
}

impl BundleKind {
    /// The marker value this build writes.
    pub const MARKER: &'static str = "posthaste-bundle";

    fn is_bundle(value: &str) -> bool {
        value == Self::MARKER
    }
}

/// One Posthaste bundle document, exactly as it is written to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosthasteBundle {
    pub schema_version: u32,
    pub kind: BundleKind,
    #[serde(default)]
    pub collections: Vec<BundleCollection>,
    #[serde(default)]
    pub environments: Vec<Environment>,
}

/// One collection inside a bundle: its display name, folder tree, and request
/// documents. `order` is preserved so a round-trip keeps the collection's place
/// in the sidebar; imports otherwise sort by display name like the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleCollection {
    pub name: String,
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub folders: Vec<Folder>,
    #[serde(default)]
    pub requests: Vec<RequestSpec>,
}

/// The parsed content the app materializes.
///
/// Folder and request identifiers are already regenerated here, so the same
/// document imported twice can never share an identifier with its first import
/// or with the document it came from. Folder references are remapped to the new
/// identifiers; a reference that cannot be resolved places its item at the
/// collection root and records a warning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportBundle {
    pub collections: Vec<ImportCollection>,
    pub environments: Vec<Environment>,
    pub warnings: Vec<ImportWarning>,
}

impl ImportBundle {
    /// Whether the document carried nothing that can be materialized.
    pub fn is_empty(&self) -> bool {
        self.collections.is_empty() && self.environments.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportCollection {
    pub name: String,
    pub order: u32,
    pub folders: Vec<Folder>,
    pub requests: Vec<RequestSpec>,
}

/// An item the document carried but the import could not keep as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportWarning {
    /// The item's name, or its position when it has none.
    pub item: String,
    pub reason: String,
}

impl fmt::Display for ImportWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.item, self.reason)
    }
}

/// The document formats this build can import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    PosthasteBundle,
    PostmanCollection,
}

impl ImportFormat {
    /// How the import report names the detected format.
    pub fn label(self) -> &'static str {
        match self {
            Self::PosthasteBundle => "Posthaste bundle",
            Self::PostmanCollection => "Postman collection",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InterchangeError {
    #[error(
        "the document declares schema version {found}, but this build supports version {supported}"
    )]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("the file is not a Posthaste bundle")]
    NotRecognized,
    #[error("the document could not be read as JSON: {message}")]
    Parse { message: String },
    #[error("{message}")]
    Unavailable { message: String },
    #[error("the cURL text is {size} bytes, but this build accepts at most {limit} bytes")]
    CurlTextTooLarge { size: usize, limit: usize },
}

/// Recognizes an importable document from its content marker first, then its
/// file name.
///
/// Content wins because a user may rename a file; the extension covers the same
/// document saved under a name that says nothing. Anything else is refused
/// rather than guessed at.
pub fn sniff(file_name: &str, bytes: &[u8]) -> Option<ImportFormat> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(BundleKind::is_bundle)
        {
            return Some(ImportFormat::PosthasteBundle);
        }
        if postman::declares_collection(&value) {
            return Some(ImportFormat::PostmanCollection);
        }
    }
    // The extension only rescues a bundle whose JSON cannot be parsed enough to
    // carry a marker; a Postman document must declare its schema.
    let name = file_name.to_ascii_lowercase();
    if name.ends_with(".posthaste.json") {
        return Some(ImportFormat::PosthasteBundle);
    }
    None
}

/// Parses a recognized document into the shape the app materializes.
pub fn parse(format: ImportFormat, bytes: &[u8]) -> Result<ImportBundle, InterchangeError> {
    match format {
        ImportFormat::PosthasteBundle => parse_bundle(bytes),
        ImportFormat::PostmanCollection => postman::parse_postman(bytes),
    }
}

fn parse_bundle(bytes: &[u8]) -> Result<ImportBundle, InterchangeError> {
    let root = serde_json::from_slice::<serde_json::Value>(bytes).map_err(|error| {
        InterchangeError::Parse {
            message: error.to_string(),
        }
    })?;
    if !root.is_object() {
        return Err(InterchangeError::NotRecognized);
    }

    match root.get("kind").and_then(serde_json::Value::as_str) {
        Some(kind) if BundleKind::is_bundle(kind) => {}
        // A document with no marker is still a bundle when it carries the
        // bundle's own structure; that is what lets an extension-only match
        // import.
        None if root.get("collections").is_some() => {}
        _ => return Err(InterchangeError::NotRecognized),
    }

    let found = root
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(u64::from(BUNDLE_SCHEMA_VERSION));
    if found > u64::from(BUNDLE_SCHEMA_VERSION) {
        return Err(InterchangeError::UnsupportedVersion {
            found: u32::try_from(found).unwrap_or(u32::MAX),
            supported: BUNDLE_SCHEMA_VERSION,
        });
    }

    let mut warnings = Vec::new();
    let collections = match root.get("collections") {
        Some(value) => parse_collections(value, &mut warnings)?,
        None => Vec::new(),
    };
    let environments = match root.get("environments") {
        Some(value) => parse_environments(value, &mut warnings),
        None => Vec::new(),
    };

    Ok(ImportBundle {
        collections,
        environments,
        warnings,
    })
}

fn parse_collections(
    value: &serde_json::Value,
    warnings: &mut Vec<ImportWarning>,
) -> Result<Vec<ImportCollection>, InterchangeError> {
    let Some(items) = value.as_array() else {
        return Err(InterchangeError::Parse {
            message: "`collections` is not a list".to_owned(),
        });
    };

    let mut collections = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let position = index + 1;
        let raw = match serde_json::from_value::<RawCollection>(item.clone()) {
            Ok(raw) => raw,
            Err(error) => {
                warnings.push(ImportWarning {
                    item: format!("collection {position}"),
                    reason: format!("the collection could not be read: {error}"),
                });
                continue;
            }
        };
        let name = raw.name.trim();
        if name.is_empty() {
            warnings.push(ImportWarning {
                item: format!("collection {position}"),
                reason: "the collection has no name".to_owned(),
            });
            continue;
        }

        let folders = parse_folders(raw.folders, name, warnings);
        let requests = parse_requests(raw.requests, name, &folders.remap, warnings);
        collections.push(ImportCollection {
            name: name.to_owned(),
            order: raw.order,
            folders: folders.folders,
            requests,
        });
    }
    Ok(collections)
}

/// A collection as written, with its items left unparsed so one malformed entry
/// cannot take its siblings down with it.
#[derive(Debug, Deserialize)]
struct RawCollection {
    name: String,
    #[serde(default)]
    order: u32,
    #[serde(default)]
    folders: Vec<serde_json::Value>,
    #[serde(default)]
    requests: Vec<serde_json::Value>,
}

struct NormalizedFolders {
    folders: Vec<Folder>,
    /// Old bundle identifier to the identifier created for it.
    remap: HashMap<FolderId, FolderId>,
}

fn parse_folders(
    items: Vec<serde_json::Value>,
    collection: &str,
    warnings: &mut Vec<ImportWarning>,
) -> NormalizedFolders {
    let mut parsed = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        match serde_json::from_value::<Folder>(item) {
            Ok(folder) => parsed.push(folder),
            Err(error) => warnings.push(ImportWarning {
                item: format!("{collection} / folder {}", index + 1),
                reason: format!("the folder could not be read: {error}"),
            }),
        }
    }

    let remap: HashMap<FolderId, FolderId> = parsed
        .iter()
        .map(|folder| (folder.id, FolderId::new()))
        .collect();
    let mut folders = parsed;
    for (index, folder) in folders.iter_mut().enumerate() {
        let label = folder_label(&folder.name, index);
        folder.id = remap[&folder.id];
        if let Some(parent) = folder.parent_id {
            match remap.get(&parent) {
                Some(mapped) => folder.parent_id = Some(*mapped),
                None => {
                    folder.parent_id = None;
                    warnings.push(ImportWarning {
                        item: format!("{collection} / {label}"),
                        reason:
                            "its parent folder is missing, so it was placed at the collection root"
                                .to_owned(),
                    });
                }
            }
        }
    }

    break_folder_cycles(collection, &mut folders, warnings);
    NormalizedFolders { folders, remap }
}

/// A parent reference that loops back on itself would make the folder tree
/// unreadable, so a folder in a cycle starts again at the collection root.
fn break_folder_cycles(
    collection: &str,
    folders: &mut [Folder],
    warnings: &mut Vec<ImportWarning>,
) {
    let parents: HashMap<FolderId, Option<FolderId>> = folders
        .iter()
        .map(|folder| (folder.id, folder.parent_id))
        .collect();
    for (index, folder) in folders.iter_mut().enumerate() {
        let mut seen = HashSet::new();
        let mut current = folder.parent_id;
        let mut cycle = false;
        while let Some(id) = current {
            if !seen.insert(id) {
                cycle = true;
                break;
            }
            current = parents.get(&id).copied().flatten();
        }
        if cycle {
            let label = folder_label(&folder.name, index);
            folder.parent_id = None;
            warnings.push(ImportWarning {
                item: format!("{collection} / {label}"),
                reason:
                    "its folder chain loops back on itself, so it was placed at the collection root"
                        .to_owned(),
            });
        }
    }
}

fn folder_label(name: &str, index: usize) -> String {
    let name = name.trim();
    if name.is_empty() {
        format!("folder {}", index + 1)
    } else {
        name.to_owned()
    }
}

fn parse_requests(
    items: Vec<serde_json::Value>,
    collection: &str,
    folder_remap: &HashMap<FolderId, FolderId>,
    warnings: &mut Vec<ImportWarning>,
) -> Vec<RequestSpec> {
    let mut requests = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let mut request = match serde_json::from_value::<RequestSpec>(item) {
            Ok(request) => request,
            Err(error) => {
                warnings.push(ImportWarning {
                    item: format!("{collection} / request {}", index + 1),
                    reason: format!("the request could not be read: {error}"),
                });
                continue;
            }
        };
        let label = request_label(&request.name, index);
        request.id = RequestId::new();
        request.schema_version = SCHEMA_VERSION;
        // A URL that still carries a query is what the document actually sent,
        // so its pairs lead the table and the stored rows follow.
        if request.url.contains('?') {
            let (base, parsed) = crate::url_query::split_url_query(&request.url);
            request.query = crate::url_query::lead_union_query_rows(parsed, &request.query);
            request.url = base;
        }
        if let Some(folder) = request.folder_id {
            match folder_remap.get(&folder) {
                Some(mapped) => request.folder_id = Some(*mapped),
                None => {
                    request.folder_id = None;
                    warnings.push(ImportWarning {
                        item: format!("{collection} / {label}"),
                        reason: "its folder is missing, so it was placed at the collection root"
                            .to_owned(),
                    });
                }
            }
        }
        requests.push(request);
    }
    requests
}

fn request_label(name: &str, index: usize) -> String {
    let name = name.trim();
    if name.is_empty() {
        format!("request {}", index + 1)
    } else {
        name.to_owned()
    }
}

fn parse_environments(
    value: &serde_json::Value,
    warnings: &mut Vec<ImportWarning>,
) -> Vec<Environment> {
    let Some(items) = value.as_array() else {
        warnings.push(ImportWarning {
            item: "environments".to_owned(),
            reason: "`environments` is not a list, so none were imported".to_owned(),
        });
        return Vec::new();
    };

    let mut environments = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match serde_json::from_value::<Environment>(item.clone()) {
            Ok(environment) if !environment.name.trim().is_empty() => {
                environments.push(environment);
            }
            Ok(_) => warnings.push(ImportWarning {
                item: format!("environment {}", index + 1),
                reason: "the environment has no name".to_owned(),
            }),
            Err(error) => warnings.push(ImportWarning {
                item: format!("environment {}", index + 1),
                reason: format!("the environment could not be read: {error}"),
            }),
        }
    }
    environments
}

/// Renders collections and environments as one bundle document.
pub fn render_bundle(collections: &[BundleCollection], environments: &[Environment]) -> String {
    let bundle = PosthasteBundle {
        schema_version: BUNDLE_SCHEMA_VERSION,
        kind: BundleKind::PosthasteBundle,
        collections: collections.to_vec(),
        environments: environments.to_vec(),
    };
    // Every field is a string, number, list, or map, so JSON serialization
    // cannot fail here. The trailing newline matches the documents the
    // workspace writes.
    let mut rendered =
        serde_json::to_string_pretty(&bundle).expect("a bundle of model structs always serializes");
    rendered.push('\n');
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiKeyLocation, HttpMethod, KeyValue, RequestAuth, RequestBody};

    const BUNDLE: &str = r#"{
  "schema_version": 1,
  "kind": "posthaste-bundle",
  "collections": [
    {
      "name": "Petstore",
      "order": 3,
      "folders": [
        { "id": "0190f0f0-0000-7000-8000-000000000001", "name": "Animals" },
        {
          "id": "0190f0f0-0000-7000-8000-000000000002",
          "name": "Dogs",
          "parent_id": "0190f0f0-0000-7000-8000-000000000001"
        }
      ],
      "requests": [
        {
          "schema_version": 1,
          "id": "0190f0f0-0000-7000-8000-0000000000aa",
          "name": "List dogs",
          "method": "GET",
          "url": "https://api.example.com/dogs",
          "query": [{ "key": "limit", "value": "10", "enabled": true }],
          "folder_id": "0190f0f0-0000-7000-8000-000000000002"
        }
      ]
    },
    { "name": "Other", "requests": [] }
  ],
  "environments": [
    {
      "schema_version": 1,
      "name": "staging",
      "vars": { "base_url": "https://staging.example.com" }
    }
  ]
}"#;

    fn parsed() -> ImportBundle {
        parse(ImportFormat::PosthasteBundle, BUNDLE.as_bytes()).unwrap()
    }

    #[test]
    fn a_bundle_literal_reads_into_the_model_structs() {
        let bundle = parsed();
        assert!(bundle.warnings.is_empty());
        assert_eq!(bundle.collections.len(), 2);
        let petstore = &bundle.collections[0];
        assert_eq!(petstore.name, "Petstore");
        assert_eq!(petstore.order, 3);
        assert_eq!(petstore.folders.len(), 2);
        assert_eq!(petstore.requests.len(), 1);
        assert_eq!(petstore.requests[0].name, "List dogs");
        assert_eq!(petstore.requests[0].method, HttpMethod::Get);
        assert_eq!(petstore.requests[0].query[0].key, "limit");
        assert_eq!(
            bundle.environments[0]
                .vars
                .get("base_url")
                .map(String::as_str),
            Some("https://staging.example.com")
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let with_extras = BUNDLE.replace(
            r#""kind": "posthaste-bundle","#,
            r#""kind": "posthaste-bundle", "future_field": { "nested": true },"#,
        );
        let bundle = parse(ImportFormat::PosthasteBundle, with_extras.as_bytes()).unwrap();
        assert_eq!(bundle.collections.len(), 2);
        assert_eq!(bundle.warnings.len(), 0);
    }

    #[test]
    fn folder_and_request_references_are_remapped_to_new_identifiers() {
        let bundle = parsed();
        let petstore = &bundle.collections[0];
        let animals = petstore
            .folders
            .iter()
            .find(|f| f.name == "Animals")
            .unwrap();
        let dogs = petstore.folders.iter().find(|f| f.name == "Dogs").unwrap();
        assert_eq!(dogs.parent_id, Some(animals.id));
        assert_eq!(petstore.requests[0].folder_id, Some(dogs.id));
        assert_ne!(
            animals.id.to_string(),
            "0190f0f0-0000-7000-8000-000000000001",
            "the source identifier is never kept"
        );
    }

    #[test]
    fn parsing_the_same_document_twice_assigns_distinct_identifiers() {
        let first = parsed();
        let second = parsed();
        let first_ids: Vec<String> = first.collections[0]
            .folders
            .iter()
            .map(|folder| folder.id.to_string())
            .chain(
                first.collections[0]
                    .requests
                    .iter()
                    .map(|request| request.id.to_string()),
            )
            .collect();
        let second_ids: Vec<String> = second.collections[0]
            .folders
            .iter()
            .map(|folder| folder.id.to_string())
            .chain(
                second.collections[0]
                    .requests
                    .iter()
                    .map(|request| request.id.to_string()),
            )
            .collect();
        assert_ne!(first_ids, second_ids);
    }

    #[test]
    fn a_malformed_request_is_skipped_with_a_warning() {
        let document = BUNDLE.replace(
            r#"{ "name": "Other", "requests": [] }"#,
            r#"{ "name": "Other", "requests": [
      { "schema_version": 1, "id": "0190f0f0-0000-7000-8000-0000000000bb",
        "name": "Broken", "method": "GET", "url": "https://api.example.com/broken",
        "body": { "type": "json", "content": 42 } },
      { "schema_version": 1, "id": "0190f0f0-0000-7000-8000-0000000000cc",
        "name": "Fine", "method": "GET", "url": "https://api.example.com/fine" }
    ] }"#,
        );
        let bundle = parse(ImportFormat::PosthasteBundle, document.as_bytes()).unwrap();
        let other = &bundle.collections[1];
        assert_eq!(other.requests.len(), 1);
        assert_eq!(other.requests[0].name, "Fine");
        assert_eq!(bundle.warnings.len(), 1);
        assert!(bundle.warnings[0].item.contains("request 1"));
        assert!(bundle.warnings[0].reason.contains("could not be read"));
    }

    #[test]
    fn a_newer_schema_version_is_refused_with_its_number() {
        let document = BUNDLE.replace(r#""schema_version": 1,"#, r#""schema_version": 7,"#);
        let error = parse(ImportFormat::PosthasteBundle, document.as_bytes()).unwrap_err();
        assert_eq!(
            error,
            InterchangeError::UnsupportedVersion {
                found: 7,
                supported: BUNDLE_SCHEMA_VERSION,
            }
        );
        assert!(error.to_string().contains('7'));
    }

    #[test]
    fn a_missing_parent_folder_falls_back_to_the_root() {
        let document = BUNDLE.replace(
            r#""parent_id": "0190f0f0-0000-7000-8000-000000000001""#,
            r#""parent_id": "0190f0f0-0000-7000-8000-0000000000ff""#,
        );
        let bundle = parse(ImportFormat::PosthasteBundle, document.as_bytes()).unwrap();
        let dogs = bundle.collections[0]
            .folders
            .iter()
            .find(|folder| folder.name == "Dogs")
            .unwrap();
        assert_eq!(dogs.parent_id, None);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item.ends_with("Dogs"))
        );
    }

    #[test]
    fn a_folder_cycle_is_broken_at_the_root() {
        let document = BUNDLE.replace(
            r#"{ "id": "0190f0f0-0000-7000-8000-000000000001", "name": "Animals" }"#,
            r#"{ "id": "0190f0f0-0000-7000-8000-000000000001", "name": "Animals",
          "parent_id": "0190f0f0-0000-7000-8000-000000000002" }"#,
        );
        let bundle = parse(ImportFormat::PosthasteBundle, document.as_bytes()).unwrap();
        let animals = bundle.collections[0]
            .folders
            .iter()
            .find(|folder| folder.name == "Animals")
            .unwrap();
        assert_eq!(animals.parent_id, None);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item.ends_with("Animals"))
        );
    }

    #[test]
    fn sniff_prefers_the_content_marker_then_the_extension() {
        assert_eq!(
            sniff("notes.json", BUNDLE.as_bytes()),
            Some(ImportFormat::PosthasteBundle)
        );
        assert_eq!(
            sniff("petstore.posthaste.json", b"not a bundle: ["),
            Some(ImportFormat::PosthasteBundle)
        );
        assert_eq!(
            sniff("PETSTORE.POSTHASTE.JSON", b""),
            Some(ImportFormat::PosthasteBundle)
        );
        assert_eq!(sniff("notes.json", br#"{"hello": "world"}"#), None);
    }

    #[test]
    fn a_document_without_marker_or_collections_is_refused() {
        let error = parse(
            ImportFormat::PosthasteBundle,
            br#"{"name": "just a document"}"#,
        )
        .unwrap_err();
        assert_eq!(error, InterchangeError::NotRecognized);
        let error = parse(
            ImportFormat::PosthasteBundle,
            br#"{"kind": "postman", "collections": []}"#,
        )
        .unwrap_err();
        assert_eq!(error, InterchangeError::NotRecognized);
    }

    #[test]
    fn an_extension_only_document_with_the_bundle_structure_imports() {
        let document = BUNDLE.replace("\"kind\": \"posthaste-bundle\",\n  ", "");
        let bundle = parse(ImportFormat::PosthasteBundle, document.as_bytes()).unwrap();
        assert_eq!(bundle.collections.len(), 2);
    }

    #[test]
    fn render_round_trips_every_request_field() {
        let folder = Folder::new("Dogs", None);
        let mut request = RequestSpec::new("Create dog");
        request.method = HttpMethod::Post;
        request.url = "https://api.example.com/dogs#top".to_owned();
        request.headers = vec![
            KeyValue::new("Content-Type", "application/json"),
            KeyValue {
                key: "X-Debug".to_owned(),
                value: "1".to_owned(),
                enabled: false,
            },
        ];
        request.query = vec![KeyValue::new("dry", "true")];
        request.body = RequestBody::Json(r#"{"name":"Rex"}"#.to_owned());
        request.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".to_owned(),
            value: "{{api_key}}".to_owned(),
            location: ApiKeyLocation::Header,
        };
        request.timeout_ms = Some(2_500);
        request.folder_id = Some(folder.id);

        let collection = BundleCollection {
            name: "Petstore".to_owned(),
            order: 2,
            folders: vec![folder.clone()],
            requests: vec![request.clone()],
        };
        let environment = Environment {
            schema_version: SCHEMA_VERSION,
            name: "staging".to_owned(),
            vars: [(
                "base_url".to_owned(),
                "https://staging.example.com".to_owned(),
            )]
            .into_iter()
            .collect(),
        };
        let rendered = render_bundle(&[collection], std::slice::from_ref(&environment));
        assert!(
            rendered.contains(r#""kind": "posthaste-bundle""#),
            "a new bundle names the current format"
        );
        let parsed = parse(ImportFormat::PosthasteBundle, rendered.as_bytes()).unwrap();

        let imported = &parsed.collections[0];
        assert_eq!(imported.name, "Petstore");
        assert_eq!(imported.order, 2);
        assert_eq!(imported.folders.len(), 1);
        assert_eq!(imported.folders[0].name, "Dogs");
        let imported_request = &imported.requests[0];
        assert_eq!(imported_request.method, HttpMethod::Post);
        assert_eq!(imported_request.url, "https://api.example.com/dogs#top");
        assert_eq!(imported_request.headers, request.headers);
        assert_eq!(imported_request.query, request.query);
        assert_eq!(imported_request.body, request.body);
        assert_eq!(imported_request.auth, request.auth);
        assert_eq!(imported_request.timeout_ms, Some(2_500));
        assert_eq!(imported_request.folder_id, Some(imported.folders[0].id));
        assert_eq!(parsed.environments, vec![environment]);
    }

    #[test]
    fn a_query_string_in_the_url_is_moved_into_rows_on_import() {
        let document = BUNDLE.replace(
            r#""url": "https://api.example.com/dogs""#,
            r#""url": "https://api.example.com/dogs?page=2""#,
        );
        let bundle = parse(ImportFormat::PosthasteBundle, document.as_bytes()).unwrap();
        let request = &bundle.collections[0].requests[0];
        assert_eq!(request.url, "https://api.example.com/dogs");
        let keys: Vec<&str> = request.query.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(keys, vec!["page", "limit"]);
    }

    /// The README's example is the format's public documentation, so it must be
    /// a document this build actually accepts.
    #[test]
    fn the_readme_bundle_example_parses() {
        let readme = include_str!("../../../../README.md");
        let section = readme
            .split("## Import and export")
            .nth(1)
            .expect("the README documents the bundle format");
        let block = section
            .split("```json")
            .nth(1)
            .expect("the section contains a JSON example");
        let document = block
            .split("```")
            .next()
            .expect("the example fence is closed");
        let format =
            sniff("readme.json", document.as_bytes()).expect("the example is recognizable");
        let bundle = parse(format, document.as_bytes()).expect("the example parses");
        assert_eq!(bundle.collections[0].name, "Petstore");
        assert_eq!(bundle.collections[0].requests[0].name, "List animals");
        assert_eq!(bundle.environments[0].name, "staging");
    }
}
