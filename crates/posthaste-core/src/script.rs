//! Pure script rendering for the request preview.
//!
//! A [`CanonicalRequest`] is the same shape Send consumes, so a rendered
//! script cannot invent a URL, header, credential, or body the app would not
//! transmit. Rendering is a string build: no shell runs, nothing is validated
//! again, and the cookie jar is not consulted.
//!
//! Command-line formats target a POSIX shell. Every value is single-quoted
//! with an embedded quote escaped as `'\''`; newlines are legal inside single
//! quotes. `.http` needs no escaping because each header and the body occupy
//! their own lines.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::http::{CanonicalBody, CanonicalPart, CanonicalRequest};

/// The script formats the preview can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptFormat {
    Curl,
    Wget,
    Http,
}

impl ScriptFormat {
    /// Every format in the order the preview's labels present them.
    pub const ALL: [Self; 3] = [Self::Curl, Self::Wget, Self::Http];

    /// The short label on the format control.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Curl => "cURL",
            Self::Wget => "wget",
            Self::Http => ".http",
        }
    }
}

/// Renders `canonical` as a runnable script in `format`.
pub fn render_script(canonical: &CanonicalRequest, format: ScriptFormat) -> String {
    match format {
        ScriptFormat::Curl => render_curl(canonical),
        ScriptFormat::Wget => render_wget(canonical),
        ScriptFormat::Http => render_http(canonical),
    }
}

fn render_curl(canonical: &CanonicalRequest) -> String {
    let basic = basic_credentials(&canonical.headers);
    let mut parts = vec![
        "curl".to_owned(),
        "-X".to_owned(),
        canonical.method.clone(),
        quote(&canonical.url),
    ];

    if let Some(credentials) = &basic {
        // cURL's own option keeps the credential out of a copied header while
        // sending exactly the same Authorization value.
        parts.push("-u".to_owned());
        parts.push(quote(&format!(
            "{}:{}",
            credentials.username, credentials.password
        )));
    }

    for (index, (name, value)) in canonical.headers.iter().enumerate() {
        if basic
            .as_ref()
            .is_some_and(|credentials| credentials.header == index)
        {
            continue;
        }
        parts.push("-H".to_owned());
        parts.push(quote(&format!("{name}: {value}")));
    }

    match &canonical.body {
        CanonicalBody::None => {}
        CanonicalBody::Raw(body) => {
            parts.push("--data-raw".to_owned());
            parts.push(quote(body));
        }
        CanonicalBody::Form(pairs) => {
            for (key, value) in pairs {
                parts.push("--data-urlencode".to_owned());
                parts.push(quote(&format!("{key}={value}")));
            }
        }
        CanonicalBody::File { path } => {
            parts.push("--data-binary".to_owned());
            parts.push(quote(&format!("@{path}")));
        }
        CanonicalBody::Multipart(parts_) => {
            for part in parts_ {
                let field = match part {
                    CanonicalPart::Text { key, value } => format!("{key}={value}"),
                    CanonicalPart::File { key, path } => format!("{key}=@{path}"),
                };
                parts.push("-F".to_owned());
                parts.push(quote(&field));
            }
        }
    }

    parts.join(" ")
}

fn render_wget(canonical: &CanonicalRequest) -> String {
    let basic = basic_credentials(&canonical.headers);
    let mut parts = vec![
        "wget".to_owned(),
        format!("--method={}", canonical.method),
        quote(&canonical.url),
    ];

    if let Some(credentials) = &basic {
        parts.push(format!("--user={}", quote(&credentials.username)));
        parts.push(format!("--password={}", quote(&credentials.password)));
    }

    for (index, (name, value)) in canonical.headers.iter().enumerate() {
        if basic
            .as_ref()
            .is_some_and(|credentials| credentials.header == index)
        {
            continue;
        }
        parts.push(format!("--header={}", quote(&format!("{name}: {value}"))));
    }

    // wget takes one body, so a form is encoded the way Send encodes it.
    match &canonical.body {
        CanonicalBody::None => {}
        CanonicalBody::Raw(body) => parts.push(format!("--body-data={}", quote(body))),
        CanonicalBody::Form(pairs) => {
            parts.push(format!("--body-data={}", quote(&encode_form(pairs))));
        }
        CanonicalBody::File { path } => parts.push(format!("--body-file={}", quote(path))),
        // wget cannot build a multipart form; the headers still describe the
        // request, and the body is left to the tool that can.
        CanonicalBody::Multipart(_) => {}
    }

    parts.join(" ")
}

