//! A read-only description of the request Send would perform.
//!
//! [`Inspection`] is built from a draft, the variables Send would resolve, and
//! the live cookie jar. Building it never sends, writes a cookie, records
//! history, or mutates the draft, and every secret value is replaced while the
//! model is built, so a view cannot reveal a value it never receives.
//!
//! The composition itself comes from [`crate::http`], the same rule set Send
//! and the script preview consume. The inspected variant adds each header's
//! origin and the transport defaults the editor already lists.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use url::form_urlencoded;

use crate::cookies::CookieJar;
use crate::http::{
    ComposedHeader, HTTP_SCHEMES, HeaderOrigin, WS_SCHEMES, body_length,
    compose_canonical_lenient_inspected_with_schemes, resolve_upload_path, validate_header,
    validate_json_body,
};
use crate::model::{
    ApiKeyLocation, MultipartPart, RequestAuth, RequestBody, RequestProtocol, RequestSpec,
};
use crate::variables::{Variables, resolve_request_lossy};

/// The placeholder a hidden credential leaves in the model.
pub const MASKED_VALUE: &str = "••••";

/// Everything one inspection needs.
///
/// The draft is unresolved; resolution runs inside [`Inspection::build`] so the
/// reported names are exactly the ones that lacked a value. `assets_root` is
/// the workspace root a relative upload path resolves against.
pub struct InspectionInput {
    pub spec: RequestSpec,
    pub vars: Variables,
    pub jar: Arc<CookieJar>,
    pub assets_root: Option<PathBuf>,
}

/// The resolved request, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inspection {
    pub method: String,
    /// The composed URL with credential query values masked.
    pub display_url: String,
    pub headers: Vec<InspectedHeader>,
    pub body: BodySummary,
    pub jar: JarRole,
    /// The subprotocols a WebSocket handshake offers, in order.
    pub subprotocols: Vec<String>,
    /// The placeholder names that had no value, sorted and unique.
    pub unresolved: Vec<String>,
    pub problems: Vec<InspectionProblem>,
    /// Whether Send would accept this draft as it stands.
    pub can_send: bool,
}

/// One header in the inspected request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InspectedHeader {
    pub name: String,
    /// The value, replaced by [`MASKED_VALUE`] when it is a credential.
    pub value: String,
    pub origin: HeaderOrigin,
}

/// What the session jar contributes to the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum JarRole {
    /// No cookie matches, and no manual header suppresses the jar.
    None,
    /// These cookie names would be attached, in sorted order.
    Attached { names: Vec<String> },
    /// An enabled `Cookie` row suppresses the jar.
    Suppressed,
}

/// The body in one line: its kind, its size when Send knows it before sending,
/// and the upload path for a single-file body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BodySummary {
    pub kind: BodyKind,
    pub size: Option<u64>,
    pub upload_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BodyKind {
    None,
    Json,
    Text,
    Form,
    File,
    Multipart,
}

/// One reason Send would reject the draft, or would fail before sending it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum InspectionProblem {
    EmptyUrl,
    InvalidUrl {
        message: String,
    },
    InvalidHeader {
        name: String,
        message: String,
    },
    InvalidJson {
        message: String,
    },
    MissingUpload {
        /// The field that names the upload: a path, or `key: path` for a
        /// multipart part.
        label: String,
        reason: UploadProblem,
    },
}

/// Why an upload cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum UploadProblem {
    NoFileChosen,
    NotAFile,
    Unavailable { message: String },
}

