//! Postman Collection v2.0 and v2.1 as an import and export format.
//!
//! The schema is large and almost every field is optional, so the document is
//! navigated as [`serde_json::Value`] through a handful of helpers rather than
//! mirrored by a struct. That keeps documents produced by other tools readable
//! and means an unknown field cannot fail an import. Everything the model
//! cannot keep is counted once per kind and reported, instead of being dropped
//! silently.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use super::{BundleCollection, ImportBundle, ImportCollection, ImportWarning, InterchangeError};
use crate::model::{
    ApiKeyLocation, Environment, Folder, FolderId, HttpMethod, KeyValue, RequestAuth, RequestBody,
    RequestSpec,
};
use crate::url_query::{join_url_query, split_url_query};

/// How deep the recursive item tree is followed. A hand-written recursion on
/// untrusted input could overflow the stack, so the walk uses an explicit stack
/// and stops here; 50 levels is far beyond any real collection, and it stays
/// below the nesting depth this build's JSON parser accepts, so the cap is
/// always reachable.
const MAX_DEPTH: usize = 50;

/// Whether `root` declares a Postman collection schema this build reads.
pub fn declares_collection(root: &Value) -> bool {
    root.get("info")
        .and_then(|info| info.get("schema"))
        .and_then(Value::as_str)
        .is_some_and(is_collection_schema)
}

fn is_collection_schema(schema: &str) -> bool {
    let schema = schema.trim_end_matches('/');
    schema.ends_with("collection/v2.1.0/collection.json")
        || schema.ends_with("collection/v2.0.0/collection.json")
}

/// Parses a v2.0 or v2.1 document into the shared import shape.
pub fn parse_postman(bytes: &[u8]) -> Result<ImportBundle, InterchangeError> {
    let root = serde_json::from_slice::<Value>(bytes).map_err(|error| InterchangeError::Parse {
        message: error.to_string(),
    })?;
    if !declares_collection(&root) {
        return Err(InterchangeError::NotRecognized);
    }

    let collection_name = root
        .get("info")
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let collection_name = if collection_name.is_empty() {
        "Imported".to_owned()
    } else {
        collection_name.to_owned()
    };

    let mut omissions = PostmanOmissions::default();
    let mut warnings = Vec::new();
    if description_present(root.get("info").and_then(|info| info.get("description"))) {
        omissions.descriptions += 1;
    }
    collect_events(&root, &mut omissions);
    if collection_auth_present(root.get("auth")) {
        omissions.collection_auth += 1;
    }

    let (variables_present, mut environment_vars) = collect_variables(&root, &mut warnings);
    let mut url_variables = Vec::new();
    let (folders, requests) = walk_items(
        &root,
        &collection_name,
        &mut omissions,
        &mut url_variables,
        &mut warnings,
    );
    for (key, value) in url_variables {
        environment_vars.entry(key).or_insert(value);
    }

    let environments = if variables_present || !environment_vars.is_empty() {
        let mut environment = Environment::new(collection_name.clone());
        environment.vars = environment_vars;
        vec![environment]
    } else {
        Vec::new()
    };

    warnings.extend(omissions.warnings());
    Ok(ImportBundle {
        collections: vec![ImportCollection {
            name: collection_name,
            order: 0,
            folders,
            requests,
        }],
        environments,
        warnings,
    })
}

/// Walks `item` iteratively. Each folder is created before its children are
/// pushed, so a child can reference its parent's new identifier.
fn walk_items(
    root: &Value,
    collection_name: &str,
    omissions: &mut PostmanOmissions,
    url_variables: &mut Vec<(String, String)>,
    warnings: &mut Vec<ImportWarning>,
) -> (Vec<Folder>, Vec<RequestSpec>) {
    let mut folders = Vec::new();
    let mut requests = Vec::new();
    let mut stack: Vec<(&Value, Option<FolderId>, usize)> = Vec::new();

    if let Some(items) = array_field(root, "item") {
        for item in items.iter().rev() {
            stack.push((item, None, 1));
        }
    }

    while let Some((item, parent, depth)) = stack.pop() {
        let Some(object) = item.as_object() else {
            warnings.push(ImportWarning {
                item: "item".to_owned(),
                reason: "the item is not an object, so it was skipped".to_owned(),
            });
            continue;
        };
        collect_events(item, omissions);
        if description_present(object.get("description")) {
            omissions.descriptions += 1;
        }
        if let Some(examples) = array_field(item, "response") {
            omissions.examples += examples.len();
        }

        let raw_name = str_field(item, "name").unwrap_or_default();
        let name = if raw_name.trim().is_empty() {
            "Untitled"
        } else {
            raw_name
        };
        let label = format!("{collection_name} / {name}");

        if let Some(children) = array_field(item, "item") {
            if depth >= MAX_DEPTH {
                warnings.push(ImportWarning {
                    item: label,
                    reason: format!(
                        "the item tree is nested more than {MAX_DEPTH} levels deep, so this subtree was skipped"
                    ),
                });
                continue;
            }
            let folder = Folder::new(name, parent);
            let folder_id = folder.id;
            folders.push(folder);
            for child in children.iter().rev() {
                stack.push((child, Some(folder_id), depth + 1));
            }
        } else if let Some(request) = object.get("request") {
            if let Some(mut spec) =
                map_request(request, name, &label, omissions, url_variables, warnings)
            {
                spec.folder_id = parent;
                requests.push(spec);
            }
        } else {
            warnings.push(ImportWarning {
                item: label,
                reason: "the item has neither a request nor an item list, so it was skipped"
                    .to_owned(),
            });
        }
    }

    (folders, requests)
}

