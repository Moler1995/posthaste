//! `{{name}}` placeholder resolution for requests.
//!
//! Resolution is intentionally a single pass over each original string:
//! substituted values are never scanned again, so a value that itself looks
//! like a placeholder stays literal.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use crate::model::{KeyValue, MultipartPart, RequestAuth, RequestBody, RequestSpec};

/// Variables available to a request: workspace defaults overwritten by the
/// active environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variables {
    values: BTreeMap<String, String>,
}

impl Variables {
    /// Builds a resolution map from workspace defaults and an optional
    /// environment whose same-named values take precedence.
    pub fn defaults_then(
        defaults: &BTreeMap<String, String>,
        environment: Option<&BTreeMap<String, String>>,
    ) -> Self {
        let mut values = defaults.clone();
        if let Some(environment) = environment {
            values.extend(
                environment
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
        }
        Self { values }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// One complete `{{name}}` placeholder found in a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderSpan {
    /// The trimmed name between the braces.
    pub name: String,
    /// Byte range covering both pairs of braces.
    pub range: Range<usize>,
}

/// The placeholder whose name the caret is currently editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlaceholder {
    /// Byte index just after the opening `{{`.
    pub name_start: usize,
    /// Byte index before the following `}}` or the caret.
    pub name_end: usize,
    /// Whether `}}` already follows the name.
    pub closed: bool,
}

/// Finds every complete, syntactically valid placeholder in `input`.
///
/// The rules match resolution: an unmatched `{{`, an empty `{{}}`, and names
/// containing braces are literal text rather than placeholders.
pub fn placeholders(input: &str) -> Vec<PlaceholderSpan> {
    let mut spans = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };

        let raw = &after[..end];
        let name = raw.trim();
        let range_start = input.len() - rest.len() + start;
        let range_end = range_start + 2 + end + 2;
        if !name.is_empty() && !name.contains(['{', '}']) {
            spans.push(PlaceholderSpan {
                name: name.to_owned(),
                range: range_start..range_end,
            });
        }
        rest = &after[end + 2..];
    }

    spans
}

/// Finds the placeholder the caret is editing, if any.
///
/// The list opens when the last `{{` before `caret` has no closing `}}`
/// between it and the caret. When a `}}` follows the name, `name_end` stops at
/// the close so accepting a suggestion rewrites the whole name; otherwise the
/// name runs through the caret.
pub fn open_placeholder(input: &str, caret: usize) -> Option<OpenPlaceholder> {
    let caret = caret.min(input.len());
    if !input.is_char_boundary(caret) {
        return None;
    }

    let before_caret = &input[..caret];
    let opener = before_caret.rfind("{{")?;
    let name_start = opener + 2;
    if before_caret[name_start..].contains("}}") {
        return None;
    }

    match input[name_start..].find("}}") {
        Some(offset) => {
            let close_start = name_start + offset;
            if close_start >= caret {
                Some(OpenPlaceholder {
                    name_start,
                    name_end: close_start,
                    closed: true,
                })
            } else {
                Some(OpenPlaceholder {
                    name_start,
                    name_end: caret,
                    closed: false,
                })
            }
        }
        None => Some(OpenPlaceholder {
            name_start,
            name_end: caret,
            closed: false,
        }),
    }
}

/// Which scope defines a variable name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VariableScope {
    /// Workspace defaults only.
    Defaults,
    /// The active environment defines the name.
    Environment,
}

/// The merged names a request can use, with where each one comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariableCatalog {
    entries: BTreeMap<String, VariableScope>,
    /// The merged values, kept beside the scopes so a field can show what a
    /// placeholder would substitute without resolving the request.
    values: BTreeMap<String, String>,
    /// Whether a loaded environment is active, even when it defines no names.
    environment_active: bool,
}