impl Inspection {
    /// Resolves `input`, composes the request, and masks every credential.
    ///
    /// File metadata is read here, so callers run this away from the UI thread.
    pub fn build(input: InspectionInput) -> Self {
        let InspectionInput {
            spec,
            vars,
            jar,
            assets_root,
        } = input;
        let (resolved, unresolved) = resolve_request_lossy(&spec, &vars);
        let schemes = match resolved.protocol {
            RequestProtocol::Http => HTTP_SCHEMES,
            RequestProtocol::WebSocket => WS_SCHEMES,
        };
        // A WebSocket handshake carries no body; a body kept from an HTTP draft
        // must not appear here any more than it appears on the wire.
        let probe = match resolved.protocol {
            RequestProtocol::Http => resolved.clone(),
            RequestProtocol::WebSocket => {
                let mut probe = resolved.clone();
                probe.body = RequestBody::None;
                probe
            }
        };
        let (canonical, composed, _notice) = compose_canonical_lenient_inspected_with_schemes(
            &probe,
            schemes,
            assets_root.as_deref(),
        );

        let mut problems = Vec::new();
        if probe.url.trim().is_empty() {
            problems.push(InspectionProblem::EmptyUrl);
        } else if !probe.url.contains("{{")
            && let Err(error) = crate::http::parse_url_with_schemes(&probe.url, schemes)
        {
            problems.push(InspectionProblem::InvalidUrl {
                message: error.to_string(),
            });
        }
        for header in &composed {
            if let Err(error) = validate_header(&header.name, &header.value) {
                problems.push(InspectionProblem::InvalidHeader {
                    name: header.name.clone(),
                    message: error.to_string(),
                });
            }
        }
        if probe.protocol == RequestProtocol::Http
            && let RequestBody::Json(body) = &probe.body
            && let Err(error) = validate_json_body(body)
        {
            problems.push(InspectionProblem::InvalidJson {
                message: error.to_string(),
            });
        }
        collect_upload_problems(&probe, assets_root.as_deref(), &mut problems);

        let headers: Vec<InspectedHeader> = composed
            .iter()
            .map(|header| InspectedHeader {
                name: header.name.clone(),
                value: masked_value(header),
                origin: header.origin,
            })
            .collect();
        let display_url = mask_query_credential(&canonical.url, &resolved.auth);
        let jar_role = if has_manual_cookie_header(&probe) {
            JarRole::Suppressed
        } else {
            let names = jar.matching_names(&canonical.url);
            if names.is_empty() {
                JarRole::None
            } else {
                JarRole::Attached { names }
            }
        };
        let (kind, size, upload_path) = body_summary(&probe.body, assets_root.as_deref());
        let can_send = unresolved.is_empty() && problems.is_empty();

        Self {
            method: probe.method.as_str().to_owned(),
            display_url,
            headers,
            body: BodySummary {
                kind,
                size,
                upload_path,
            },
            jar: jar_role,
            subprotocols: match resolved.protocol {
                RequestProtocol::Http => Vec::new(),
                RequestProtocol::WebSocket => resolved.subprotocols.clone(),
            },
            unresolved,
            can_send,
            problems,
        }
    }
}

fn masked_value(header: &ComposedHeader) -> String {
    const CREDENTIAL_NAMES: [&str; 3] = ["authorization", "proxy-authorization", "cookie"];
    let credential = header.origin == HeaderOrigin::Auth
        || CREDENTIAL_NAMES
            .iter()
            .any(|name| header.name.eq_ignore_ascii_case(name));
    if credential {
        MASKED_VALUE.to_owned()
    } else {
        header.value.clone()
    }
}

/// Replaces the value of the active API key's query parameter, keeping the
/// parameter name and every other pair. A URL with no such value is returned
/// unchanged, down to its original encoding.
fn mask_query_credential(url: &str, auth: &RequestAuth) -> String {
    let RequestAuth::ApiKey {
        key,
        value,
        location: ApiKeyLocation::Query,
    } = auth
    else {
        return url.to_owned();
    };
    if key.is_empty() || value.is_empty() {
        return url.to_owned();
    }
    let (before_fragment, fragment) = match url.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (url, None),
    };
    let Some((base, query)) = before_fragment.split_once('?') else {
        return url.to_owned();
    };
    // Decoding first means the name matches whatever encoding the composer
    // chose; the rebuilt URL only replaces the pairs and is never shown with a
    // raw value either way.
    let pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let is_credential = |name: &str, value: &str| name == key && !value.is_empty();
    if !pairs.iter().any(|(name, value)| is_credential(name, value)) {
        return url.to_owned();
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in pairs {
        if is_credential(&name, &value) {
            serializer.append_pair(&name, MASKED_VALUE);
        } else {
            serializer.append_pair(&name, &value);
        }
    }
    let mut result = format!("{base}?{}", serializer.finish());
    if let Some(fragment) = fragment {
        result.push('#');
        result.push_str(fragment);
    }
    result
}

fn has_manual_cookie_header(spec: &RequestSpec) -> bool {
    spec.headers
        .iter()
        .any(|row| row.enabled && !row.key.is_empty() && row.key.eq_ignore_ascii_case("cookie"))
}

fn body_summary(
    body: &RequestBody,
    assets_root: Option<&Path>,
) -> (BodyKind, Option<u64>, Option<String>) {
    match body {
        RequestBody::None => (BodyKind::None, None, None),
        RequestBody::Json(_) => (BodyKind::Json, body_length(body, assets_root), None),
        RequestBody::Text(_) => (BodyKind::Text, body_length(body, assets_root), None),
        RequestBody::Form(_) => (BodyKind::Form, body_length(body, assets_root), None),
        RequestBody::File { path } => (
            BodyKind::File,
            body_length(body, assets_root),
            Some(path.clone()),
        ),
        // Multipart framing and the boundary length are unknown until reqwest
        // builds the form.
        RequestBody::Multipart(_) => (BodyKind::Multipart, None, None),
    }
}