/// Maps one Postman request. A request the model cannot store is reported and
/// skipped; an unsupported body or auth is dropped while the request imports.
fn map_request(
    request: &Value,
    name: &str,
    label: &str,
    omissions: &mut PostmanOmissions,
    url_variables: &mut Vec<(String, String)>,
    warnings: &mut Vec<ImportWarning>,
) -> Option<RequestSpec> {
    if let Some(url) = request.as_str() {
        let mut spec = RequestSpec::new(name);
        spec.url = url.to_owned();
        spec.normalize_query();
        return Some(spec);
    }
    let Some(object) = request.as_object() else {
        warnings.push(ImportWarning {
            item: label.to_owned(),
            reason: "the request could not be read, so it was skipped".to_owned(),
        });
        return None;
    };

    if description_present(object.get("description")) {
        omissions.descriptions += 1;
    }

    let method_text = str_field(request, "method").unwrap_or("GET").trim();
    let method = if method_text.is_empty() {
        HttpMethod::Get
    } else {
        match parse_method(method_text) {
            Some(method) => method,
            None => {
                warnings.push(ImportWarning {
                    item: label.to_owned(),
                    reason: format!(
                        "the method {method_text} cannot be stored, so the request was skipped"
                    ),
                });
                return None;
            }
        }
    };

    let headers = value_rows(request.get("header"));
    let (url, query, converted) = map_url(object.get("url"), label, warnings);
    url_variables.extend(converted);

    let body = map_body(request.get("body"), &headers, label, omissions, warnings);
    let auth = map_auth(request.get("auth"), omissions);

    let mut spec = RequestSpec::new(name);
    spec.method = method;
    spec.url = url;
    spec.headers = headers;
    spec.query = query;
    spec.body = body;
    spec.auth = auth;
    spec.normalize_query();
    Some(spec)
}

fn parse_method(method: &str) -> Option<HttpMethod> {
    match method.trim().to_ascii_uppercase().as_str() {
        "GET" => Some(HttpMethod::Get),
        "POST" => Some(HttpMethod::Post),
        "PUT" => Some(HttpMethod::Put),
        "PATCH" => Some(HttpMethod::Patch),
        "DELETE" => Some(HttpMethod::Delete),
        "HEAD" => Some(HttpMethod::Head),
        "OPTIONS" => Some(HttpMethod::Options),
        _ => None,
    }
}

/// Splits a Postman URL into its base, its query rows, and the URL variables
/// that actually filled a `:name` path segment.
///
/// `query[]` is authoritative when present because `raw` omits disabled rows;
/// otherwise the query carried by `raw` is split into rows. Path placeholders
/// are rewritten segment by segment, so the scheme and authority are untouched.
fn map_url(
    value: Option<&Value>,
    label: &str,
    warnings: &mut Vec<ImportWarning>,
) -> (String, Vec<KeyValue>, Vec<(String, String)>) {
    let mut url_variables = Vec::new();
    let (raw, rows) = match value {
        Some(Value::String(raw)) => (raw.clone(), None),
        Some(Value::Object(object)) => {
            let raw = str_field_from(object, "raw")
                .map(str::to_owned)
                .unwrap_or_else(|| url_from_parts(object));
            let rows = object
                .get("query")
                .and_then(Value::as_array)
                .map(|query| query.iter().map(key_value).collect());
            (raw, rows)
        }
        None => (String::new(), None),
        Some(_) => {
            warnings.push(ImportWarning {
                item: label.to_owned(),
                reason: "the URL is neither a string nor an object, so the request has no URL"
                    .to_owned(),
            });
            (String::new(), None)
        }
    };

    let (base, raw_rows) = split_url_query(&raw);
    let (base, converted) = rewrite_path_variables(&base);
    let query = rows.unwrap_or(raw_rows);

    if let Some(object) = value.and_then(Value::as_object)
        && let Some(variables) = object.get("variable").and_then(Value::as_array)
    {
        for variable in variables {
            let Some(key) = str_field(variable, "key") else {
                continue;
            };
            if !converted.contains(key) {
                continue;
            }
            let value = scalar_field(variable, "value").unwrap_or_default();
            url_variables.push((key.to_owned(), value));
        }
    }

    (base, query, url_variables)
}

/// Rebuilds a URL object that has no `raw` string. The importer only uses this
/// when another tool omitted `raw`; `raw` is what Postman itself writes.
fn url_from_parts(object: &Map<String, Value>) -> String {
    let host = object
        .get("host")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(".")
        })
        .unwrap_or_default();
    if host.is_empty() {
        return String::new();
    }

    let mut url = match str_field_from(object, "protocol") {
        Some(protocol) => format!("{protocol}://{host}"),
        None => host,
    };
    if let Some(port) = scalar_field_from(object, "port").filter(|port| !port.is_empty()) {
        url.push(':');
        url.push_str(&port);
    }
    if let Some(path) = object
        .get("path")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|path| !path.is_empty())
    {
        url.push('/');
        url.push_str(path.trim_start_matches('/'));
    }
    url
}

/// Rewrites `:name` path segments into `{{name}}`, returning the segments that
/// were converted so `url.variable[]` can be merged for exactly those keys.
fn rewrite_path_variables(base: &str) -> (String, HashSet<String>) {
    let (head, fragment) = match base.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (base, None),
    };
    let mut converted = HashSet::new();
    let mut url = String::with_capacity(head.len());
    for (index, segment) in head.split('/').enumerate() {
        if index > 0 {
            url.push('/');
        }
        match segment.strip_prefix(':') {
            Some(name) if !name.is_empty() => {
                converted.insert(name.to_owned());
                url.push_str("{{");
                url.push_str(name);
                url.push_str("}}");
            }
            _ => url.push_str(segment),
        }
    }
    if let Some(fragment) = fragment {
        url.push('#');
        url.push_str(fragment);
    }
    (url, converted)
}