fn render_http(canonical: &CanonicalRequest) -> String {
    let mut script = String::new();
    script.push_str(&canonical.method);
    script.push(' ');
    script.push_str(&canonical.url);
    script.push('\n');

    for (name, value) in &canonical.headers {
        script.push_str(name);
        script.push_str(": ");
        script.push_str(value);
        script.push('\n');
    }

    // The blank line separates the header block from the body, even when the
    // body is empty.
    script.push('\n');

    match &canonical.body {
        CanonicalBody::None => {}
        CanonicalBody::Raw(body) => script.push_str(body),
        CanonicalBody::Form(pairs) => script.push_str(&encode_form(pairs)),
        // The `.http` body-from-file form, relative to the workspace root.
        CanonicalBody::File { path } => {
            script.push_str("< ");
            script.push_str(path);
        }
        // `.http` has no multipart syntax, so no body is rendered rather than
        // a wrong one.
        CanonicalBody::Multipart(_) => {}
    }

    script
}

/// A Basic credential recovered from the canonical `Authorization` header.
struct BasicCredentials {
    username: String,
    password: String,
    /// Index of the header the credential came from, so it is not printed
    /// twice.
    header: usize,
}

/// Finds an `Authorization: Basic <base64>` header and decodes it.
///
/// The canonical request carries the credential only as a header, so the shell
/// renderers decode the user and password to use their tool's own option. An
/// undecodable value stays a header instead of disappearing.
fn basic_credentials(headers: &[(String, String)]) -> Option<BasicCredentials> {
    headers
        .iter()
        .enumerate()
        .find_map(|(header, (name, value))| {
            if !name.eq_ignore_ascii_case("authorization") {
                return None;
            }
            let encoded = value.strip_prefix("Basic ")?;
            let decoded = BASE64.decode(encoded).ok()?;
            let text = String::from_utf8(decoded).ok()?;
            let (username, password) = text.split_once(':')?;
            Some(BasicCredentials {
                username: username.to_owned(),
                password: password.to_owned(),
                header,
            })
        })
}

