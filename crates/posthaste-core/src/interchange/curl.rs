//! cURL command-line import.
//!
//! One pasted document can hold several commands, one per line or continued
//! with a trailing `\` (or `^` in cmd.exe). The tokenizer follows the shell
//! quoting rules Chrome and Firefox use when copying a request, and the
//! interpreter maps only the parts of cURL that a [`RequestSpec`] can hold.
//! Everything else is reported rather than guessed at.

use crate::model::{HttpMethod, KeyValue, RequestAuth, RequestBody, RequestSpec};

use super::{ImportBundle, ImportCollection, ImportWarning, InterchangeError};

/// The largest cURL document the panel accepts. Parsing runs on the UI thread,
/// so a pasted multi-megabyte document is refused instead of shaped.
pub const MAX_CURL_TEXT_BYTES: usize = 1024 * 1024;

/// One shell word and where it started in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub text: String,
    /// Byte offset of the word's first character in the original document.
    pub index: usize,
    /// Whether the word contained a `$` or backtick that the shell would
    /// expand. Single-quoted and escaped characters stay literal.
    pub shell_expands: bool,
}

/// One command: its words and the line the command starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurlCommand {
    pub words: Vec<Word>,
    pub line: usize,
}

/// The requests read from pasted command text, before they are placed in a
/// collection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurlImport {
    pub requests: Vec<RequestSpec>,
    pub warnings: Vec<ImportWarning>,
}

/// Converts pasted cURL text into one collection of requests.
///
/// `collection_name` is the name the user typed in the panel. A command that
/// cannot be imported contributes a warning and is skipped; the remaining
/// commands still become requests.
pub fn parse_curl_text(
    text: &str,
    collection_name: &str,
) -> Result<ImportBundle, InterchangeError> {
    let CurlImport { requests, warnings } = parse_curl_requests(text)?;
    let collections = if requests.is_empty() {
        Vec::new()
    } else {
        vec![ImportCollection {
            name: collection_name.trim().to_owned(),
            order: 0,
            folders: Vec::new(),
            requests,
        }]
    };
    Ok(ImportBundle {
        collections,
        environments: Vec::new(),
        warnings,
    })
}

/// Reads pasted command text into requests without choosing a destination.
///
/// The panel uses this when the requests go into an existing collection; the
/// bundle entry point above wraps it with a new collection.
pub fn parse_curl_requests(text: &str) -> Result<CurlImport, InterchangeError> {
    if text.len() > MAX_CURL_TEXT_BYTES {
        return Err(InterchangeError::CurlTextTooLarge {
            size: text.len(),
            limit: MAX_CURL_TEXT_BYTES,
        });
    }

    let mut import = CurlImport::default();
    for command in tokenize(text) {
        let interpretation = interpret(&command);
        for reason in interpretation.warnings {
            import.warnings.push(ImportWarning {
                item: format!("line {}", command.line),
                reason,
            });
        }
        let Some(parsed) = interpretation.parsed else {
            continue;
        };
        match to_request(&parsed) {
            Some(request) => import.requests.push(request),
            None => import.warnings.push(ImportWarning {
                item: format!("line {}", command.line),
                reason: "the command has no URL, so it was skipped".to_owned(),
            }),
        }
    }
    Ok(import)
}

/// Splits the document into commands. A command ends at a newline that is not
/// escaped or preceded by a continuation character.
fn tokenize(text: &str) -> Vec<CurlCommand> {
    let chars: Vec<char> = text.chars().collect();
    let mut offsets = Vec::with_capacity(chars.len() + 1);
    let mut offset = 0;
    for ch in &chars {
        offsets.push(offset);
        offset += ch.len_utf8();
    }
    offsets.push(offset);

    let mut scanner = Scanner {
        chars: &chars,
        offsets: &offsets,
        position: 0,
        line: 1,
    };
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = WordBuilder::default();
    let mut command_start = None;

    while let Some(ch) = scanner.peek() {
        match ch {
            ' ' | '\t' | '\r' => {
                word.finish(&mut words, &mut command_start);
                scanner.bump();
            }
            '\n' => {
                word.finish(&mut words, &mut command_start);
                flush_command(&mut words, &mut commands, &mut command_start);
                scanner.bump();
            }
            '#' if word.index.is_none() => {
                let start = scanner.index();
                let start_line = scanner.line;
                let mut comment = String::from('#');
                while let Some(inner) = scanner.peek() {
                    if inner == '\n' {
                        break;
                    }
                    comment.push(inner);
                    scanner.bump();
                }
                // A comment-only line is reported as a skipped line; a
                // trailing comment after a command is ignored like the shell.
                if words.is_empty() {
                    words.push(Word {
                        text: comment,
                        index: start,
                        shell_expands: false,
                    });
                    command_start = Some(start_line);
                }
            }
            '\\' => {
                scanner.bump();
                if scanner.consume_continuation() {
                    continue;
                }
                if let Some((escaped, index, line)) = scanner.bump_positioned() {
                    word.push(escaped, index, line);
                }
            }
            '^' => {
                let index = scanner.index();
                let line = scanner.line;
                scanner.bump();
                if !scanner.consume_continuation() {
                    word.push('^', index, line);
                }
            }
            '\'' => {
                scanner.bump();
                while let Some((inner, index, line)) = scanner.bump_positioned() {
                    if inner == '\'' {
                        break;
                    }
                    word.push(inner, index, line);
                }
            }
            '"' => {
                scanner.bump();
                while let Some((inner, index, line)) = scanner.bump_positioned() {
                    match inner {
                        '"' => break,
                        '\\' => match scanner.bump_positioned() {
                            Some((escaped, _, _)) if escaped == '"' || escaped == '\\' => {
                                word.push(escaped, index, line);
                            }
                            // A backslash-newline inside double quotes is a
                            // line continuation with no character of its own.
                            Some(('\n', _, _)) => {}
                            Some(('\r', _, _)) if scanner.peek() == Some('\n') => {
                                scanner.bump();
                            }
                            // Other escapes keep the backslash, as in a shell.
                            Some((escaped, _, _)) => {
                                word.push('\\', index, line);
                                word.push(escaped, index, line);
                            }
                            None => {}
                        },
                        _ => {
                            if inner == '$' || inner == '`' {
                                word.mark_shell_expansion();
                            }
                            word.push(inner, index, line);
                        }
                    }
                }
            }
            '$' if scanner.peek_at(1) == Some('\'') => {
                scanner.bump();
                scanner.bump();
                tokenize_ansi_c(&mut scanner, &mut word);
            }
            _ => {
                let index = scanner.index();
                let line = scanner.line;
                scanner.bump();
                if ch == '$' || ch == '`' {
                    word.mark_shell_expansion();
                }
                word.push(ch, index, line);
            }
        }
    }
    word.finish(&mut words, &mut command_start);
    flush_command(&mut words, &mut commands, &mut command_start);
    commands
}