/// Maps a Postman body to the model's variants. The text of an unsupported body
/// is not imported because the model has nowhere faithful to put it.
fn map_body(
    value: Option<&Value>,
    headers: &[KeyValue],
    label: &str,
    omissions: &mut PostmanOmissions,
    warnings: &mut Vec<ImportWarning>,
) -> RequestBody {
    let Some(body) = value else {
        return RequestBody::None;
    };
    let Some(object) = body.as_object() else {
        warnings.push(ImportWarning {
            item: label.to_owned(),
            reason: "the body could not be read, so the request has no body".to_owned(),
        });
        return RequestBody::None;
    };
    let mode = str_field(body, "mode").unwrap_or("none");

    match mode {
        "raw" => {
            let text = str_field(body, "raw").unwrap_or_default();
            let language = object
                .get("options")
                .and_then(|options| options.get("raw"))
                .and_then(|raw| raw.get("language"))
                .and_then(Value::as_str);
            let json = match language {
                Some(language) => language.eq_ignore_ascii_case("json"),
                None => headers.iter().any(|header| {
                    header.enabled
                        && header.key.eq_ignore_ascii_case("content-type")
                        && is_json_content_type(&header.value)
                }),
            };
            if json {
                RequestBody::Json(text.to_owned())
            } else {
                RequestBody::Text(text.to_owned())
            }
        }
        "urlencoded" => RequestBody::Form(value_rows(object.get("urlencoded"))),
        "formdata" => {
            let mut rows = Vec::new();
            if let Some(parts) = object.get("formdata").and_then(Value::as_array) {
                for part in parts {
                    let key = scalar_field(part, "key").unwrap_or_default();
                    let part_type = str_field(part, "type").unwrap_or("text");
                    if part_type.eq_ignore_ascii_case("file") {
                        omissions.formdata_files.push(key);
                        continue;
                    }
                    rows.push(key_value(part));
                }
            }
            RequestBody::Form(rows)
        }
        "graphql" => {
            omissions.graphql_bodies += 1;
            RequestBody::None
        }
        "file" => {
            omissions.file_bodies += 1;
            RequestBody::None
        }
        "none" | "" => RequestBody::None,
        other => {
            warnings.push(ImportWarning {
                item: label.to_owned(),
                reason: format!(
                    "the body mode `{other}` is not supported, so the body was skipped"
                ),
            });
            RequestBody::None
        }
    }
}

fn is_json_content_type(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    value.starts_with("application/json") || value.contains("+json")
}

/// Maps request auth. `inherit` is reported like every other scheme the model
/// does not have, because inheriting collection auth would otherwise be silent.
fn map_auth(value: Option<&Value>, omissions: &mut PostmanOmissions) -> RequestAuth {
    let Some(auth) = value else {
        return RequestAuth::None;
    };
    let Some(object) = auth.as_object() else {
        return RequestAuth::None;
    };
    let kind = str_field(auth, "type").unwrap_or_default();

    match kind {
        "" | "noauth" => RequestAuth::None,
        "bearer" => RequestAuth::Bearer {
            token: auth_field(object, "bearer", "token").unwrap_or_default(),
        },
        "basic" => RequestAuth::Basic {
            username: auth_field(object, "basic", "username").unwrap_or_default(),
            password: auth_field(object, "basic", "password").unwrap_or_default(),
        },
        "apikey" => {
            let location = match auth_field(object, "apikey", "in")
                .unwrap_or_else(|| "header".to_owned())
                .to_ascii_lowercase()
                .as_str()
            {
                "query" => ApiKeyLocation::Query,
                _ => ApiKeyLocation::Header,
            };
            RequestAuth::ApiKey {
                key: auth_field(object, "apikey", "key").unwrap_or_default(),
                value: auth_field(object, "apikey", "value").unwrap_or_default(),
                location,
            }
        }
        other => {
            *omissions.auth_schemes.entry(other.to_owned()).or_insert(0) += 1;
            RequestAuth::None
        }
    }
}

/// Reads one auth attribute from Postman's `[{key, value}]` array, also
/// accepting the object form some tools emit.
fn auth_field(object: &Map<String, Value>, section: &str, key: &str) -> Option<String> {
    let value = object.get(section)?;
    if let Some(entries) = value.as_array() {
        return entries
            .iter()
            .find(|entry| str_field(entry, "key") == Some(key))
            .and_then(|entry| scalar_field(entry, "value"));
    }
    scalar_field(value, key)
}

/// Collects `variable[]` into an environment map. Object and array values have
/// no string form and are reported rather than guessed at.
fn collect_variables(
    root: &Value,
    warnings: &mut Vec<ImportWarning>,
) -> (bool, BTreeMap<String, String>) {
    let mut vars = BTreeMap::new();
    let Some(variables) = array_field(root, "variable") else {
        return (false, vars);
    };
    if variables.is_empty() {
        return (false, vars);
    }

    for (index, variable) in variables.iter().enumerate() {
        let key = str_field(variable, "key").unwrap_or_default();
        if key.is_empty() {
            warnings.push(ImportWarning {
                item: format!("collection variable {}", index + 1),
                reason: "the variable has no key, so it was skipped".to_owned(),
            });
            continue;
        }
        match variable.get("value") {
            Some(value @ (Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null)) => {
                vars.insert(key.to_owned(), scalar_text(value));
            }
            Some(_) => warnings.push(ImportWarning {
                item: format!("collection variable {key}"),
                reason: "its value is not a scalar, so it was skipped".to_owned(),
            }),
            None => {
                vars.insert(key.to_owned(), String::new());
            }
        }
    }
    (true, vars)
}

fn collection_auth_present(value: Option<&Value>) -> bool {
    value.and_then(Value::as_object).is_some_and(|auth| {
        auth.get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| !kind.is_empty() && kind != "noauth")
    })
}

fn collect_events(item: &Value, omissions: &mut PostmanOmissions) {
    let Some(events) = array_field(item, "event") else {
        return;
    };
    for event in events {
        match str_field(event, "listen") {
            Some("prerequest") => omissions.scripts += 1,
            Some("test") => omissions.tests += 1,
            _ => {}
        }
    }
}

fn description_present(value: Option<&Value>) -> bool {
    match value {
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Object(object)) => object
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty()),
        _ => false,
    }
}

/// Maps a header, query, urlencoded, or formdata row, preserving `disabled`.
fn key_value(value: &Value) -> KeyValue {
    KeyValue {
        key: scalar_field(value, "key").unwrap_or_default(),
        value: scalar_field(value, "value").unwrap_or_default(),
        enabled: !bool_field(value, "disabled").unwrap_or(false),
    }
}

fn value_rows(value: Option<&Value>) -> Vec<KeyValue> {
    value
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(key_value).collect())
        .unwrap_or_default()
}

/// The omissions a Postman document carries but the model cannot represent.
/// Counts are accumulated across the whole document so the report has one line
/// per kind rather than one per item.
#[derive(Debug, Default)]
struct PostmanOmissions {
    scripts: usize,
    tests: usize,
    examples: usize,
    descriptions: usize,
    collection_auth: usize,
    auth_schemes: BTreeMap<String, usize>,
    graphql_bodies: usize,
    file_bodies: usize,
    formdata_files: Vec<String>,
}