/// POSIX single-quote `value` so the shell keeps it as one argument.
fn quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// The form-urlencoded body Send transmits, used where a format takes one
/// body string rather than one argument per pair.
fn encode_form(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(method: &str, url: &str) -> CanonicalRequest {
        CanonicalRequest {
            method: method.to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: CanonicalBody::None,
        }
    }

    /// A cURL command for a GET with no body still names the method and URL.
    #[test]
    fn curl_describes_a_get_without_a_body() {
        let script = render_script(
            &canonical("GET", "https://api.example.com/items"),
            ScriptFormat::Curl,
        );
        assert_eq!(script, "curl -X GET 'https://api.example.com/items'");
    }

    #[test]
    fn curl_renders_json_text_and_form_bodies() {
        let mut request = canonical("POST", "https://example.com/api");
        request.headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
        request.body = CanonicalBody::Raw(r#"{"ok":true}"#.to_owned());
        let script = render_script(&request, ScriptFormat::Curl);
        assert!(script.contains("-X POST"));
        assert!(script.contains("-H 'Content-Type: application/json'"));
        assert!(script.contains(r#"--data-raw '{"ok":true}'"#));

        request.body = CanonicalBody::Raw("plain text".to_owned());
        let script = render_script(&request, ScriptFormat::Curl);
        assert!(script.contains("--data-raw 'plain text'"));

        request.body = CanonicalBody::Form(vec![
            ("name".to_owned(), "posthaste bird".to_owned()),
            ("symbol".to_owned(), "&".to_owned()),
        ]);
        let script = render_script(&request, ScriptFormat::Curl);
        assert!(script.contains("--data-urlencode 'name=posthaste bird'"));
        assert!(script.contains("--data-urlencode 'symbol=&'"));
    }

    #[test]
    fn curl_represents_every_credential_once() {
        let mut bearer = canonical("GET", "https://example.com");
        bearer.headers = vec![("Authorization".to_owned(), "Bearer abc".to_owned())];
        let script = render_script(&bearer, ScriptFormat::Curl);
        assert!(script.contains("-H 'Authorization: Bearer abc'"));
        assert!(!script.contains("-u "));

        let mut basic = canonical("GET", "https://example.com");
        basic.headers = vec![("Authorization".to_owned(), "Basic dXNlcjpwYXNz".to_owned())];
        let script = render_script(&basic, ScriptFormat::Curl);
        assert!(script.contains("-u 'user:pass'"));
        assert!(
            !script.contains("Authorization"),
            "the credential is not printed twice"
        );

        let mut api_header = canonical("GET", "https://example.com");
        api_header.headers = vec![("X-Api-Key".to_owned(), "secret".to_owned())];
        let script = render_script(&api_header, ScriptFormat::Curl);
        assert!(script.contains("-H 'X-Api-Key: secret'"));

        let api_query = canonical("GET", "https://example.com?api_key=secret");
        let script = render_script(&api_query, ScriptFormat::Curl);
        assert!(script.contains("api_key=secret"));
    }

    #[test]
    fn wget_renders_the_same_request() {
        let mut request = canonical("PUT", "https://example.com/api?a=1");
        request.headers = vec![
            ("accept".to_owned(), "application/json".to_owned()),
            ("x-note".to_owned(), "two words".to_owned()),
        ];
        request.body = CanonicalBody::Raw("hello".to_owned());
        let script = render_script(&request, ScriptFormat::Wget);
        assert!(script.starts_with("wget --method=PUT 'https://example.com/api?a=1'"));
        assert!(script.contains("--header='accept: application/json'"));
        assert!(script.contains("--header='x-note: two words'"));
        assert!(script.contains("--body-data='hello'"));
    }

    #[test]
    fn wget_omits_a_body_argument_for_a_get() {
        let script = render_script(&canonical("GET", "https://example.com"), ScriptFormat::Wget);
        assert_eq!(script, "wget --method=GET 'https://example.com'");
    }

    #[test]
    fn wget_encodes_a_form_as_one_body() {
        let mut request = canonical("POST", "https://example.com");
        request.body = CanonicalBody::Form(vec![
            ("name".to_owned(), "posthaste bird".to_owned()),
            ("symbol".to_owned(), "&".to_owned()),
        ]);
        let script = render_script(&request, ScriptFormat::Wget);
        assert!(
            script.contains("--body-data='name=posthaste+bird&symbol=%26'"),
            "the form is one encoded body: {script}"
        );
    }

    #[test]
    fn wget_renders_basic_auth_with_its_own_options() {
        let mut request = canonical("GET", "https://example.com");
        request.headers = vec![("Authorization".to_owned(), "Basic dXNlcjpwYXNz".to_owned())];
        let script = render_script(&request, ScriptFormat::Wget);
        assert!(script.contains("--user='user'"));
        assert!(script.contains("--password='pass'"));
        assert!(!script.contains("Authorization"));
    }

    #[test]
    fn wget_renders_bearer_and_api_key_headers() {
        let mut request = canonical("GET", "https://example.com");
        request.headers = vec![
            ("Authorization".to_owned(), "Bearer abc".to_owned()),
            ("X-Api-Key".to_owned(), "secret".to_owned()),
        ];
        let script = render_script(&request, ScriptFormat::Wget);
        assert!(script.contains("--header='Authorization: Bearer abc'"));
        assert!(script.contains("--header='X-Api-Key: secret'"));
    }

    #[test]
    fn http_is_a_request_file() {
        let mut request = canonical("POST", "https://api.example.com/items?a=1");
        request.headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("X-Trace".to_owned(), "abc".to_owned()),
        ];
        request.body = CanonicalBody::Raw(r#"{"ok":true}"#.to_owned());
        let script = render_script(&request, ScriptFormat::Http);
        assert_eq!(
            script,
            "POST https://api.example.com/items?a=1\nContent-Type: application/json\nX-Trace: abc\n\n{\"ok\":true}"
        );
    }

    #[test]
    fn http_renders_a_form_encoded_with_its_content_type() {
        let mut request = canonical("POST", "https://example.com");
        request.headers = vec![(
            "Content-Type".to_owned(),
            "application/x-www-form-urlencoded".to_owned(),
        )];
        request.body = CanonicalBody::Form(vec![
            ("name".to_owned(), "posthaste bird".to_owned()),
            ("symbol".to_owned(), "&".to_owned()),
        ]);
        let script = render_script(&request, ScriptFormat::Http);
        assert_eq!(
            script,
            "POST https://example.com\nContent-Type: application/x-www-form-urlencoded\n\nname=posthaste+bird&symbol=%26"
        );
    }

    #[test]
    fn http_empty_body_ends_at_the_blank_line() {
        let script = render_script(&canonical("GET", "https://example.com"), ScriptFormat::Http);
        assert_eq!(script, "GET https://example.com\n\n");
    }

    #[test]
    fn shell_values_with_spaces_quotes_newlines_and_non_ascii_stay_one_argument() {
        let url = "https://example.com/it's".to_owned();
        let header_value = "one 'two'\nthree 你好".to_owned();
        let body = "body with 'quotes'\nand 你好".to_owned();
        let request = CanonicalRequest {
            method: "POST".to_owned(),
            url: url.clone(),
            headers: vec![("X-Note".to_owned(), header_value.clone())],
            body: CanonicalBody::Raw(body.clone()),
        };

        for format in [ScriptFormat::Curl, ScriptFormat::Wget] {
            let script = render_script(&request, format);
            let arguments = shell_arguments(&script);

            assert!(
                arguments.iter().any(|argument| argument == &url),
                "{format:?} keeps the URL whole: {script}"
            );
            assert!(
                arguments
                    .iter()
                    .any(|argument| argument.ends_with(&format!("X-Note: {header_value}"))),
                "{format:?} keeps the header value whole: {script}"
            );
            assert!(
                arguments
                    .iter()
                    .any(|argument| argument == &body || argument == &format!("--body-data={body}")),
                "{format:?} keeps the body whole: {script}"
            );
        }
    }

    #[test]
    fn no_format_contains_transport_headers() {
        let request = CanonicalRequest {
            method: "POST".to_owned(),
            url: "https://example.com".to_owned(),
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            body: CanonicalBody::Raw("{}".to_owned()),
        };
        for format in ScriptFormat::ALL {
            let script = render_script(&request, format);
            for forbidden in ["Host:", "Content-Length:", "Accept-Encoding:"] {
                assert!(
                    !script.contains(forbidden),
                    "{format:?} must not carry {forbidden}: {script}"
                );
            }
        }
    }

    /// Splits a command the way a POSIX shell would for single-quoted words.
    /// Outside quotes a backslash escapes the next character; inside quotes
    /// every character is literal.
    fn shell_arguments(script: &str) -> Vec<String> {
        let mut arguments = Vec::new();
        let mut current = String::new();
        let mut chars = script.chars().peekable();
        let mut in_single = false;
        while let Some(ch) = chars.next() {
            match ch {
                '\'' => in_single = !in_single,
                '\\' if !in_single => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                _ if !in_single && ch.is_whitespace() => {
                    if !current.is_empty() {
                        arguments.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            }
        }
        if !current.is_empty() {
            arguments.push(current);
        }
        arguments
    }
}