/// The shared `$'…'` body. The opening `$'` is already consumed.
fn tokenize_ansi_c(scanner: &mut Scanner<'_>, word: &mut WordBuilder) {
    while let Some((inner, index, line)) = scanner.bump_positioned() {
        if inner == '\'' {
            return;
        }
        if inner != '\\' {
            word.push(inner, index, line);
            continue;
        }
        let Some((escaped, index, line)) = scanner.bump_positioned() else {
            return;
        };
        match escaped {
            'n' => word.push('\n', index, line),
            'r' => word.push('\r', index, line),
            't' => word.push('\t', index, line),
            '\\' => word.push('\\', index, line),
            '\'' => word.push('\'', index, line),
            'x' => {
                let mut digits = String::new();
                while digits.len() < 2 {
                    match scanner.peek() {
                        Some(digit) if digit.is_ascii_hexdigit() => {
                            digits.push(digit);
                            scanner.bump();
                        }
                        _ => break,
                    }
                }
                if digits.len() == 2
                    && let Ok(byte) = u8::from_str_radix(&digits, 16)
                {
                    word.push(char::from(byte), index, line);
                    continue;
                }
                word.push('\\', index, line);
                word.push('x', index, line);
                for digit in digits.chars() {
                    word.push(digit, index, line);
                }
            }
            _ => {
                word.push('\\', index, line);
                word.push(escaped, index, line);
            }
        }
    }
}

fn flush_command(
    words: &mut Vec<Word>,
    commands: &mut Vec<CurlCommand>,
    command_start: &mut Option<usize>,
) {
    if words.is_empty() {
        return;
    }
    commands.push(CurlCommand {
        words: std::mem::take(words),
        line: command_start.take().unwrap_or(1),
    });
}