/// Reports every upload Send's preflight would reject: the single-file body,
/// then each enabled multipart file part with a name.
fn collect_upload_problems(
    spec: &RequestSpec,
    assets_root: Option<&Path>,
    problems: &mut Vec<InspectionProblem>,
) {
    let check = |path: &str, label: String, problems: &mut Vec<InspectionProblem>| {
        if let Some(reason) = upload_problem(path, assets_root) {
            problems.push(InspectionProblem::MissingUpload { label, reason });
        }
    };
    match &spec.body {
        RequestBody::File { path } => check(path, path.clone(), problems),
        RequestBody::Multipart(parts) => {
            for part in parts {
                let MultipartPart::File { key, path, enabled } = part else {
                    continue;
                };
                if *enabled && !key.is_empty() {
                    check(path, format!("{key}: {path}"), problems);
                }
            }
        }
        _ => {}
    }
}

fn upload_problem(path: &str, assets_root: Option<&Path>) -> Option<UploadProblem> {
    if path.trim().is_empty() {
        return Some(UploadProblem::NoFileChosen);
    }
    let resolved = match resolve_upload_path(path, assets_root) {
        Ok(resolved) => resolved,
        Err(error) => {
            return Some(UploadProblem::Unavailable {
                message: error.to_string(),
            });
        }
    };
    match std::fs::metadata(&resolved) {
        Ok(metadata) if metadata.is_file() => None,
        Ok(_) => Some(UploadProblem::NotAFile),
        Err(error) => Some(UploadProblem::Unavailable {
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use url::Url;

    use super::*;
    use crate::model::{HttpMethod, KeyValue};

    fn vars() -> Variables {
        Variables::defaults_then(&BTreeMap::new(), None)
    }

    fn input(spec: RequestSpec) -> InspectionInput {
        InspectionInput {
            spec,
            vars: vars(),
            jar: Arc::new(CookieJar::session_only("test")),
            assets_root: None,
        }
    }

    fn json_request() -> RequestSpec {
        let mut spec = RequestSpec::new("inspect");
        spec.url = "https://example.test/items".to_owned();
        spec.method = HttpMethod::Post;
        spec.body = RequestBody::Json(r#"{"ok":true}"#.to_owned());
        spec
    }

    #[test]
    fn origins_cover_rows_auth_defaults_and_transport_rows() {
        let mut spec = json_request();
        spec.headers.push(KeyValue::new("X-Trace", "1"));
        spec.auth = RequestAuth::Bearer {
            token: "secret-token".to_owned(),
        };

        let inspection = Inspection::build(input(spec));
        let header = |name: &str| {
            inspection
                .headers
                .iter()
                .find(|header| header.name.eq_ignore_ascii_case(name))
                .unwrap_or_else(|| panic!("{name} is listed"))
        };

        assert_eq!(header("X-Trace").origin, HeaderOrigin::User);
        assert_eq!(header("Authorization").origin, HeaderOrigin::Auth);
        assert_eq!(header("Content-Type").origin, HeaderOrigin::Generated);
        assert_eq!(header("Host").origin, HeaderOrigin::Generated);
        assert_eq!(header("Content-Length").origin, HeaderOrigin::Generated);
        assert_eq!(header("Accept-Encoding").origin, HeaderOrigin::Generated);
        assert_eq!(
            inspection
                .headers
                .iter()
                .filter(|header| header.name.eq_ignore_ascii_case("authorization"))
                .count(),
            1,
            "auth appears once"
        );
    }

    #[test]
    fn a_user_row_suppresses_the_same_generated_default() {
        let mut spec = json_request();
        spec.headers.push(KeyValue::new("Host", "mirror.test"));

        let inspection = Inspection::build(input(spec));
        let hosts: Vec<&InspectedHeader> = inspection
            .headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case("host"))
            .collect();

        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].origin, HeaderOrigin::User);
        assert_eq!(hosts[0].value, "mirror.test");
    }

    #[test]
    fn a_user_authorization_row_wins_over_the_credential() {
        let mut spec = json_request();
        spec.headers
            .push(KeyValue::new("Authorization", "Bearer row-token"));
        spec.auth = RequestAuth::Bearer {
            token: "secret-token".to_owned(),
        };

        let inspection = Inspection::build(input(spec));
        let auths: Vec<&InspectedHeader> = inspection
            .headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case("authorization"))
            .collect();

        assert_eq!(auths.len(), 1, "the credential adds no second row");
        assert_eq!(auths[0].origin, HeaderOrigin::User);
        assert_eq!(
            auths[0].value, MASKED_VALUE,
            "a user credential is masked too"
        );
    }

    #[test]
    fn credentials_are_masked_in_headers_and_query() {
        let mut spec = json_request();
        spec.auth = RequestAuth::ApiKey {
            key: "api_key".to_owned(),
            value: "sk-live-secret".to_owned(),
            location: ApiKeyLocation::Query,
        };
        let inspection = Inspection::build(input(spec));

        assert!(
            inspection.display_url.contains("api_key=••••"),
            "the parameter name stays and the value is hidden: {}",
            inspection.display_url
        );
        assert!(!inspection.display_url.contains("sk-live-secret"));

        let mut spec = json_request();
        spec.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".to_owned(),
            value: "sk-live-secret".to_owned(),
            location: ApiKeyLocation::Header,
        };
        let inspection = Inspection::build(input(spec));
        let key = inspection
            .headers
            .iter()
            .find(|header| header.name == "X-Api-Key")
            .expect("the key header is listed");
        assert_eq!(key.value, MASKED_VALUE);

        let mut spec = json_request();
        spec.headers
            .push(KeyValue::new("Proxy-Authorization", "Basic abc"));
        spec.headers.push(KeyValue::new("Cookie", "sid=secret"));
        let inspection = Inspection::build(input(spec));
        for name in ["Proxy-Authorization", "Cookie"] {
            let header = inspection
                .headers
                .iter()
                .find(|header| header.name == name)
                .expect("the row is listed");
            assert_eq!(header.value, MASKED_VALUE);
        }
    }

    #[test]
    fn basic_auth_never_leaks_its_password() {
        let mut spec = json_request();
        spec.auth = RequestAuth::Basic {
            username: "alice".to_owned(),
            password: "hunter2".to_owned(),
        };
        let inspection = Inspection::build(input(spec));
        let encoded = serde_json::to_string(&inspection).expect("the model serializes");

        assert!(!encoded.contains("hunter2"));
        assert!(!encoded.contains("alice"));
    }

    #[test]
    fn unresolved_names_are_reported_and_kept_readable() {
        let mut spec = json_request();
        spec.url = "https://{{host}}/items".to_owned();

        let inspection = Inspection::build(input(spec));

        assert_eq!(inspection.unresolved, vec!["host".to_owned()]);
        assert!(!inspection.can_send);
        assert!(
            inspection.display_url.contains("{{host}}"),
            "braces stay readable: {}",
            inspection.display_url
        );
        assert!(!inspection.display_url.contains("%7B"));
    }

    #[test]
    fn local_failures_are_collected() {
        let mut spec = RequestSpec::new("inspect");
        let inspection = Inspection::build(input(spec.clone()));
        assert!(matches!(
            inspection.problems.as_slice(),
            [InspectionProblem::EmptyUrl]
        ));

        spec.url = "not a url".to_owned();
        let inspection = Inspection::build(input(spec));
        assert!(matches!(
            inspection.problems.as_slice(),
            [InspectionProblem::InvalidUrl { .. }]
        ));

        let mut spec = json_request();
        spec.headers.push(KeyValue::new("Bad Header", "1"));
        let inspection = Inspection::build(input(spec));
        assert!(matches!(
            inspection.problems.as_slice(),
            [InspectionProblem::InvalidHeader { name, .. }] if name == "Bad Header"
        ));

        let mut spec = json_request();
        spec.body = RequestBody::Json("{not json".to_owned());
        let inspection = Inspection::build(input(spec));
        assert!(matches!(
            inspection.problems.as_slice(),
            [InspectionProblem::InvalidJson { .. }]
        ));

        let mut spec = json_request();
        spec.body = RequestBody::File {
            path: "missing.bin".to_owned(),
        };
        let inspection = Inspection::build(input(spec));
        assert!(
            matches!(
                inspection.problems.as_slice(),
                [InspectionProblem::MissingUpload { reason, .. }]
                    if matches!(reason, UploadProblem::Unavailable { .. })
            ),
            "{:?}",
            inspection.problems
        );
        assert_eq!(
            inspection.body.size, None,
            "an unreadable upload has no size"
        );
    }

    #[test]
    fn body_summaries_name_kind_size_and_upload_path() {
        let mut spec = json_request();
        spec.body = RequestBody::Json("{\"a\":1}".to_owned());
        let inspection = Inspection::build(input(spec));
        assert_eq!(inspection.body.kind, BodyKind::Json);
        assert_eq!(inspection.body.size, Some(7));

        let mut spec = json_request();
        spec.body = RequestBody::File {
            path: "upload.bin".to_owned(),
        };
        let inspection = Inspection::build(input(spec));
        assert_eq!(inspection.body.kind, BodyKind::File);
        assert_eq!(inspection.body.upload_path.as_deref(), Some("upload.bin"));

        let mut spec = json_request();
        spec.body = RequestBody::Multipart(Vec::new());
        let inspection = Inspection::build(input(spec));
        assert_eq!(inspection.body.kind, BodyKind::Multipart);
        assert_eq!(inspection.body.size, None, "the framing is unknown");
    }

    #[test]
    fn a_present_upload_is_sized_from_its_metadata() {
        let dir = tempfile::tempdir().expect("temp upload");
        let path = dir.path().join("upload.bin");
        std::fs::write(&path, b"0123456789").expect("write upload");

        let mut spec = json_request();
        spec.body = RequestBody::File {
            path: path.to_string_lossy().into_owned(),
        };
        let inspection = Inspection::build(input(spec));

        assert_eq!(inspection.body.kind, BodyKind::File);
        assert_eq!(inspection.body.size, Some(10));
        let length = inspection
            .headers
            .iter()
            .find(|header| header.name == "Content-Length")
            .expect("a known size is listed");
        assert_eq!(length.value, "10");
        assert_eq!(length.origin, HeaderOrigin::Generated);
        assert!(inspection.problems.is_empty(), "{:?}", inspection.problems);
        assert!(inspection.can_send);

        // A directory is not an upload even though it exists.
        let mut spec = json_request();
        spec.body = RequestBody::File {
            path: dir.path().to_string_lossy().into_owned(),
        };
        let inspection = Inspection::build(input(spec));
        assert!(
            matches!(
                inspection.problems.as_slice(),
                [InspectionProblem::MissingUpload {
                    reason: UploadProblem::NotAFile,
                    ..
                }]
            ),
            "{:?}",
            inspection.problems
        );
        assert_eq!(inspection.body.size, None);
    }

    #[test]
    fn the_jar_reports_names_and_a_manual_header_suppresses_it() {
        let jar = Arc::new(CookieJar::session_only("test"));
        let url = Url::parse("https://example.test/items").expect("valid url");
        jar.add(&url, "sid", "cookie-secret")
            .expect("cookie stored");
        jar.add(&url, "theme", "dark").expect("cookie stored");
        let revision = jar.revision();

        let inspection = Inspection::build(InspectionInput {
            spec: json_request(),
            vars: vars(),
            jar: jar.clone(),
            assets_root: None,
        });
        assert_eq!(
            inspection.jar,
            JarRole::Attached {
                names: vec!["sid".to_owned(), "theme".to_owned()]
            }
        );
        let encoded = serde_json::to_string(&inspection).expect("the model serializes");
        assert!(!encoded.contains("cookie-secret"));
        assert_eq!(jar.revision(), revision, "inspection never writes the jar");

        let mut spec = json_request();
        spec.headers.push(KeyValue::new("Cookie", "sid=secret"));
        let inspection = Inspection::build(InspectionInput {
            spec,
            vars: vars(),
            jar,
            assets_root: None,
        });
        assert_eq!(inspection.jar, JarRole::Suppressed);
    }

    #[test]
    fn a_websocket_draft_reports_the_handshake_without_a_body() {
        let mut spec = RequestSpec::new("inspect");
        spec.protocol = RequestProtocol::WebSocket;
        spec.url = "wss://example.test/socket".to_owned();
        spec.subprotocols = vec!["graphql-ws".to_owned()];
        spec.body = RequestBody::Json("{\"ignored\":true}".to_owned());

        let inspection = Inspection::build(input(spec));

        assert_eq!(inspection.method, "GET");
        assert_eq!(inspection.display_url, "wss://example.test/socket");
        assert_eq!(inspection.subprotocols, vec!["graphql-ws".to_owned()]);
        assert_eq!(inspection.body.kind, BodyKind::None);
        assert!(inspection.can_send);
    }

    /// One fixture the accuracy tests send to a capture server.
    ///
    /// `secrets` maps a credential's header or query name to the value the
    /// wire must carry while the inspector shows [`MASKED_VALUE`].
    /// `wire_extras` names headers the transport writes that the inspector
    /// cannot know before sending, such as a multipart `Content-Length`.
    struct WireFixture {
        label: &'static str,
        spec: RequestSpec,
        secrets: Vec<(&'static str, String)>,
        jar_cookies: Vec<(&'static str, &'static str)>,
        wire_extras: Vec<&'static str>,
    }

    fn wire_fixtures(url: &str, upload: &str) -> Vec<WireFixture> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let mut fixtures = Vec::new();

        let mut bearer = json_request();
        bearer.url = url.to_owned();
        bearer.headers.push(KeyValue::new("X-Trace", "trace-1"));
        bearer.auth = RequestAuth::Bearer {
            token: "bearer-secret".to_owned(),
        };
        fixtures.push(WireFixture {
            label: "bearer",
            spec: bearer,
            secrets: vec![("authorization", "Bearer bearer-secret".to_owned())],
            jar_cookies: Vec::new(),
            wire_extras: Vec::new(),
        });

        let mut basic = json_request();
        basic.url = url.to_owned();
        basic.auth = RequestAuth::Basic {
            username: "alice".to_owned(),
            password: "hunter2".to_owned(),
        };
        let basic_wire = format!("Basic {}", STANDARD.encode("alice:hunter2"));
        fixtures.push(WireFixture {
            label: "basic",
            spec: basic,
            secrets: vec![("authorization", basic_wire)],
            jar_cookies: Vec::new(),
            wire_extras: Vec::new(),
        });

        let mut api_key_header = json_request();
        api_key_header.url = url.to_owned();
        api_key_header.auth = RequestAuth::ApiKey {
            key: "X-Api-Key".to_owned(),
            value: "sk-header-secret".to_owned(),
            location: ApiKeyLocation::Header,
        };
        fixtures.push(WireFixture {
            label: "api key header",
            spec: api_key_header,
            secrets: vec![("x-api-key", "sk-header-secret".to_owned())],
            jar_cookies: Vec::new(),
            wire_extras: Vec::new(),
        });

        let mut api_key_query = json_request();
        api_key_query.url = url.to_owned();
        api_key_query.auth = RequestAuth::ApiKey {
            key: "api_key".to_owned(),
            value: "sk-query-secret".to_owned(),
            location: ApiKeyLocation::Query,
        };
        fixtures.push(WireFixture {
            label: "api key query",
            spec: api_key_query,
            secrets: vec![("api_key", "sk-query-secret".to_owned())],
            jar_cookies: Vec::new(),
            wire_extras: Vec::new(),
        });

        let mut manual_cookie = json_request();
        manual_cookie.url = url.to_owned();
        manual_cookie
            .headers
            .push(KeyValue::new("Cookie", "sid=manual-secret"));
        fixtures.push(WireFixture {
            label: "manual cookie",
            spec: manual_cookie,
            secrets: vec![("cookie", "sid=manual-secret".to_owned())],
            jar_cookies: Vec::new(),
            wire_extras: Vec::new(),
        });

        let mut jar = json_request();
        jar.url = url.to_owned();
        fixtures.push(WireFixture {
            label: "jar cookies",
            spec: jar,
            secrets: Vec::new(),
            jar_cookies: vec![("sid", "jar-secret"), ("theme", "dark")],
            wire_extras: vec!["cookie"],
        });

        let mut multipart = json_request();
        multipart.url = url.to_owned();
        multipart.body = RequestBody::Multipart(vec![
            MultipartPart::text("note", "hello"),
            MultipartPart::file("file", upload),
        ]);
        fixtures.push(WireFixture {
            label: "multipart",
            spec: multipart,
            secrets: Vec::new(),
            jar_cookies: Vec::new(),
            // The boundary, and the length that depends on it, exist only once
            // reqwest builds the form; the inspector says so instead.
            wire_extras: vec!["content-length"],
        });

        fixtures
    }

    /// A capture server that records each request head and answers `200`.
    struct CaptureServer {
        addr: std::net::SocketAddr,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl CaptureServer {
        fn spawn() -> Self {
            use std::io::Write;
            use std::sync::{Arc, Mutex};

            let listener =
                std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind capture server");
            let addr = listener.local_addr().expect("capture server address");
            let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let captured = requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        break;
                    };
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    let Ok(head) = read_head(&mut stream) else {
                        continue;
                    };
                    captured.lock().expect("requests lock").push(head);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    );
                    let _ = stream.flush();
                }
            });
            Self { addr, requests }
        }

        fn target(&self) -> String {
            format!("http://{}", self.addr)
        }

        /// Removes and returns the newest captured head.
        fn take(&self) -> String {
            self.requests
                .lock()
                .expect("requests lock")
                .pop()
                .expect("a captured request")
        }
    }

    /// Reads a request's head, draining the body so the client can finish.
    fn read_head(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
        use std::io::Read;

        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 4096];
        let head_end = loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(String::from_utf8_lossy(&buffer).into_owned());
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut remaining = length.saturating_sub(buffer.len() - head_end);
        while remaining > 0 {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            remaining = remaining.saturating_sub(read);
        }
        Ok(head)
    }

    fn wire_parts(wire: &str) -> (String, String, Vec<(String, String)>) {
        let mut lines = wire.lines();
        let request_line = lines.next().expect("a request line");
        let mut words = request_line.split(' ');
        let method = words.next().unwrap_or_default().to_owned();
        let target = words.next().unwrap_or_default().to_owned();
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        (method, target, headers)
    }

    fn encoding_tokens(value: &str) -> Vec<String> {
        let mut tokens: Vec<String> = value
            .split(',')
            .map(|token| token.trim().to_ascii_lowercase())
            .filter(|token| !token.is_empty())
            .collect();
        tokens.sort();
        tokens.dedup();
        tokens
    }

    fn query_pairs(url: &Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect()
    }

    /// The inspector's URL must describe the wire's target, with credentials
    /// shown as [`MASKED_VALUE`] and their wire values named by `secrets`.
    fn assert_url_matches(
        label: &str,
        inspection: &Inspection,
        target: &str,
        secrets: &[(&str, String)],
    ) {
        let shown = Url::parse(&inspection.display_url).expect("the display URL parses");
        let wire = Url::parse(&format!("http://wire.invalid{target}")).expect("the target parses");
        assert_eq!(shown.path(), wire.path(), "{label}: path");

        let shown_pairs = query_pairs(&shown);
        let wire_pairs = query_pairs(&wire);
        assert_eq!(shown_pairs.len(), wire_pairs.len(), "{label}: query pairs");
        for ((shown_name, shown_value), (wire_name, wire_value)) in
            shown_pairs.iter().zip(&wire_pairs)
        {
            assert_eq!(shown_name, wire_name, "{label}: query name");
            if shown_value == MASKED_VALUE {
                let secret = secrets
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(shown_name))
                    .unwrap_or_else(|| panic!("{label}: {shown_name} is a named credential"));
                assert_eq!(wire_value, &secret.1, "{label}: {shown_name} value");
            } else {
                assert_eq!(shown_value, wire_value, "{label}: {shown_name} value");
            }
        }
    }

    fn assert_headers_match(
        label: &str,
        inspection: &Inspection,
        headers: &[(String, String)],
        secrets: &[(&str, String)],
        wire_extras: &[&str],
    ) {
        for header in &inspection.headers {
            let (_, wire_value) = headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&header.name))
                .unwrap_or_else(|| panic!("{label}: the wire carries {}", header.name));
            if header.value == MASKED_VALUE {
                let secret = secrets
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&header.name))
                    .unwrap_or_else(|| panic!("{label}: {} is a named credential", header.name));
                assert_eq!(wire_value, &secret.1, "{label}: {} value", header.name);
            } else if header.name.eq_ignore_ascii_case("accept-encoding") {
                assert_eq!(
                    encoding_tokens(wire_value),
                    encoding_tokens(&header.value),
                    "{label}: accept-encoding"
                );
            } else if header.name.eq_ignore_ascii_case("content-type")
                && inspection.body.kind == BodyKind::Multipart
            {
                assert!(
                    wire_value.starts_with("multipart/form-data; boundary="),
                    "{label}: {wire_value}"
                );
            } else {
                assert_eq!(wire_value, &header.value, "{label}: {}", header.name);
            }
        }

        for (name, _) in headers {
            let known = inspection
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case(name))
                || wire_extras
                    .iter()
                    .any(|extra| name.eq_ignore_ascii_case(extra));
            assert!(
                known,
                "{label}: the inspector is missing the wire header {name}"
            );
        }
    }

    #[tokio::test]
    async fn the_inspector_matches_the_request_the_server_receives() {
        use crate::http::{HttpConfig, HttpExecutor};

        let server = CaptureServer::spawn();
        let dir = tempfile::tempdir().expect("temp upload");
        let upload = dir.path().join("note.txt");
        std::fs::write(&upload, b"hello upload").expect("write upload");
        let base = server.target();
        let url = format!("{base}/items?existing=1");

        for fixture in wire_fixtures(&url, &upload.to_string_lossy()) {
            let label = fixture.label;
            let jar = Arc::new(CookieJar::session_only("wire"));
            let origin = Url::parse(&base).expect("server URL");
            for (name, value) in &fixture.jar_cookies {
                jar.add(&origin, name, value).expect("store cookie");
            }

            let inspection = Inspection::build(InspectionInput {
                spec: fixture.spec.clone(),
                vars: vars(),
                jar: jar.clone(),
                assets_root: None,
            });
            assert!(inspection.can_send, "{label}: {:?}", inspection.problems);

            let executor =
                HttpExecutor::with_cookie_jar(HttpConfig::default(), jar).expect("executor");
            let (_cancel, cancel) = tokio::sync::oneshot::channel();
            executor
                .execute(&fixture.spec, cancel)
                .await
                .expect("the fixture is sent");
            let wire = server.take();
            let (method, target, headers) = wire_parts(&wire);

            assert_eq!(inspection.method, method, "{label}: method");
            assert_url_matches(label, &inspection, &target, &fixture.secrets);
            assert_headers_match(
                label,
                &inspection,
                &headers,
                &fixture.secrets,
                &fixture.wire_extras,
            );

            if !fixture.jar_cookies.is_empty() {
                let cookie = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .map(|(_, value)| value.as_str())
                    .expect("the jar attaches a cookie header");
                for (name, value) in &fixture.jar_cookies {
                    assert!(
                        cookie.contains(&format!("{name}={value}")),
                        "{label}: {cookie} carries {name}"
                    );
                }
                let JarRole::Attached { names } = &inspection.jar else {
                    panic!("{label}: the jar is reported as attached");
                };
                for (name, _) in &fixture.jar_cookies {
                    assert!(
                        names.iter().any(|reported| reported == name),
                        "{label}: {names:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_inspector_only_adds_generated_transport_rows_to_the_preview() {
        use crate::script::{ScriptFormat, render_script};

        let dir = tempfile::tempdir().expect("temp upload");
        let upload = dir.path().join("note.txt");
        std::fs::write(&upload, b"hello upload").expect("write upload");
        let url = "https://example.test/items?existing=1";

        for fixture in wire_fixtures(url, &upload.to_string_lossy()) {
            let label = fixture.label;
            let (resolved, missing) =
                crate::variables::resolve_request_lossy(&fixture.spec, &vars());
            assert!(missing.is_empty(), "{label}: {missing:?}");
            let (canonical, notice) = crate::http::compose_canonical_lenient(&resolved);
            assert!(notice.is_none(), "{label}: {notice:?}");
            let script = render_script(&canonical, ScriptFormat::Http);
            let inspection = Inspection::build(input(fixture.spec.clone()));

            let mut lines = script.lines();
            let request_line = lines.next().expect("a request line");
            let (method, preview_url) = request_line.split_once(' ').expect("method and URL");
            assert_eq!(inspection.method, method, "{label}: method");

            let shown = Url::parse(&inspection.display_url).expect("display URL");
            let preview = Url::parse(preview_url).expect("preview URL");
            assert_eq!(shown.path(), preview.path(), "{label}: path");
            let shown_pairs = query_pairs(&shown);
            let preview_pairs = query_pairs(&preview);
            assert_eq!(
                shown_pairs.len(),
                preview_pairs.len(),
                "{label}: query pairs"
            );
            for ((shown_name, shown_value), (preview_name, preview_value)) in
                shown_pairs.iter().zip(&preview_pairs)
            {
                assert_eq!(shown_name, preview_name, "{label}: query name");
                assert!(
                    shown_value == preview_value || shown_value == MASKED_VALUE,
                    "{label}: {shown_name} is {shown_value} on screen"
                );
            }

            let preview_headers: Vec<(String, String)> = lines
                .take_while(|line| !line.is_empty())
                .map(|line| {
                    let (name, value) = line.split_once(": ").expect("a header line");
                    (name.to_owned(), value.to_owned())
                })
                .collect();
            assert!(
                inspection.headers.len() >= preview_headers.len(),
                "{label}: the inspector shows every preview row"
            );
            for (header, (name, value)) in inspection.headers.iter().zip(&preview_headers) {
                assert_eq!(&header.name, name, "{label}: header order");
                if header.value != MASKED_VALUE {
                    assert_eq!(&header.value, value, "{label}: {name} value");
                }
            }

            let extras = &inspection.headers[preview_headers.len()..];
            let expected: Vec<(String, HeaderOrigin)> =
                crate::http::generated_headers(&resolved, None)
                    .into_iter()
                    .filter(|generated| {
                        !preview_headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case(&generated.name))
                    })
                    .map(|generated| (generated.name, HeaderOrigin::Generated))
                    .collect();
            let actual: Vec<(String, HeaderOrigin)> = extras
                .iter()
                .map(|header| (header.name.clone(), header.origin))
                .collect();
            assert_eq!(
                actual, expected,
                "{label}: the extras are the generated rows"
            );
        }
    }
}