impl PostmanOmissions {
    fn warnings(&self) -> Vec<ImportWarning> {
        let mut warnings = Vec::new();
        if self.scripts > 0 {
            warnings.push(ImportWarning {
                item: plural(self.scripts, "pre-request script"),
                reason: "Posthaste does not store or run scripts.".to_owned(),
            });
        }
        if self.tests > 0 {
            warnings.push(ImportWarning {
                item: plural(self.tests, "test script"),
                reason: "Posthaste does not store or run tests.".to_owned(),
            });
        }
        if self.examples > 0 {
            warnings.push(ImportWarning {
                item: plural(self.examples, "saved example"),
                reason: "saved response examples are not imported.".to_owned(),
            });
        }
        if self.descriptions > 0 {
            warnings.push(ImportWarning {
                item: plural(self.descriptions, "description"),
                reason: "descriptions are not imported.".to_owned(),
            });
        }
        if self.collection_auth > 0 {
            warnings.push(ImportWarning {
                item: "collection-level auth".to_owned(),
                reason: "it was not applied; each request keeps its own auth.".to_owned(),
            });
        }
        for (scheme, count) in &self.auth_schemes {
            warnings.push(ImportWarning {
                item: format!(
                    "{count} {scheme} auth definition{}",
                    if *count == 1 { "" } else { "s" }
                ),
                reason: "the auth scheme is not supported, so those requests kept no auth."
                    .to_owned(),
            });
        }
        if self.graphql_bodies > 0 {
            warnings.push(ImportWarning {
                item: if self.graphql_bodies == 1 {
                    "1 GraphQL body".to_owned()
                } else {
                    format!("{} GraphQL bodies", self.graphql_bodies)
                },
                reason: "GraphQL bodies are not stored.".to_owned(),
            });
        }
        if self.file_bodies > 0 {
            warnings.push(ImportWarning {
                item: plural(self.file_bodies, "file body"),
                reason: "file bodies are not stored; a binary file transfer is a separate feature."
                    .to_owned(),
            });
        }
        if !self.formdata_files.is_empty() {
            let mut seen = HashSet::new();
            let names: Vec<&str> = self
                .formdata_files
                .iter()
                .map(String::as_str)
                .filter(|name| seen.insert(*name))
                .collect();
            warnings.push(ImportWarning {
                item: format!(
                    "{} form-data file part{} ({})",
                    names.len(),
                    if names.len() == 1 { "" } else { "s" },
                    names.join(", ")
                ),
                reason: "file parts are not imported.".to_owned(),
            });
        }
        warnings
    }
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn str_field_from<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key)?.as_str()
}

fn array_field<'a>(value: &'a Value, key: &str) -> Option<&'a Vec<Value>> {
    value.get(key)?.as_array()
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key)?.as_bool()
}

fn scalar_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).map(scalar_text)
}

fn scalar_field_from(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).map(scalar_text)
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null | Value::Array(_) | Value::Object(_) => String::new(),
    }
}

/// Renders one collection as a v2.1 document. Folders and requests are ordered
/// by display name, matching the sidebar; ids are fresh on every export because
/// the workspace has no collection identity to preserve.
pub fn render_postman(collection: &BundleCollection) -> String {
    let mut info = Map::new();
    info.insert(
        "_postman_id".to_owned(),
        Value::String(uuid::Uuid::now_v7().to_string()),
    );
    info.insert("name".to_owned(), Value::String(collection.name.clone()));
    info.insert(
        "schema".to_owned(),
        Value::String(
            "https://schema.getpostman.com/json/collection/v2.1.0/collection.json".to_owned(),
        ),
    );

    let mut items = Vec::new();
    for folder in sorted_folders(&collection.folders, None) {
        items.push(folder_item(
            folder,
            &collection.folders,
            &collection.requests,
        ));
    }
    for request in sorted_requests(&collection.requests, None) {
        items.push(request_item(request));
    }

    let document = json!({
        "info": Value::Object(info),
        "item": items,
    });
    let mut rendered =
        serde_json::to_string_pretty(&document).expect("a Postman document always serializes");
    rendered.push('\n');
    rendered
}

fn folder_item(folder: &Folder, folders: &[Folder], requests: &[RequestSpec]) -> Value {
    let mut items = Vec::new();
    for child in sorted_folders(folders, Some(folder.id)) {
        items.push(folder_item(child, folders, requests));
    }
    for request in sorted_requests(requests, Some(folder.id)) {
        items.push(request_item(request));
    }
    json!({
        "name": folder.name,
        "item": items,
    })
}

fn sorted_folders(folders: &[Folder], parent: Option<FolderId>) -> Vec<&Folder> {
    let mut children: Vec<&Folder> = folders
        .iter()
        .filter(|folder| folder.parent_id == parent)
        .collect();
    children.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    children
}

fn sorted_requests(requests: &[RequestSpec], folder: Option<FolderId>) -> Vec<&RequestSpec> {
    let mut children: Vec<&RequestSpec> = requests
        .iter()
        .filter(|request| request.folder_id == folder)
        .collect();
    children.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    children
}

fn request_item(request: &RequestSpec) -> Value {
    let mut object = Map::new();
    object.insert(
        "method".to_owned(),
        Value::String(request.method.as_str().to_owned()),
    );
    object.insert(
        "header".to_owned(),
        Value::Array(rows_json(&request.headers)),
    );
    object.insert("url".to_owned(), url_json(request));
    if let Some(body) = body_json(&request.body) {
        object.insert("body".to_owned(), body);
    }
    if let Some(auth) = auth_json(&request.auth) {
        object.insert("auth".to_owned(), auth);
    }

    json!({
        "name": request.name,
        "request": Value::Object(object),
    })
}

fn rows_json(rows: &[KeyValue]) -> Vec<Value> {
    rows.iter()
        .map(|row| {
            let mut object = Map::new();
            object.insert("key".to_owned(), Value::String(row.key.clone()));
            object.insert("value".to_owned(), Value::String(row.value.clone()));
            if !row.enabled {
                object.insert("disabled".to_owned(), Value::Bool(true));
            }
            Value::Object(object)
        })
        .collect()
}