impl VariableCatalog {
    /// Builds the catalog from the same maps resolution merges. An environment
    /// name wins over a default with the same name.
    pub fn from_scopes(
        defaults: &BTreeMap<String, String>,
        environment: Option<&BTreeMap<String, String>>,
    ) -> Self {
        let mut entries: BTreeMap<String, VariableScope> = defaults
            .keys()
            .map(|name| (name.clone(), VariableScope::Defaults))
            .collect();
        let mut values = defaults.clone();
        if let Some(environment) = environment {
            entries.extend(
                environment
                    .keys()
                    .map(|name| (name.clone(), VariableScope::Environment)),
            );
            values.extend(
                environment
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
        }
        Self {
            entries,
            values,
            environment_active: environment.is_some(),
        }
    }

    /// The merged value `name` resolves to, including an empty value. `None`
    /// means no scope defines the name.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// The merged values as the map resolution takes, so a view can check a
    /// draft exactly the way Send resolves it.
    pub fn variables(&self) -> Variables {
        Variables {
            values: self.values.clone(),
        }
    }

    /// The scope a new variable should be written to: the active environment
    /// when one is loaded, otherwise the workspace defaults.
    pub fn target_scope(&self) -> VariableScope {
        if self.environment_active {
            VariableScope::Environment
        } else {
            VariableScope::Defaults
        }
    }

    /// Names whose trimmed prefix matches `prefix` without regard to case,
    /// ordered alphabetically.
    pub fn candidates(&self, prefix: &str) -> Vec<(&str, VariableScope)> {
        let prefix = prefix.trim().to_lowercase();
        let mut candidates: Vec<(&str, String, VariableScope)> = self
            .entries
            .iter()
            .filter_map(|(name, scope)| {
                let lower = name.to_lowercase();
                lower
                    .starts_with(&prefix)
                    .then_some((name.as_str(), lower, *scope))
            })
            .collect();
        candidates.sort_by(|(left_name, left_lower, _), (right_name, right_lower, _)| {
            left_lower
                .cmp(right_lower)
                .then_with(|| left_name.cmp(right_name))
        });
        candidates
            .into_iter()
            .map(|(name, _, scope)| (name, scope))
            .collect()
    }

    /// Whether the merged set defines `name`, including an empty value.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// The scope that defines `name`, if any.
    pub fn scope(&self, name: &str) -> Option<VariableScope> {
        self.entries.get(name).copied()
    }
}

/// Resolves every placeholder in `spec`.
///
/// Returns `Err` with the sorted, unique names that have no value. The method,
/// timeout, name, and id are never templated.
pub fn resolve_request(spec: &RequestSpec, vars: &Variables) -> Result<RequestSpec, Vec<String>> {
    let (resolved, missing) = resolve_request_lossy(spec, vars);
    if missing.is_empty() {
        Ok(resolved)
    } else {
        Err(missing)
    }
}

/// Resolves every placeholder it can, keeping the names that have no value.
///
/// [`resolve_request`] delegates here and turns a non-empty name list into
/// `Err`. The script preview uses the lossy form so a draft with a missing
/// variable can still be shown with the placeholder as written.
pub fn resolve_request_lossy(spec: &RequestSpec, vars: &Variables) -> (RequestSpec, Vec<String>) {
    let mut missing = BTreeSet::new();
    let mut resolved = spec.clone();

    resolved.url = substitute(&spec.url, vars, &mut missing);
    for (target, source) in resolved.headers.iter_mut().zip(&spec.headers) {
        resolve_row(target, source, vars, &mut missing);
    }
    for (target, source) in resolved.query.iter_mut().zip(&spec.query) {
        resolve_row(target, source, vars, &mut missing);
    }

    resolved.body = match &spec.body {
        RequestBody::None => RequestBody::None,
        RequestBody::Json(text) => RequestBody::Json(substitute(text, vars, &mut missing)),
        RequestBody::Text(text) => RequestBody::Text(substitute(text, vars, &mut missing)),
        RequestBody::Form(rows) => RequestBody::Form(
            rows.iter()
                .map(|row| KeyValue {
                    key: substitute(&row.key, vars, &mut missing),
                    value: substitute(&row.value, vars, &mut missing),
                    enabled: row.enabled,
                })
                .collect(),
        ),
        // A recorded file path is an asset name, not a template: braces in it
        // are never placeholders, so the path is carried over verbatim.
        RequestBody::File { path } => RequestBody::File { path: path.clone() },
        RequestBody::Multipart(parts) => RequestBody::Multipart(
            parts
                .iter()
                .map(|part| match part {
                    MultipartPart::Text {
                        key,
                        value,
                        enabled,
                    } => MultipartPart::Text {
                        key: substitute(key, vars, &mut missing),
                        value: substitute(value, vars, &mut missing),
                        enabled: *enabled,
                    },
                    MultipartPart::File { key, path, enabled } => MultipartPart::File {
                        key: substitute(key, vars, &mut missing),
                        path: path.clone(),
                        enabled: *enabled,
                    },
                })
                .collect(),
        ),
    };

    resolved.auth = match &spec.auth {
        RequestAuth::None => RequestAuth::None,
        RequestAuth::Bearer { token } => RequestAuth::Bearer {
            token: substitute(token, vars, &mut missing),
        },
        RequestAuth::Basic { username, password } => RequestAuth::Basic {
            username: substitute(username, vars, &mut missing),
            password: substitute(password, vars, &mut missing),
        },
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => RequestAuth::ApiKey {
            key: substitute(key, vars, &mut missing),
            value: substitute(value, vars, &mut missing),
            location: *location,
        },
    };

    (resolved, missing.into_iter().collect())
}

fn resolve_row(
    target: &mut KeyValue,
    source: &KeyValue,
    vars: &Variables,
    missing: &mut BTreeSet<String>,
) {
    target.key = substitute(&source.key, vars, missing);
    target.value = substitute(&source.value, vars, missing);
}

/// Replaces `{{ name }}` sequences in `input`.
///
/// An unmatched `{{`, or braces with an empty or brace-containing name, are
/// left exactly as written so JSON and text bodies can contain literal braces.
fn substitute(input: &str, vars: &Variables, missing: &mut BTreeSet<String>) -> String {
    let mut output = String::with_capacity(input.len());
    let mut copied = 0;

    for span in placeholders(input) {
        output.push_str(&input[copied..span.range.start]);
        if let Some(value) = vars.get(&span.name) {
            output.push_str(value);
        } else {
            missing.insert(span.name.clone());
            output.push_str(&input[span.range.clone()]);
        }
        copied = span.range.end;
    }

    output.push_str(&input[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Variables {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        Variables::defaults_then(&map, None)
    }

    fn templated() -> RequestSpec {
        let mut spec = RequestSpec::new("{{name}}");
        spec.url = "https://{{host}}/items/{{ id }}?keep=1".to_owned();
        spec.headers = vec![KeyValue::new("X-{{header}}", "{{token}}")];
        spec.query = vec![KeyValue::new("{{query_key}}", "{{query_value}}")];
        spec.body = RequestBody::Json(r#"{"id":"{{item_id}}"}"#.to_owned());
        spec.timeout_ms = Some(1_500);
        spec
    }

    #[test]
    fn resolves_every_templated_field() {
        let resolved = resolve_request(
            &templated(),
            &vars(&[
                ("host", "api.example.com"),
                ("id", "42"),
                ("header", "Trace"),
                ("token", "secret"),
                ("query_key", "page"),
                ("query_value", "2"),
                ("item_id", "abc"),
            ]),
        )
        .expect("all variables defined");

        assert_eq!(resolved.url, "https://api.example.com/items/42?keep=1");
        assert_eq!(resolved.headers, vec![KeyValue::new("X-Trace", "secret")]);
        assert_eq!(resolved.query, vec![KeyValue::new("page", "2")]);
        assert_eq!(
            resolved.body,
            RequestBody::Json(r#"{"id":"abc"}"#.to_owned())
        );
    }

    #[test]
    fn leaves_method_timeout_and_name_untouched() {
        let spec = templated();
        let resolved = resolve_request(&spec, &vars(&[])).expect_err("missing variables");
        assert!(!resolved.is_empty());

        let resolved = resolve_request(
            &spec,
            &vars(&[
                ("host", "h"),
                ("id", "1"),
                ("header", "H"),
                ("token", "t"),
                ("query_key", "q"),
                ("query_value", "v"),
                ("item_id", "i"),
            ]),
        )
        .expect("all variables defined");
        assert_eq!(resolved.method, spec.method);
        assert_eq!(resolved.timeout_ms, spec.timeout_ms);
        assert_eq!(resolved.name, "{{name}}");
        assert_eq!(resolved.id, spec.id);
    }

    #[test]
    fn resolves_text_and_form_bodies() {
        let mut spec = RequestSpec::new("form");
        spec.body = RequestBody::Form(vec![
            KeyValue::new("{{field}}", "{{value}}"),
            KeyValue {
                key: "ignored".to_owned(),
                value: "{{value}}".to_owned(),
                enabled: false,
            },
        ]);
        let resolved = resolve_request(&spec, &vars(&[("field", "name"), ("value", "posthaste")]))
            .expect("form resolves");
        assert_eq!(
            resolved.body,
            RequestBody::Form(vec![
                KeyValue::new("name", "posthaste"),
                KeyValue {
                    key: "ignored".to_owned(),
                    value: "posthaste".to_owned(),
                    enabled: false,
                },
            ])
        );

        spec.body = RequestBody::Text("hello {{value}}".to_owned());
        let resolved = resolve_request(&spec, &vars(&[("value", "world")])).expect("text resolves");
        assert_eq!(resolved.body, RequestBody::Text("hello world".to_owned()));
    }

    #[test]
    fn resolves_multipart_keys_and_text_values_but_not_file_paths() {
        let mut spec = RequestSpec::new("upload");
        spec.body = RequestBody::Multipart(vec![
            MultipartPart::text("{{field}}", "hello {{value}}"),
            MultipartPart::file("{{file_key}}", "files/{{literal}} license.txt"),
        ]);

        let resolved = resolve_request(
            &spec,
            &vars(&[
                ("field", "title"),
                ("value", "Posthaste"),
                ("file_key", "upload"),
            ]),
        )
        .expect("the text placeholders resolve");

        assert_eq!(
            resolved.body,
            RequestBody::Multipart(vec![
                MultipartPart::text("title", "hello Posthaste"),
                // The path is an asset name, braces and all; it is not looked up.
                MultipartPart::file("upload", "files/{{literal}} license.txt"),
            ])
        );
        let RequestBody::Multipart(parts) = &resolved.body else {
            panic!("the body stays multipart");
        };
        assert!(
            parts.iter().all(MultipartPart::enabled),
            "resolution keeps each row's enabled flag"
        );
    }

    #[test]
    fn reports_a_missing_multipart_variable() {
        let mut spec = RequestSpec::new("upload");
        spec.body = RequestBody::Multipart(vec![MultipartPart::text("title", "{{caption}}")]);

        assert_eq!(
            resolve_request(&spec, &vars(&[])).expect_err("caption has no value"),
            vec!["caption"]
        );

        let (resolved, missing) = resolve_request_lossy(&spec, &vars(&[]));
        assert_eq!(missing, vec!["caption"]);
        assert_eq!(
            resolved.body,
            RequestBody::Multipart(vec![MultipartPart::text("title", "{{caption}}")]),
            "the unresolved placeholder stays as written"
        );
    }

    #[test]
    fn environment_values_override_defaults() {
        let defaults: BTreeMap<String, String> =
            [("host".to_owned(), "dev.example.com".to_owned())].into();
        let environment: BTreeMap<String, String> = [
            ("host".to_owned(), "prod.example.com".to_owned()),
            ("token".to_owned(), "t".to_owned()),
        ]
        .into();
        let vars = Variables::defaults_then(&defaults, Some(&environment));
        assert_eq!(vars.get("host"), Some("prod.example.com"));
        assert_eq!(vars.get("token"), Some("t"));
    }

    #[test]
    fn values_are_not_expanded_again() {
        let mut spec = RequestSpec::new("nested");
        spec.url = "https://{{outer}}".to_owned();
        let resolved = resolve_request(
            &spec,
            &vars(&[("outer", "{{inner}}"), ("inner", "expanded")]),
        )
        .expect("outer is defined");
        assert_eq!(resolved.url, "https://{{inner}}");
    }

    #[test]
    fn missing_names_are_sorted_and_unique() {
        let mut spec = RequestSpec::new("missing");
        spec.url = "{{zeta}}/{{alpha}}/{{zeta}}".to_owned();
        spec.headers = vec![KeyValue::new("X", "{{beta}}")];
        let missing = resolve_request(&spec, &vars(&[])).expect_err("nothing defined");
        assert_eq!(missing, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn lossy_resolution_keeps_missing_placeholders_and_reports_them() {
        let spec = templated();
        let vars = vars(&[("host", "api.example.com"), ("id", "42")]);

        let (resolved, missing) = resolve_request_lossy(&spec, &vars);
        assert_eq!(resolved.url, "https://api.example.com/items/42?keep=1");
        assert_eq!(
            resolved.headers,
            vec![KeyValue::new("X-{{header}}", "{{token}}")],
            "an unresolved value stays as written"
        );
        assert_eq!(
            resolved.query,
            vec![KeyValue::new("{{query_key}}", "{{query_value}}")]
        );
        assert_eq!(
            missing,
            vec!["header", "item_id", "query_key", "query_value", "token"],
            "only the names without a value are reported"
        );

        assert!(
            resolve_request(&spec, &vars).is_err(),
            "the strict path still rejects the same input"
        );
    }

    #[test]
    fn resolves_every_auth_field() {
        use crate::model::ApiKeyLocation;

        let mut spec = RequestSpec::new("auth");
        spec.auth = RequestAuth::Bearer {
            token: "{{token}}".to_owned(),
        };
        let resolved = resolve_request(&spec, &vars(&[("token", "abc")])).expect("token defined");
        assert_eq!(
            resolved.auth,
            RequestAuth::Bearer {
                token: "abc".to_owned()
            }
        );

        spec.auth = RequestAuth::Basic {
            username: "{{user}}".to_owned(),
            password: "{{pass}}".to_owned(),
        };
        let resolved =
            resolve_request(&spec, &vars(&[("user", "u"), ("pass", "p")])).expect("both defined");
        assert_eq!(
            resolved.auth,
            RequestAuth::Basic {
                username: "u".to_owned(),
                password: "p".to_owned()
            }
        );

        spec.auth = RequestAuth::ApiKey {
            key: "{{key}}".to_owned(),
            value: "{{value}}".to_owned(),
            location: ApiKeyLocation::Query,
        };
        let resolved =
            resolve_request(&spec, &vars(&[("key", "api_key"), ("value", "v")])).expect("defined");
        assert_eq!(
            resolved.auth,
            RequestAuth::ApiKey {
                key: "api_key".to_owned(),
                value: "v".to_owned(),
                location: ApiKeyLocation::Query
            }
        );

        let missing = resolve_request(&spec, &vars(&[("key", "api_key")])).expect_err("no value");
        assert_eq!(missing, vec!["value"]);
    }

    #[test]
    fn literal_braces_are_preserved() {
        let mut spec = RequestSpec::new("literal");
        spec.body = RequestBody::Json(
            r#"{"open":"{{","empty":"{{}}","brace":"{{a{b}}","closed":"}}"}"#.to_owned(),
        );
        let resolved = resolve_request(&spec, &vars(&[])).expect("no placeholders");
        assert_eq!(
            resolved.body,
            RequestBody::Json(
                r#"{"open":"{{","empty":"{{}}","brace":"{{a{b}}","closed":"}}"}"#.to_owned()
            )
        );
    }

    #[test]
    fn empty_values_are_defined() {
        let mut spec = RequestSpec::new("empty");
        spec.url = "https://example.com/{{suffix}}".to_owned();
        let resolved = resolve_request(&spec, &vars(&[("suffix", "")])).expect("empty is defined");
        assert_eq!(resolved.url, "https://example.com/");
    }

    #[test]
    fn placeholders_report_trimmed_names_and_whole_ranges() {
        let input = "https://{{host}}/items/{{ id }}";
        assert_eq!(
            placeholders(input),
            vec![
                PlaceholderSpan {
                    name: "host".to_owned(),
                    range: 8..16,
                },
                PlaceholderSpan {
                    name: "id".to_owned(),
                    range: 23..31,
                },
            ]
        );
    }

    #[test]
    fn placeholders_skip_literal_braces() {
        for input in ["{{", "{{}}", "{{a{b}}", "x {{ y", "}}"] {
            assert!(
                placeholders(input).is_empty(),
                "{input:?} has no placeholder"
            );
        }
        assert_eq!(
            placeholders("{{}} {{a}}"),
            vec![PlaceholderSpan {
                name: "a".to_owned(),
                range: 5..10,
            }]
        );
    }

    #[test]
    fn open_placeholder_tracks_the_edited_name() {
        assert_eq!(
            open_placeholder("https://{{ho", 12),
            Some(OpenPlaceholder {
                name_start: 10,
                name_end: 12,
                closed: false,
            })
        );
        assert_eq!(
            open_placeholder("{{hots}}", 5),
            Some(OpenPlaceholder {
                name_start: 2,
                name_end: 6,
                closed: true,
            }),
            "a caret inside a complete placeholder rewrites its name"
        );
        assert_eq!(
            open_placeholder("{{id}} and {{", 13),
            Some(OpenPlaceholder {
                name_start: 13,
                name_end: 13,
                closed: false,
            }),
            "the last opener wins"
        );
        assert_eq!(
            open_placeholder("{{id}}", 6),
            None,
            "a caret outside the placeholder does not trigger"
        );
        assert_eq!(
            open_placeholder("{{id}}{{", 8),
            Some(OpenPlaceholder {
                name_start: 8,
                name_end: 8,
                closed: false,
            })
        );
        assert_eq!(
            open_placeholder("no opener", 4),
            None,
            "text without {{ does not trigger"
        );
    }

    #[test]
    fn open_placeholder_accepts_the_caret_at_the_close() {
        assert_eq!(
            open_placeholder("{{hot}}", 5),
            Some(OpenPlaceholder {
                name_start: 2,
                name_end: 5,
                closed: true,
            }),
            "the caret just before }} is inside the placeholder"
        );
    }

    #[test]
    fn catalog_merges_scopes_and_marks_environment_overrides() {
        let defaults: BTreeMap<String, String> = [
            ("token".to_owned(), "default".to_owned()),
            ("host".to_owned(), "default.example.com".to_owned()),
            ("empty".to_owned(), String::new()),
        ]
        .into();
        let environment: BTreeMap<String, String> = [("token".to_owned(), "env".to_owned())].into();
        let catalog = VariableCatalog::from_scopes(&defaults, Some(&environment));

        assert_eq!(catalog.scope("host"), Some(VariableScope::Defaults));
        assert_eq!(catalog.scope("token"), Some(VariableScope::Environment));
        assert_eq!(catalog.scope("empty"), Some(VariableScope::Defaults));
        assert!(catalog.contains("empty"), "an empty value is defined");
        assert!(!catalog.contains("missing"));
    }

    #[test]
    fn catalog_candidates_filter_case_insensitively_and_sort() {
        let defaults: BTreeMap<String, String> = [
            ("alpha".to_owned(), String::new()),
            ("Beta".to_owned(), String::new()),
            ("beta_two".to_owned(), String::new()),
            ("gamma".to_owned(), String::new()),
        ]
        .into();
        let environment: BTreeMap<String, String> = [("BETA".to_owned(), String::new())].into();
        let catalog = VariableCatalog::from_scopes(&defaults, Some(&environment));

        assert_eq!(
            catalog.candidates("be"),
            vec![
                ("BETA", VariableScope::Environment),
                ("Beta", VariableScope::Defaults),
                ("beta_two", VariableScope::Defaults),
            ],
            "matches ignore case and ties use byte order"
        );
        assert_eq!(
            catalog.candidates("  B  "),
            vec![
                ("BETA", VariableScope::Environment),
                ("Beta", VariableScope::Defaults),
                ("beta_two", VariableScope::Defaults),
            ],
            "the prefix is trimmed and case-insensitive"
        );
        assert_eq!(catalog.candidates("").len(), 5);
        assert!(catalog.candidates("zz").is_empty());
    }
}