/// Turns a cURL command into the parts of a request it can express.
fn interpret(command: &CurlCommand) -> Interpretation {
    let mut warnings = Vec::new();
    let Some(first) = command.words.first() else {
        return Interpretation {
            parsed: None,
            warnings,
        };
    };
    if !is_curl_invocation(&first.text) {
        warnings.push(format!(
            "`{}` is not a cURL command, so it was skipped",
            first.text
        ));
        return Interpretation {
            parsed: None,
            warnings,
        };
    }
    for word in &command.words {
        if is_shell_operator(&word.text) {
            warnings.push(
                "shell pipelines, redirection, and command chaining are not supported, so the command was skipped"
                    .to_owned(),
            );
            return Interpretation {
                parsed: None,
                warnings,
            };
        }
        if word.shell_expands
            && let Some(construct) = shell_substitution(&word.text)
        {
            warnings.push(format!(
                "the shell substitution {construct} is not supported, so the command was skipped"
            ));
            return Interpretation {
                parsed: None,
                warnings,
            };
        }
    }

    let mut parsed = ParsedCommand::default();
    let words = &command.words;
    let mut index = 1;
    while index < words.len() {
        let word = &words[index].text;
        if word.starts_with("--") {
            interpret_long(word, words, &mut index, &mut parsed, &mut warnings);
        } else if word.starts_with('-') && word.len() > 1 {
            interpret_short(word, words, &mut index, &mut parsed, &mut warnings);
        } else {
            set_url(&mut parsed, word.clone(), &mut warnings);
            index += 1;
        }
    }

    match resolve_method(
        parsed.explicit_method.as_deref(),
        parsed.body_flag,
        parsed.get,
        &mut warnings,
    ) {
        Some(method) => parsed.method = method,
        None => {
            return Interpretation {
                parsed: None,
                warnings,
            };
        }
    }
    Interpretation {
        parsed: Some(parsed),
        warnings,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedCommand {
    method: HttpMethod,
    explicit_method: Option<String>,
    url: Option<String>,
    headers: Vec<KeyValue>,
    body: Vec<BodyPart>,
    auth: Option<(String, String)>,
    timeout_ms: Option<u64>,
    get: bool,
    /// Whether a data or form flag appeared, even if its value was rejected.
    body_flag: bool,
}

impl Default for ParsedCommand {
    fn default() -> Self {
        Self {
            method: HttpMethod::Get,
            explicit_method: None,
            url: None,
            headers: Vec::new(),
            body: Vec::new(),
            auth: None,
            timeout_ms: None,
            get: false,
            body_flag: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyPart {
    Data(String),
    Form(KeyValue),
}

#[derive(Debug, Clone)]
struct Interpretation {
    parsed: Option<ParsedCommand>,
    warnings: Vec<String>,
}

fn interpret_long(
    word: &str,
    words: &[Word],
    index: &mut usize,
    parsed: &mut ParsedCommand,
    warnings: &mut Vec<String>,
) {
    let (name, inline) = match word.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (word, None),
    };
    match name {
        "--request" => match take_value(inline, words, index) {
            Some(value) => parsed.explicit_method = Some(value.trim().to_owned()),
            None => warnings.push(missing_value(name)),
        },
        "--header" => match take_value(inline, words, index) {
            Some(value) => add_header(parsed, &value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--data" | "--data-binary" => match take_value(inline, words, index) {
            Some(value) => add_data(parsed, value, true, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--data-raw" => match take_value(inline, words, index) {
            Some(value) => add_data(parsed, value, false, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--data-urlencode" => match take_value(inline, words, index) {
            Some(value) => add_urlencoded(parsed, &value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--form" => match take_value(inline, words, index) {
            Some(value) => add_form_part(parsed, &value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--user" => match take_value(inline, words, index) {
            Some(value) => set_user(parsed, &value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--cookie" => match take_value(inline, words, index) {
            Some(value) => add_cookie(parsed, &value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--user-agent" => match take_value(inline, words, index) {
            Some(value) => parsed
                .headers
                .push(KeyValue::new("User-Agent", value.trim())),
            None => warnings.push(missing_value(name)),
        },
        "--referer" => match take_value(inline, words, index) {
            Some(value) => parsed.headers.push(KeyValue::new("Referer", value.trim())),
            None => warnings.push(missing_value(name)),
        },
        "--url" => match take_value(inline, words, index) {
            Some(value) => set_url(parsed, value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--max-time" => match take_value(inline, words, index) {
            Some(value) => set_timeout(parsed, &value, warnings),
            None => warnings.push(missing_value(name)),
        },
        "--get" => {
            parsed.get = true;
            *index += 1;
        }
        _ if is_ignored_long(name) => {
            *index += 1;
        }
        _ if takes_value_long(name) => {
            if take_value(inline, words, index).is_some() {
                warnings.push(unsupported(name));
            } else {
                warnings.push(missing_value(name));
            }
        }
        _ => {
            *index += 1;
            warnings.push(unsupported(name));
        }
    }
}

fn interpret_short(
    word: &str,
    words: &[Word],
    index: &mut usize,
    parsed: &mut ParsedCommand,
    warnings: &mut Vec<String>,
) {
    let start = *index;
    let flags: Vec<char> = word.chars().skip(1).collect();
    let mut position = 0;
    while position < flags.len() {
        let flag = flags[position];
        let remainder: String = flags[position + 1..].iter().collect();
        match flag {
            'G' => {
                parsed.get = true;
                position += 1;
            }
            'X' | 'H' | 'd' | 'u' | 'b' | 'A' | 'e' | 'm' | 'F' => {
                let inline = (!remainder.is_empty()).then_some(remainder.as_str());
                match take_value(inline, words, index) {
                    Some(value) => apply_short_option(flag, value, parsed, warnings),
                    None => warnings.push(missing_value(&format!("-{flag}"))),
                }
                break;
            }
            'o' | 'x' | 'c' | 'w' | 'D' | 'U' | 'E' | 'T' | 'K' => {
                let inline = (!remainder.is_empty()).then_some(remainder.as_str());
                if take_value(inline, words, index).is_some() {
                    warnings.push(unsupported(&format!("-{flag}")));
                } else {
                    warnings.push(missing_value(&format!("-{flag}")));
                }
                break;
            }
            's' | 'S' | 'k' | 'L' | 'i' | 'v' | 'f' | 'g' | '#' => position += 1,
            _ => {
                warnings.push(unsupported(&format!("-{flag}")));
                position += 1;
            }
        }
    }
    if *index == start {
        *index += 1;
    }
}

fn apply_short_option(
    flag: char,
    value: String,
    parsed: &mut ParsedCommand,
    warnings: &mut Vec<String>,
) {
    match flag {
        'X' => parsed.explicit_method = Some(value.trim().to_owned()),
        'H' => add_header(parsed, &value, warnings),
        'd' => add_data(parsed, value, true, warnings),
        'u' => set_user(parsed, &value, warnings),
        'b' => add_cookie(parsed, &value, warnings),
        'A' => parsed
            .headers
            .push(KeyValue::new("User-Agent", value.trim())),
        'e' => parsed.headers.push(KeyValue::new("Referer", value.trim())),
        'm' => set_timeout(parsed, &value, warnings),
        'F' => add_form_part(parsed, &value, warnings),
        _ => unreachable!("only value-taking short flags reach here"),
    }
}

/// Consumes the option's value, either from the `--flag=value` spelling or the
/// next word. `index` always advances, so the caller's loop keeps moving even
/// when the value is missing.
fn take_value(inline: Option<&str>, words: &[Word], index: &mut usize) -> Option<String> {
    if let Some(value) = inline {
        *index += 1;
        return Some(value.to_owned());
    }
    match words.get(*index + 1) {
        Some(word) => {
            *index += 2;
            Some(word.text.clone())
        }
        None => {
            *index += 1;
            None
        }
    }
}

fn add_header(parsed: &mut ParsedCommand, header: &str, warnings: &mut Vec<String>) {
    match header.split_once(':') {
        Some((key, value)) if !key.trim().is_empty() => {
            let (key, value) = (key.trim(), value.trim());
            parsed.headers.push(KeyValue::new(key, value));
        }
        _ => warnings.push(format!(
            "the header `{header}` is not in `Name: value` form and was ignored"
        )),
    }
}

fn add_data(
    parsed: &mut ParsedCommand,
    value: String,
    interpret_at: bool,
    warnings: &mut Vec<String>,
) {
    parsed.body_flag = true;
    if interpret_at && value.starts_with('@') {
        warnings.push(format!(
            "the file body `{value}` is not supported and was ignored"
        ));
    } else {
        parsed.body.push(BodyPart::Data(value));
    }
}

fn add_urlencoded(parsed: &mut ParsedCommand, value: &str, warnings: &mut Vec<String>) {
    parsed.body_flag = true;
    if value.starts_with('@') || (!value.contains('=') && value.contains('@')) {
        warnings.push(format!(
            "the file body `{value}` is not supported and was ignored"
        ));
        return;
    }
    let (key, value) = value.split_once('=').unwrap_or((value, ""));
    parsed.body.push(BodyPart::Form(KeyValue::new(key, value)));
}

fn add_form_part(parsed: &mut ParsedCommand, value: &str, warnings: &mut Vec<String>) {
    parsed.body_flag = true;
    let Some((name, part)) = value.split_once('=') else {
        warnings.push(format!(
            "the form part `{value}` is not in `name=value` form and was ignored"
        ));
        return;
    };
    if part.starts_with('@') || part.starts_with('<') {
        warnings.push(format!(
            "the file part `{value}` is not supported and was ignored"
        ));
        return;
    }
    if name.is_empty() {
        warnings.push(format!(
            "the form part `{value}` has no name and was ignored"
        ));
        return;
    }
    parsed.body.push(BodyPart::Form(KeyValue::new(name, part)));
}

fn set_url(parsed: &mut ParsedCommand, url: String, warnings: &mut Vec<String>) {
    if parsed.url.is_some() {
        warnings.push(format!(
            "the URL `{url}` was ignored because only the first URL is imported"
        ));
    } else {
        parsed.url = Some(url);
    }
}

fn set_user(parsed: &mut ParsedCommand, value: &str, warnings: &mut Vec<String>) {
    match value.split_once(':') {
        Some((user, password)) => {
            parsed.auth = Some((user.to_owned(), password.to_owned()));
        }
        None => {
            warnings.push(
                "`--user` has no colon, so the password is empty instead of prompted for"
                    .to_owned(),
            );
            parsed.auth = Some((value.to_owned(), String::new()));
        }
    }
}

fn add_cookie(parsed: &mut ParsedCommand, value: &str, warnings: &mut Vec<String>) {
    if value.contains('=') {
        parsed.headers.push(KeyValue::new("Cookie", value.trim()));
    } else {
        warnings.push(format!(
            "the cookie file `{value}` is not supported and was ignored"
        ));
    }
}

fn set_timeout(parsed: &mut ParsedCommand, value: &str, warnings: &mut Vec<String>) {
    match value.trim().parse::<f64>() {
        Ok(seconds) if seconds.is_finite() && seconds >= 0.0 => {
            // curl treats zero as "no timeout", not as an instant timeout. A
            // positive value that rounds down to zero milliseconds cannot be
            // represented either, so it means the same thing.
            let millis = (seconds * 1000.0).round() as u64;
            parsed.timeout_ms = (millis > 0).then_some(millis);
        }
        _ => warnings.push(format!(
            "`--max-time` value `{value}` is not a number of seconds and was ignored"
        )),
    }
}

fn resolve_method(
    explicit: Option<&str>,
    has_body: bool,
    get: bool,
    warnings: &mut Vec<String>,
) -> Option<HttpMethod> {
    if let Some(raw) = explicit {
        let raw = raw.trim();
        if let Some(method) = HttpMethod::ALL
            .iter()
            .copied()
            .find(|method| method.as_str().eq_ignore_ascii_case(raw))
        {
            return Some(method);
        }
        warnings.push(format!(
            "the method `{raw}` cannot be stored, so the command was skipped"
        ));
        return None;
    }
    if get || !has_body {
        Some(HttpMethod::Get)
    } else {
        Some(HttpMethod::Post)
    }
}

fn to_request(parsed: &ParsedCommand) -> Option<RequestSpec> {
    let url = parsed.url.clone()?;
    if url.trim().is_empty() {
        return None;
    }
    let mut request = RequestSpec::new(request_name(parsed.method, &url));
    request.method = parsed.method;
    request.url = url;
    request.headers = parsed.headers.clone();
    request.timeout_ms = parsed.timeout_ms;
    if let Some((username, password)) = &parsed.auth {
        request.auth = RequestAuth::Basic {
            username: username.clone(),
            password: password.clone(),
        };
    }

    if !parsed.body.is_empty() {
        if parsed.get {
            // `-d` with `-G` appends to the URL's own query, so the URL's rows
            // come first and the data rows follow in the order curl gave them.
            request.normalize_query();
            for part in &parsed.body {
                match part {
                    BodyPart::Data(text) => request.query.extend(split_pairs(text)),
                    BodyPart::Form(row) => request.query.push(row.clone()),
                }
            }
        } else if parsed
            .body
            .iter()
            .any(|part| matches!(part, BodyPart::Form(_)))
        {
            let mut rows = Vec::new();
            for part in &parsed.body {
                match part {
                    BodyPart::Data(text) => rows.extend(split_pairs(text)),
                    BodyPart::Form(row) => rows.push(row.clone()),
                }
            }
            request.body = RequestBody::Form(rows);
        } else {
            let mut data = Vec::new();
            for part in &parsed.body {
                if let BodyPart::Data(text) = part {
                    data.push(text.clone());
                }
            }
            let joined = data.join("&");
            if !joined.is_empty() {
                request.body = body_for(&joined, &parsed.headers);
            }
        }
    }

    request.normalize_query();
    Some(request)
}

fn body_for(text: &str, headers: &[KeyValue]) -> RequestBody {
    let media_type = headers
        .iter()
        .rev()
        .find(|header| header.key.eq_ignore_ascii_case("content-type"))
        .map(|header| {
            header
                .value
                .split(';')
                .next()
                .unwrap_or(&header.value)
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if media_type == "application/json" || media_type.ends_with("+json") {
        RequestBody::Json(text.to_owned())
    } else if media_type == "application/x-www-form-urlencoded" {
        RequestBody::Form(split_pairs(text))
    } else {
        RequestBody::Text(text.to_owned())
    }
}

fn split_pairs(text: &str) -> Vec<KeyValue> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            KeyValue::new(key, value)
        })
        .collect()
}

fn request_name(method: HttpMethod, url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => format!("{} {}", method.as_str(), parsed.path()),
        Err(_) => format!("{} {url}", method.as_str()),
    }
}

fn is_curl_invocation(word: &str) -> bool {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    name.eq_ignore_ascii_case("curl") || name.eq_ignore_ascii_case("curl.exe")
}

fn is_shell_operator(word: &str) -> bool {
    matches!(
        word,
        "|" | "||" | "&&" | "&" | ";" | ">" | ">>" | "<" | "<<" | "2>" | "2>>" | "&>" | "2>&1"
    )
}

fn shell_substitution(word: &str) -> Option<&'static str> {
    if word.contains("$(") {
        Some("`$(…)`")
    } else if word.contains('`') {
        Some("backticks")
    } else if has_variable(word) {
        Some("`$VAR`")
    } else {
        None
    }
}

fn has_variable(word: &str) -> bool {
    let bytes = word.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'$' {
            let next = bytes[index + 1];
            if next == b'{' || next == b'_' || next.is_ascii_alphabetic() {
                return true;
            }
        }
        index += 1;
    }
    false
}

fn is_ignored_long(name: &str) -> bool {
    matches!(
        name,
        "--insecure"
            | "--compressed"
            | "--location"
            | "--fail"
            | "--globoff"
            | "--http1.1"
            | "--http2"
            | "--silent"
            | "--show-error"
            | "--verbose"
            | "--include"
    )
}

fn takes_value_long(name: &str) -> bool {
    matches!(
        name,
        "--proxy"
            | "--proxy-user"
            | "--cacert"
            | "--capath"
            | "--cert"
            | "--key"
            | "--pass"
            | "--output"
            | "--connect-timeout"
            | "--retry"
            | "--retry-delay"
            | "--retry-max-time"
            | "--resolve"
            | "--interface"
            | "--limit-rate"
            | "--cookie-jar"
            | "--write-out"
            | "--trace"
            | "--trace-ascii"
            | "--dump-header"
            | "--stderr"
            | "--max-redirs"
            | "--noproxy"
            | "--unix-socket"
            | "--ciphers"
            | "--oauth2-bearer"
            | "--aws-sigv4"
            | "--form-string"
            | "--request-target"
            | "--connect-to"
            | "--dns-servers"
            | "--keepalive-time"
    )
}

fn missing_value(flag: &str) -> String {
    format!("the flag `{flag}` requires a value and was ignored")
}

fn unsupported(flag: &str) -> String {
    format!("the flag `{flag}` is not supported and was ignored")
}

/// The scanner walks the document as characters while keeping byte offsets.
struct Scanner<'a> {
    chars: &'a [char],
    offsets: &'a [usize],
    position: usize,
    line: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.position += 1;
        if ch == '\n' {
            self.line += 1;
        }
        Some(ch)
    }

    /// Returns the character, its byte offset, and the line it sits on.
    fn bump_positioned(&mut self) -> Option<(char, usize, usize)> {
        let index = self.index();
        let line = self.line;
        let ch = self.bump()?;
        Some((ch, index, line))
    }

    fn index(&self) -> usize {
        self.offsets
            .get(self.position)
            .copied()
            .unwrap_or_else(|| self.offsets.last().copied().unwrap_or(0))
    }

    /// Consumes spaces and one line ending, returning whether the position was
    /// a continuation. The caller has already consumed the `\` or `^`.
    fn consume_continuation(&mut self) -> bool {
        let mut end = self.position;
        while matches!(self.chars.get(end), Some(' ' | '\t')) {
            end += 1;
        }
        let width = match self.chars.get(end) {
            Some('\n') => 1,
            Some('\r') if self.chars.get(end + 1) == Some(&'\n') => 2,
            _ => return false,
        };
        while self.position < end + width {
            self.bump();
        }
        true
    }
}

/// Collects a word's characters and remembers its start.
#[derive(Debug, Default)]
struct WordBuilder {
    text: String,
    index: Option<usize>,
    line: usize,
    shell_expands: bool,
}

impl WordBuilder {
    fn push(&mut self, ch: char, index: usize, line: usize) {
        if self.index.is_none() {
            self.index = Some(index);
            self.line = line;
        }
        self.text.push(ch);
    }

    fn mark_shell_expansion(&mut self) {
        self.shell_expands = true;
    }

    fn finish(&mut self, words: &mut Vec<Word>, command_start: &mut Option<usize>) {
        if let Some(index) = self.index.take() {
            if words.is_empty() {
                *command_start = Some(self.line);
            }
            words.push(Word {
                text: std::mem::take(&mut self.text),
                index,
                shell_expands: self.shell_expands,
            });
            self.shell_expands = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ImportBundle {
        parse_curl_text(text, "cURL import").expect("the text parses")
    }

    fn requests(bundle: &ImportBundle) -> &[RequestSpec] {
        &bundle.collections[0].requests
    }

    #[test]
    fn requests_parse_without_a_collection_until_the_caller_picks_one() {
        let import = parse_curl_requests(
            "curl 'https://api.example.com/v1/users?page=2'\n\
             curl -X POST 'https://api.example.com/v1/users'",
        )
        .expect("the text parses");
        assert_eq!(import.requests.len(), 2);
        assert_eq!(import.requests[0].name, "GET /v1/users");
        assert!(import.warnings.is_empty());
    }

    #[test]
    fn tokenize_preserves_single_quotes_and_escapes_double_quotes() {
        let commands =
            tokenize(r#"curl -H 'Accept: application/json' --data-raw "{\"a\":\"b c\"}""#);
        assert_eq!(commands.len(), 1);
        let words: Vec<&str> = commands[0]
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(
            words,
            [
                "curl",
                "-H",
                "Accept: application/json",
                "--data-raw",
                r#"{"a":"b c"}"#,
            ]
        );
        assert_eq!(commands[0].words[2].index, 9);
        assert_eq!(commands[0].line, 1);
    }

    #[test]
    fn tokenize_decodes_ansi_c_escapes_and_unquoted_backslashes() {
        let commands = tokenize(r#"curl --data-raw $'line1\nline2\t\x41' a\ b"#);
        assert_eq!(commands.len(), 1);
        let words: Vec<&str> = commands[0]
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(words, ["curl", "--data-raw", "line1\nline2\tA", "a b"]);
    }

    #[test]
    fn tokenize_joins_continued_commands_and_handles_crlf() {
        let text = "curl 'https://example.com/a' \\\r\n  -H 'Accept: text/plain'\r\n\
                    \n\
                    curl ^\r\n  'https://example.com/b'";
        let commands = tokenize(text);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].words.len(), 4);
        assert_eq!(commands[0].line, 1);
        assert_eq!(commands[1].words.len(), 2);
        assert_eq!(commands[1].line, 4);
        assert_eq!(commands[1].words[1].text, "https://example.com/b");
    }

    #[test]
    fn tokenize_keeps_unknown_double_quote_escapes_literal() {
        let commands = tokenize(r#"curl -d "line\nnext""#);
        assert_eq!(commands[0].words[2].text, r"line\nnext");
    }

    #[test]
    fn supported_flags_map_onto_the_request() {
        let bundle = parse(
            "curl -X PUT 'https://api.example.com/v1/users?page=2' \
             -H 'Accept: application/json' -H 'X-Test: 1' \
             -u alice:secret -b 'session=abc' -A 'posthaste/1' \
             -e 'https://ref.example' -m 2.5",
        );
        let request = &requests(&bundle)[0];
        assert_eq!(request.method, HttpMethod::Put);
        assert_eq!(request.name, "PUT /v1/users");
        assert_eq!(request.url, "https://api.example.com/v1/users");
        assert_eq!(request.query, vec![KeyValue::new("page", "2")]);
        assert_eq!(
            request.headers,
            vec![
                KeyValue::new("Accept", "application/json"),
                KeyValue::new("X-Test", "1"),
                KeyValue::new("Cookie", "session=abc"),
                KeyValue::new("User-Agent", "posthaste/1"),
                KeyValue::new("Referer", "https://ref.example"),
            ]
        );
        assert_eq!(
            request.auth,
            RequestAuth::Basic {
                username: "alice".to_owned(),
                password: "secret".to_owned(),
            }
        );
        assert_eq!(request.timeout_ms, Some(2_500));
    }

    #[test]
    fn methods_follow_the_explicit_flag_or_the_body() {
        let bundle = parse(
            "curl https://example.com/a\n\
             curl -d a=1 https://example.com/b\n\
             curl -X DELETE https://example.com/c\n\
             curl -G -d a=1 https://example.com/d",
        );
        let requests = requests(&bundle);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[1].method, HttpMethod::Post);
        assert_eq!(requests[2].method, HttpMethod::Delete);
        assert_eq!(requests[3].method, HttpMethod::Get);
    }

    #[test]
    fn a_rejected_file_form_part_still_makes_the_method_post() {
        let bundle = parse("curl -F 'file=@report.pdf' https://example.com/upload");
        let request = &requests(&bundle)[0];
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.body, RequestBody::None);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.reason.contains("file=@report.pdf"))
        );
    }

    #[test]
    fn bodies_choose_a_mode_from_the_effective_content_type() {
        let bundle = parse(
            "curl https://example.com/a -H 'Content-Type: application/json' -d '{\"name\":\"Rex\"}'\n\
             curl https://example.com/b -H 'Content-Type: text/plain' --data 'hello world'\n\
             curl https://example.com/c -H 'Content-Type: application/x-www-form-urlencoded' -d 'a=1&b=2'\n\
             curl https://example.com/d --data-urlencode 'name=Rex'\n\
             curl https://example.com/e -H 'Content-Type: application/json' -d a=1 -d b=2",
        );
        let requests = requests(&bundle);
        assert_eq!(
            requests[0].body,
            RequestBody::Json(r#"{"name":"Rex"}"#.to_owned())
        );
        assert_eq!(
            requests[1].body,
            RequestBody::Text("hello world".to_owned())
        );
        assert_eq!(
            requests[2].body,
            RequestBody::Form(vec![KeyValue::new("a", "1"), KeyValue::new("b", "2"),])
        );
        assert_eq!(
            requests[3].body,
            RequestBody::Form(vec![KeyValue::new("name", "Rex")])
        );
        assert_eq!(requests[4].body, RequestBody::Json("a=1&b=2".to_owned()));
    }

    #[test]
    fn get_moves_data_into_query_rows() {
        let bundle = parse(
            "curl -G 'https://example.com/search?page=2' \
             -d 'a=1' --data-urlencode 'b=two words'",
        );
        let request = &requests(&bundle)[0];
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.body, RequestBody::None);
        assert_eq!(request.url, "https://example.com/search");
        assert_eq!(
            request.query,
            vec![
                KeyValue::new("page", "2"),
                KeyValue::new("a", "1"),
                KeyValue::new("b", "two words"),
            ]
        );
    }

    #[test]
    fn unsupported_flags_and_file_parts_are_reported() {
        let bundle = parse(
            "curl --proxy http://localhost:8080 https://example.com/a\n\
             curl --data-binary @payload.json https://example.com/b\n\
             curl -F 'file=@report.pdf' -F 'name=Rex' https://example.com/c\n\
             curl -b cookies.txt https://example.com/d\n\
             curl -u alice https://example.com/e",
        );
        let requests = requests(&bundle);
        assert_eq!(requests.len(), 5);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 1" && warning.reason.contains("--proxy"))
        );
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 2" && warning.reason.contains("payload.json"))
        );
        assert!(bundle.warnings.iter().any(|warning| {
            warning.item == "line 3" && warning.reason.contains("file=@report.pdf")
        }));
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 4" && warning.reason.contains("cookies.txt"))
        );
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 5" && warning.reason.contains("password"))
        );
        assert_eq!(
            requests[2].body,
            RequestBody::Form(vec![KeyValue::new("name", "Rex")])
        );
        assert_eq!(
            requests[4].auth,
            RequestAuth::Basic {
                username: "alice".to_owned(),
                password: String::new(),
            }
        );
    }

    #[test]
    fn unknown_flags_are_reported_while_the_command_still_imports() {
        let bundle = parse(
            "curl --frobnicate https://example.com/a\n\
             curl -Z https://example.com/b",
        );
        assert_eq!(requests(&bundle).len(), 2);
        assert!(bundle.warnings.iter().any(|warning| {
            warning.item == "line 1" && warning.reason.contains("--frobnicate")
        }));
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 2" && warning.reason.contains("-Z"))
        );
    }

    #[test]
    fn accepted_no_op_flags_are_silent() {
        let bundle = parse(
            "curl -sS -kL --compressed -i -v --http1.1 --fail --globoff -# https://example.com/a",
        );
        assert!(bundle.warnings.is_empty());
        assert_eq!(requests(&bundle).len(), 1);
    }

    #[test]
    fn shell_substitutions_skip_the_whole_command() {
        let bundle = parse(
            "curl https://example.com/a\n\
             curl https://example.com/b?token=$(cat token.txt)\n\
             curl 'https://example.com/c' | jq .\n\
             curl \"https://example.com/d?token=$TOKEN\"",
        );
        assert_eq!(requests(&bundle).len(), 1);
        assert!(bundle.warnings.iter().any(|warning| {
            warning.item == "line 2" && warning.reason.contains("shell substitution")
        }));
        for line in ["line 2", "line 3", "line 4"] {
            assert!(
                bundle.warnings.iter().any(|warning| warning.item == line),
                "no warning for {line}"
            );
        }
    }

    #[test]
    fn single_quoted_shell_syntax_stays_literal() {
        let bundle = parse(
            "curl https://example.com/a -d '{\"$ref\":\"#/x\"}'\n\
             curl https://example.com/b -d 'echo `date`'",
        );
        let requests = requests(&bundle);
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body,
            RequestBody::Text(r##"{"$ref":"#/x"}"##.to_owned())
        );
        assert_eq!(
            requests[1].body,
            RequestBody::Text("echo `date`".to_owned())
        );
        assert!(
            bundle.warnings.is_empty(),
            "single quotes expand nothing: {:?}",
            bundle.warnings
        );
    }

    #[test]
    fn zero_max_time_disables_the_timeout() {
        let bundle = parse(
            "curl -m 0 https://example.com/a\n\
             curl --max-time 0.0 https://example.com/b\n\
             curl -m 0.001 https://example.com/c",
        );
        let requests = requests(&bundle);
        assert_eq!(requests[0].timeout_ms, None);
        assert_eq!(requests[1].timeout_ms, None);
        assert_eq!(requests[2].timeout_ms, Some(1));
        assert!(
            bundle.warnings.is_empty(),
            "zero is a valid timeout value: {:?}",
            bundle.warnings
        );
    }

    #[test]
    fn non_curl_lines_comments_and_blank_lines_are_skipped_with_a_warning() {
        let bundle = parse("\n\n# copy as cURL\nset -x\ncurl https://example.com/a\n");
        assert_eq!(requests(&bundle).len(), 1);
        assert!(bundle.warnings.iter().any(|warning| {
            warning.item == "line 3" && warning.reason.contains("not a cURL command")
        }));
        assert!(bundle.warnings.iter().any(|warning| {
            warning.item == "line 4" && warning.reason.contains("not a cURL command")
        }));
    }

    #[test]
    fn commands_without_a_url_or_with_an_unstorable_method_are_skipped() {
        let bundle = parse(
            "curl -X POST\n\
             curl -X TRACE https://example.com/a\n\
             curl https://example.com/b",
        );
        let requests = requests(&bundle);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://example.com/b");
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| { warning.item == "line 1" && warning.reason.contains("no URL") })
        );
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| { warning.item == "line 2" && warning.reason.contains("TRACE") })
        );
    }

    #[test]
    fn names_use_the_method_and_path_and_keep_placeholders_literal() {
        let bundle = parse(
            "curl '{{base_url}}/users'\n\
             curl -X POST 'https://api.example.com/v1/users?page=2'",
        );
        let requests = requests(&bundle);
        assert_eq!(requests[0].name, "GET {{base_url}}/users");
        assert_eq!(requests[0].url, "{{base_url}}/users");
        assert_eq!(requests[1].name, "POST /v1/users");
    }

    #[test]
    fn three_commands_become_one_collection_with_reported_positions() {
        let text = "\ncurl https://api.example.com/v1/users\n\
                    curl -X POST 'https://api.example.com/v1/users'\n\
                    curl --proxy http://localhost:8080\n";
        let bundle = parse(text);
        assert_eq!(bundle.collections.len(), 1);
        assert_eq!(bundle.collections[0].name, "cURL import");
        let names: Vec<&str> = bundle.collections[0]
            .requests
            .iter()
            .map(|request| request.name.as_str())
            .collect();
        assert_eq!(names, ["GET /v1/users", "POST /v1/users"]);
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 4" && warning.reason.contains("--proxy"))
        );
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.item == "line 4" && warning.reason.contains("no URL"))
        );
    }

    #[test]
    fn oversized_text_is_refused() {
        let text = "x".repeat(MAX_CURL_TEXT_BYTES + 1);
        let error = parse_curl_text(&text, "cURL import").expect_err("too large");
        assert_eq!(
            error,
            InterchangeError::CurlTextTooLarge {
                size: MAX_CURL_TEXT_BYTES + 1,
                limit: MAX_CURL_TEXT_BYTES,
            }
        );
        assert!(error.to_string().contains("1048576"));
    }

    /// The README is the panel's public documentation, so its example must be
    /// text this parser actually accepts.
    #[test]
    fn the_readme_curl_examples_parse() {
        let readme = include_str!("../../../../README.md");
        let section = readme
            .split("### Pasting cURL")
            .nth(1)
            .expect("the README documents pasting cURL");
        let block = section
            .split("```sh")
            .nth(1)
            .expect("the section has a shell example");
        let document = block.split("```").next().expect("the example is closed");
        let bundle = parse_curl_text(document, "README cURL").expect("the examples parse");
        let requests = &bundle.collections[0].requests;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].name, "GET /v1/users");
        assert_eq!(requests[0].query, vec![KeyValue::new("page", "2")]);
        assert!(
            requests[0].headers.iter().any(|header| {
                header.key == "Authorization" && header.value == "Bearer {{token}}"
            })
        );
        assert_eq!(requests[1].name, "POST /v1/users");
        assert_eq!(
            requests[1].body,
            RequestBody::Json(r#"{"name":"Rex","tags":["dog"]}"#.to_owned())
        );
    }
}