/// Writes the URL as an object even when it cannot be parsed: `query[]` is what
/// preserves disabled rows, and `raw` keeps `{{placeholders}}` verbatim.
fn url_json(request: &RequestSpec) -> Value {
    let raw = join_url_query(&request.url, &request.query);
    let mut object = Map::new();
    object.insert("raw".to_owned(), Value::String(raw.clone()));
    object.insert("query".to_owned(), Value::Array(rows_json(&request.query)));

    if let Ok(parsed) = url::Url::parse(&raw) {
        if let Some(host) = parsed.host_str() {
            let parts: Vec<Value> = if host.starts_with('[') {
                vec![Value::String(host.to_owned())]
            } else {
                host.split('.')
                    .map(|part| Value::String(part.to_owned()))
                    .collect()
            };
            object.insert("host".to_owned(), Value::Array(parts));
        }
        let path: Vec<Value> = parsed
            .path_segments()
            .map(|segments| {
                segments
                    .filter(|segment| !segment.is_empty())
                    .map(|segment| Value::String(segment.to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        object.insert("path".to_owned(), Value::Array(path));
    }
    Value::Object(object)
}

fn body_json(body: &RequestBody) -> Option<Value> {
    match body {
        RequestBody::None => None,
        RequestBody::Json(text) => Some(json!({
            "mode": "raw",
            "raw": text,
            "options": { "raw": { "language": "json" } },
        })),
        RequestBody::Text(text) => Some(json!({
            "mode": "raw",
            "raw": text,
            "options": { "raw": { "language": "text" } },
        })),
        RequestBody::Form(rows) => Some(json!({
            "mode": "urlencoded",
            "urlencoded": rows_json(rows),
        })),
        // Import already skips file bodies and drops file parts, since the
        // workspace-relative paths have no Postman equivalent; exporting the
        // same shapes would claim a fidelity the importer does not restore.
        RequestBody::File { .. } | RequestBody::Multipart(_) => None,
    }
}

fn auth_json(auth: &RequestAuth) -> Option<Value> {
    match auth {
        RequestAuth::None => None,
        RequestAuth::Bearer { token } => Some(json!({
            "type": "bearer",
            "bearer": [{ "key": "token", "value": token }],
        })),
        RequestAuth::Basic { username, password } => Some(json!({
            "type": "basic",
            "basic": [
                { "key": "username", "value": username },
                { "key": "password", "value": password },
            ],
        })),
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => Some(json!({
            "type": "apikey",
            "apikey": [
                { "key": "key", "value": key },
                { "key": "value", "value": value },
                {
                    "key": "in",
                    "value": match location {
                        ApiKeyLocation::Header => "header",
                        ApiKeyLocation::Query => "query",
                    },
                },
            ],
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA_21: &str = "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";
    const SCHEMA_20: &str = "https://schema.getpostman.com/json/collection/v2.0.0/collection.json";

    fn document(items: Value) -> String {
        json!({
            "info": { "name": "Petstore", "schema": SCHEMA_21 },
            "item": items,
        })
        .to_string()
    }

    fn parsed(items: Value) -> ImportBundle {
        parse_postman(document(items).as_bytes()).expect("the fixture parses")
    }

    fn request(name: &str, value: Value) -> Value {
        json!({ "name": name, "request": value })
    }

    #[test]
    fn detection_accepts_both_schemas_and_refuses_unrelated_json() {
        let v21 = json!({
            "info": { "name": "Petstore", "schema": SCHEMA_21 },
            "item": [],
        })
        .to_string();
        assert_eq!(
            super::super::sniff("collection.json", v21.as_bytes()),
            Some(super::super::ImportFormat::PostmanCollection)
        );

        let v20 = json!({
            "info": { "name": "Petstore", "schema": SCHEMA_20 },
            "item": [],
        })
        .to_string();
        assert_eq!(
            super::super::sniff("collection.json", v20.as_bytes()),
            Some(super::super::ImportFormat::PostmanCollection)
        );

        let unrelated = br#"{"hello": "world"}"#;
        assert_eq!(
            super::super::sniff("collection.postman_collection.json", unrelated),
            None
        );
        assert_eq!(
            parse_postman(unrelated).unwrap_err(),
            InterchangeError::NotRecognized
        );
    }

    #[test]
    fn nested_groups_folders_and_root_requests_build_the_tree() {
        let bundle = parsed(json!([
            {
                "name": "Animals",
                "item": [{
                    "name": "Dogs",
                    "item": [request("List dogs", json!({
                        "method": "GET",
                        "url": "https://api.example.com/dogs"
                    }))]
                }]
            },
            request("Ping", json!({
                "method": "GET",
                "url": "https://api.example.com/ping"
            }))
        ]));

        let collection = &bundle.collections[0];
        assert_eq!(collection.name, "Petstore");
        let animals = collection
            .folders
            .iter()
            .find(|folder| folder.name == "Animals")
            .expect("root folder");
        let dogs = collection
            .folders
            .iter()
            .find(|folder| folder.name == "Dogs")
            .expect("nested folder");
        assert_eq!(animals.parent_id, None);
        assert_eq!(dogs.parent_id, Some(animals.id));

        let list = collection
            .requests
            .iter()
            .find(|request| request.name == "List dogs")
            .expect("request in the inner folder");
        assert_eq!(list.folder_id, Some(dogs.id));
        let ping = collection
            .requests
            .iter()
            .find(|request| request.name == "Ping")
            .expect("root request");
        assert_eq!(ping.folder_id, None);
    }

    #[test]
    fn duplicate_request_names_in_different_folders_both_import() {
        let bundle = parsed(json!([
            {
                "name": "One",
                "item": [request("Same", json!({
                    "method": "GET", "url": "https://example.com/one"
                }))]
            },
            {
                "name": "Two",
                "item": [request("Same", json!({
                    "method": "GET", "url": "https://example.com/two"
                }))]
            }
        ]));
        let names: Vec<&str> = bundle.collections[0]
            .requests
            .iter()
            .map(|request| request.name.as_str())
            .collect();
        assert_eq!(names, vec!["Same", "Same"]);
    }

    #[test]
    fn a_tree_deeper_than_the_cap_stops_with_a_warning() {
        let mut item = request(
            "Leaf",
            json!({
                "method": "GET", "url": "https://example.com/leaf"
            }),
        );
        // Deep enough to pass the cap, shallow enough that the JSON parser
        // accepts the document — the cap must stay reachable.
        for level in (0..MAX_DEPTH + 5).rev() {
            item = json!({ "name": format!("Level {level}"), "item": [item] });
        }
        let bundle = parsed(json!([item]));

        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.reason.contains(&format!("{MAX_DEPTH} levels"))),
            "the cap is reported: {:?}",
            bundle.warnings
        );
        assert!(
            bundle.collections[0]
                .requests
                .iter()
                .all(|request| request.name != "Leaf"),
            "the deeper subtree is skipped"
        );
    }

    #[test]
    fn headers_query_rows_and_path_variables_are_mapped() {
        let bundle = parsed(json!([request(
            "Search",
            json!({
                "method": "GET",
                "header": [
                    { "key": "Accept", "value": "application/json" },
                    { "key": "X-Debug", "value": "1", "disabled": true }
                ],
                "url": {
                    "raw": "https://api.example.com/users/:id?page=2",
                    "query": [
                        { "key": "page", "value": "2" },
                        { "key": "sort", "value": "name", "disabled": true }
                    ],
                    "variable": [{ "key": "id", "value": "42" }]
                }
            })
        )]));

        let request = &bundle.collections[0].requests[0];
        assert_eq!(request.url, "https://api.example.com/users/{{id}}");
        assert_eq!(request.headers.len(), 2);
        assert_eq!(request.headers[0].key, "Accept");
        assert!(request.headers[0].enabled);
        assert_eq!(request.headers[1].key, "X-Debug");
        assert!(!request.headers[1].enabled);
        assert_eq!(request.query.len(), 2);
        assert_eq!(request.query[0].key, "page");
        assert!(request.query[0].enabled);
        assert_eq!(request.query[1].key, "sort");
        assert!(!request.query[1].enabled);
        assert_eq!(
            bundle.environments[0].vars.get("id").map(String::as_str),
            Some("42"),
            "url.variable[] fills the environment"
        );
    }

    #[test]
    fn a_string_url_has_its_query_split_into_rows() {
        let bundle = parsed(json!([request(
            "Search",
            json!({
                "method": "GET",
                "url": "https://api.example.com/users?page=2&sort=name"
            })
        )]));

        let request = &bundle.collections[0].requests[0];
        assert_eq!(request.url, "https://api.example.com/users");
        let keys: Vec<&str> = request.query.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(keys, vec!["page", "sort"]);
    }

    #[test]
    fn an_unsupported_method_is_reported_and_the_item_is_skipped() {
        let bundle = parsed(json!([request(
            "Trace",
            json!({
                "method": "TRACE",
                "url": "https://example.com/trace"
            })
        )]));

        assert!(bundle.collections[0].requests.is_empty());
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.reason.contains("TRACE")),
            "the method is named: {:?}",
            bundle.warnings
        );
    }

    #[test]
    fn each_body_mode_maps_to_its_request_body() {
        let bundle = parsed(json!([
            request(
                "Json",
                json!({
                    "method": "POST",
                    "url": "https://example.com/json",
                    "body": {
                        "mode": "raw",
                        "raw": "{\"name\":\"Rex\"}",
                        "options": { "raw": { "language": "json" } }
                    }
                })
            ),
            request(
                "JsonHeader",
                json!({
                    "method": "POST",
                    "url": "https://example.com/header",
                    "header": [{ "key": "Content-Type", "value": "application/json" }],
                    "body": { "mode": "raw", "raw": "{\"ok\":true}" }
                })
            ),
            request(
                "Text",
                json!({
                    "method": "POST",
                    "url": "https://example.com/text",
                    "body": { "mode": "raw", "raw": "hello" }
                })
            ),
            request(
                "Form",
                json!({
                    "method": "POST",
                    "url": "https://example.com/form",
                    "body": {
                        "mode": "urlencoded",
                        "urlencoded": [
                            { "key": "name", "value": "Rex" },
                            { "key": "debug", "value": "1", "disabled": true }
                        ]
                    }
                })
            ),
            request(
                "Multipart",
                json!({
                    "method": "POST",
                    "url": "https://example.com/multipart",
                    "body": {
                        "mode": "formdata",
                        "formdata": [
                            { "key": "name", "value": "Rex", "type": "text" },
                            { "key": "photo", "type": "file", "src": "/tmp/rex.png" }
                        ]
                    }
                })
            ),
            request(
                "Graphql",
                json!({
                    "method": "POST",
                    "url": "https://example.com/graphql",
                    "body": { "mode": "graphql" }
                })
            ),
            request(
                "File",
                json!({
                    "method": "POST",
                    "url": "https://example.com/file",
                    "body": { "mode": "file", "file": { "src": "/tmp/rex.png" } }
                })
            )
        ]));

        let by_name = |name: &str| {
            bundle.collections[0]
                .requests
                .iter()
                .find(|request| request.name == name)
                .expect("request")
        };
        assert_eq!(
            by_name("Json").body,
            RequestBody::Json("{\"name\":\"Rex\"}".to_owned())
        );
        assert_eq!(
            by_name("JsonHeader").body,
            RequestBody::Json("{\"ok\":true}".to_owned())
        );
        assert_eq!(by_name("Text").body, RequestBody::Text("hello".to_owned()));
        assert_eq!(
            by_name("Form").body,
            RequestBody::Form(vec![
                KeyValue::new("name", "Rex"),
                KeyValue {
                    key: "debug".to_owned(),
                    value: "1".to_owned(),
                    enabled: false,
                },
            ])
        );
        assert_eq!(
            by_name("Multipart").body,
            RequestBody::Form(vec![KeyValue::new("name", "Rex")])
        );
        assert_eq!(by_name("Graphql").body, RequestBody::None);
        assert_eq!(by_name("File").body, RequestBody::None);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item.contains("photo")),
            "the skipped file part is named: {:?}",
            bundle.warnings
        );
    }

    #[test]
    fn each_supported_auth_variant_maps_and_unsupported_ones_are_reported() {
        let bundle = parsed(json!([
            request(
                "Bearer",
                json!({
                    "method": "GET",
                    "url": "https://example.com/bearer",
                    "auth": { "type": "bearer", "bearer": [{ "key": "token", "value": "secret" }] }
                })
            ),
            request(
                "Basic",
                json!({
                    "method": "GET",
                    "url": "https://example.com/basic",
                    "auth": {
                        "type": "basic",
                        "basic": [
                            { "key": "username", "value": "rex" },
                            { "key": "password", "value": "woof" }
                        ]
                    }
                })
            ),
            request(
                "ApiKeyHeader",
                json!({
                    "method": "GET",
                    "url": "https://example.com/header",
                    "auth": {
                        "type": "apikey",
                        "apikey": [
                            { "key": "key", "value": "X-Api-Key" },
                            { "key": "value", "value": "secret" }
                        ]
                    }
                })
            ),
            request(
                "ApiKeyQuery",
                json!({
                    "method": "GET",
                    "url": "https://example.com/query",
                    "auth": {
                        "type": "apikey",
                        "apikey": [
                            { "key": "key", "value": "api_key" },
                            { "key": "value", "value": "secret" },
                            { "key": "in", "value": "query" }
                        ]
                    }
                })
            ),
            request(
                "NoAuth",
                json!({
                    "method": "GET",
                    "url": "https://example.com/none",
                    "auth": { "type": "noauth" }
                })
            ),
            request(
                "Inherit",
                json!({
                    "method": "GET",
                    "url": "https://example.com/inherit",
                    "auth": { "type": "inherit" }
                })
            ),
            request(
                "Oauth",
                json!({
                    "method": "GET",
                    "url": "https://example.com/oauth",
                    "auth": { "type": "oauth2" }
                })
            )
        ]));

        let by_name = |name: &str| {
            bundle.collections[0]
                .requests
                .iter()
                .find(|request| request.name == name)
                .expect("request")
        };
        assert_eq!(
            by_name("Bearer").auth,
            RequestAuth::Bearer {
                token: "secret".to_owned()
            }
        );
        assert_eq!(
            by_name("Basic").auth,
            RequestAuth::Basic {
                username: "rex".to_owned(),
                password: "woof".to_owned(),
            }
        );
        assert_eq!(
            by_name("ApiKeyHeader").auth,
            RequestAuth::ApiKey {
                key: "X-Api-Key".to_owned(),
                value: "secret".to_owned(),
                location: ApiKeyLocation::Header,
            }
        );
        assert_eq!(
            by_name("ApiKeyQuery").auth,
            RequestAuth::ApiKey {
                key: "api_key".to_owned(),
                value: "secret".to_owned(),
                location: ApiKeyLocation::Query,
            }
        );
        assert_eq!(by_name("NoAuth").auth, RequestAuth::None);
        assert_eq!(by_name("Inherit").auth, RequestAuth::None);
        assert_eq!(by_name("Oauth").auth, RequestAuth::None);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item.contains("oauth2")),
            "the unsupported scheme is named: {:?}",
            bundle.warnings
        );
    }

    #[test]
    fn collection_variables_become_an_environment_and_skip_objects() {
        let document = json!({
            "info": { "name": "Petstore", "schema": SCHEMA_21 },
            "item": [],
            "variable": [
                { "key": "baseUrl", "value": "https://api.example.com" },
                { "key": "count", "value": 3 },
                { "key": "nested", "value": { "a": 1 } }
            ]
        })
        .to_string();
        let bundle = parse_postman(document.as_bytes()).expect("the fixture parses");

        assert_eq!(bundle.environments.len(), 1);
        assert_eq!(bundle.environments[0].name, "Petstore");
        assert_eq!(
            bundle.environments[0]
                .vars
                .get("baseUrl")
                .map(String::as_str),
            Some("https://api.example.com")
        );
        assert_eq!(
            bundle.environments[0].vars.get("count").map(String::as_str),
            Some("3")
        );
        assert!(!bundle.environments[0].vars.contains_key("nested"));
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item.contains("nested"))
        );
    }

    #[test]
    fn no_variables_means_no_environment() {
        let bundle = parsed(json!([]));
        assert!(bundle.environments.is_empty());
    }

    #[test]
    fn repeated_omissions_are_one_summary_line_per_kind() {
        let items: Vec<Value> = (0..3)
            .map(|index| {
                json!({
                    "name": format!("Request {index}"),
                    "request": {
                        "method": "GET",
                        "url": format!("https://example.com/{index}"),
                        "description": "a request"
                    },
                    "description": "an item",
                    "event": [
                        { "listen": "prerequest", "script": { "exec": ["one"] } },
                        { "listen": "test", "script": { "exec": ["two"] } }
                    ],
                    "response": [{ "name": "example" }]
                })
            })
            .collect();
        let document = json!({
            "info": {
                "name": "Petstore",
                "schema": SCHEMA_21,
                "description": "the collection"
            },
            // A collection's scripts live at the document root.
            "event": [{ "listen": "prerequest", "script": { "exec": ["setup"] } }],
            "item": items,
            "auth": { "type": "bearer", "bearer": [] }
        })
        .to_string();
        let bundle = parse_postman(document.as_bytes()).expect("the fixture parses");

        let script_lines: Vec<&ImportWarning> = bundle
            .warnings
            .iter()
            .filter(|warning| {
                warning.item.contains("pre-request script") || warning.item.contains("test script")
            })
            .collect();
        assert_eq!(
            script_lines.len(),
            2,
            "one line for each script kind: {:?}",
            bundle.warnings
        );
        assert!(
            script_lines[0].item.starts_with('4'),
            "the collection's script counts too: {:?}",
            script_lines[0]
        );

        let description_lines = bundle
            .warnings
            .iter()
            .filter(|warning| warning.item.contains("description"))
            .count();
        assert_eq!(description_lines, 1);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item.contains("collection-level auth")),
            "collection auth is reported, not copied into requests"
        );
        assert_eq!(bundle.collections[0].requests.len(), 3);
    }

    #[test]
    fn export_writes_a_v21_tree_with_every_request_field() {
        let folder = Folder::new("Animals", None);
        let mut request = RequestSpec::new("List animals");
        request.url = "https://api.example.com/animals".to_owned();
        request.folder_id = Some(folder.id);
        request.headers = vec![KeyValue {
            key: "X-Debug".to_owned(),
            value: "1".to_owned(),
            enabled: false,
        }];
        request.query = vec![
            KeyValue::new("page", "2"),
            KeyValue {
                key: "sort".to_owned(),
                value: "name".to_owned(),
                enabled: false,
            },
        ];
        request.body = RequestBody::Json(r#"{"name":"Rex"}"#.to_owned());
        request.auth = RequestAuth::Bearer {
            token: "secret".to_owned(),
        };
        let collection = BundleCollection {
            name: "Petstore".to_owned(),
            order: 0,
            folders: vec![folder],
            requests: vec![request],
        };

        let rendered = render_postman(&collection);
        let document: Value = serde_json::from_str(&rendered).expect("rendered JSON");
        assert_eq!(
            document["info"]["schema"],
            Value::String(SCHEMA_21.to_owned())
        );
        assert!(
            document["info"]["_postman_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(
            document.get("variable").is_none(),
            "variables are not invented"
        );

        let folder_item = &document["item"][0];
        assert_eq!(folder_item["name"], "Animals");
        let request_item = &folder_item["item"][0];
        assert_eq!(request_item["name"], "List animals");
        assert_eq!(request_item["request"]["method"], "GET");
        assert_eq!(
            request_item["request"]["header"][0]["disabled"],
            Value::Bool(true)
        );
        assert_eq!(
            request_item["request"]["url"]["raw"],
            "https://api.example.com/animals?page=2"
        );
        assert_eq!(
            request_item["request"]["url"]["query"][1]["disabled"],
            Value::Bool(true)
        );
        assert_eq!(request_item["request"]["body"]["mode"], "raw");
        assert_eq!(
            request_item["request"]["body"]["options"]["raw"]["language"],
            "json"
        );
        assert_eq!(request_item["request"]["auth"]["type"], "bearer");
        assert_eq!(
            request_item["request"]["auth"]["bearer"][0]["value"],
            "secret"
        );
    }

    #[test]
    fn a_placeholder_url_still_carries_its_query_rows() {
        let mut request = RequestSpec::new("Ping");
        request.url = "{{base_url}}/ping".to_owned();
        request.query = vec![KeyValue::new("page", "2")];
        let collection = BundleCollection {
            name: "Petstore".to_owned(),
            order: 0,
            folders: Vec::new(),
            requests: vec![request],
        };

        let document: Value = serde_json::from_str(&render_postman(&collection)).unwrap();
        let url = &document["item"][0]["request"]["url"];
        assert_eq!(url["raw"], "{{base_url}}/ping?page=2");
        assert_eq!(url["query"][0]["key"], "page");
        assert!(
            url.get("host").is_none(),
            "a placeholder URL cannot be parsed"
        );
    }

    #[test]
    fn an_exported_collection_round_trips_without_omissions() {
        let folder = Folder::new("Animals", None);
        let mut request = RequestSpec::new("Create animal");
        request.method = HttpMethod::Post;
        request.url = "https://api.example.com/animals".to_owned();
        request.folder_id = Some(folder.id);
        request.headers = vec![KeyValue {
            key: "X-Debug".to_owned(),
            value: "1".to_owned(),
            enabled: false,
        }];
        request.query = vec![KeyValue {
            key: "dry".to_owned(),
            value: "true".to_owned(),
            enabled: false,
        }];
        request.body = RequestBody::Json(r#"{"name":"Rex"}"#.to_owned());
        request.auth = RequestAuth::Bearer {
            token: "secret".to_owned(),
        };
        let collection = BundleCollection {
            name: "Petstore".to_owned(),
            order: 0,
            folders: vec![folder],
            requests: vec![request.clone()],
        };

        let rendered = render_postman(&collection);
        let imported = parse_postman(rendered.as_bytes()).expect("the export parses");
        assert!(imported.warnings.is_empty(), "{:?}", imported.warnings);
        let reimported = &imported.collections[0];
        assert_eq!(reimported.folders.len(), 1);
        assert_eq!(reimported.requests.len(), 1);
        let spec = &reimported.requests[0];
        assert_eq!(spec.name, "Create animal");
        assert_eq!(spec.method, HttpMethod::Post);
        assert_eq!(spec.url, request.url);
        assert_eq!(spec.folder_id, Some(reimported.folders[0].id));
        assert_eq!(spec.headers, request.headers);
        assert_eq!(spec.query, request.query);
        assert_eq!(spec.body, request.body);
        assert_eq!(spec.auth, request.auth);
    }

    #[test]
    fn render_assigns_a_fresh_id_to_every_export() {
        let collection = BundleCollection {
            name: "Petstore".to_owned(),
            order: 0,
            folders: Vec::new(),
            requests: Vec::new(),
        };
        let first: Value = serde_json::from_str(&render_postman(&collection)).unwrap();
        let second: Value = serde_json::from_str(&render_postman(&collection)).unwrap();
        assert_ne!(first["info"]["_postman_id"], second["info"]["_postman_id"]);
    }

    /// The guide's Postman example is the format's public documentation, so it
    /// must be a document this build actually accepts.
    #[test]
    fn the_guide_postman_example_parses() {
        let guide = include_str!("../../../../docs/guide.md");
        let section = guide
            .split("### Postman collections")
            .nth(1)
            .expect("the guide documents the Postman format");
        let block = section
            .split("```json")
            .nth(1)
            .expect("the section contains a JSON example");
        let document = block
            .split("```")
            .next()
            .expect("the example fence is closed");
        let format = super::super::sniff("guide.postman_collection.json", document.as_bytes())
            .expect("the example is recognizable");
        let bundle = super::super::parse(format, document.as_bytes()).expect("the example parses");
        assert_eq!(bundle.collections[0].name, "Petstore");
        assert_eq!(bundle.collections[0].requests.len(), 1);
        assert_eq!(bundle.collections[0].requests[0].name, "List animals");
        assert_eq!(
            bundle.environments[0]
                .vars
                .get("base_url")
                .map(String::as_str),
            Some("https://api.example.com")
        );
    }
}
