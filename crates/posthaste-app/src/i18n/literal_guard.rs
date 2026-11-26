//! Source scan that keeps user-visible English out of the production code.
//!
//! Interface prose belongs in [`super::catalogue`]. A literal that looks like
//! an English sentence in a production region is either a missed translation or
//! a deliberate exception, and the exceptions are listed here rather than
//! scattered through the views.
//!
//! The scan is a small lexer rather than a line grep: it blanks comments and
//! string contents so a commented-out sentence or a brace inside a string
//! cannot confuse the structure, skips `#[cfg(test)]` items to the end of their
//! braces, and ignores literals inside macros that never reach a frame
//! (`assert!`, `panic!`, `tracing::warn!`) and inside `expect` messages.
//!
//! A per-file baseline makes this a ratchet: the count may only fall. Adding an
//! exemption is a deliberate edit to [`ALLOWED_LITERALS`], and lowering the
//! baseline after a translation is the expected upkeep.

use std::path::{Path, PathBuf};

/// Files scanned beyond `src/views/**` (paths are relative to `src/`).
const EXTRA_SOURCES: &[&str] = &["app.rs", "history.rs", "workspace.rs"];

/// Literals that are intentionally not translated.
const ALLOWED_LITERALS: &[&str] = &[
    // A protocol sample: the two subprotocols are tokens, not prose.
    "graphql-ws, chat",
    // Default object names are stored in workspace documents.
    "cURL import",
    "Untitled request",
    // Reasons recorded on a session-only cookie jar; they are diagnostics.
    "tests keep the cookie jar in memory",
    "the workspace could not be opened",
    // An I/O diagnostic inside the response viewer's writer.
    "formatted output exceeded its limit",
    // Key-context ids used to resolve editor key bindings.
    "TextInput VariableCompletion",
    "TextArea VariableCompletion",
    // A strftime pattern: a format token, not a sentence.
    "%Y-%m-%d %H:%M:%S",
    // Reasons passed to the save-path flush; they are only recorded in
    // tracing fields and never reach a frame.
    "closing the window",
    "opening a workspace in a new window",
    "switching workspaces",
    "recording a completed run",
    "changing the request selection",
];

/// Files the parent module compiles only under `#[cfg(test)]`, so no literal
/// in them can reach a frame. The item-level scan cannot see the module
/// declaration in `views/mod.rs`.
const TEST_ONLY_SOURCES: &[&str] = &["views/editor_suite.rs"];

/// Macros whose string arguments never reach the interface.
const QUIET_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "panic",
    "unimplemented",
    "unreachable",
    "todo",
];

/// `tracing` event macros, whose fields are diagnostics.
const QUIET_TRACING_MACROS: &[&str] = &["debug", "error", "info", "trace", "warn"];

/// The largest number of English-looking literals each scanned file may hold.
///
/// The count may only be lowered. A file missing from the list must be clean.
const BASELINES: &[(&str, usize)] = &[
    // Three storage-layer reasons that the app presents as diagnostic detail
    // inside a translated sentence, so they keep their original text.
    ("workspace.rs", 3),
];

/// One English-looking literal in a production region.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Offender {
    line: usize,
    text: String,
}

/// The scanned sources, relative to `src/`.
fn scanned_sources(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = EXTRA_SOURCES.iter().map(|path| path.to_string()).collect();
    let mut pending: Vec<PathBuf> = vec![root.join("views")];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(
                    path.strip_prefix(root)
                        .expect("a path under src")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    files.sort();
    files.retain(|file| !TEST_ONLY_SOURCES.contains(&file.as_str()));
    files
}

/// The English-looking literals in `source` that reach a frame.
fn offenders_in(source: &str) -> Vec<Offender> {
    let lexed = lex(source);
    let excluded = cfg_test_ranges(&lexed.clean);
    let quiet = quiet_ranges(&lexed.clean);
    lexed
        .literals
        .into_iter()
        .filter(|literal| !inside(&excluded, literal.start) && !inside(&quiet, literal.start))
        .filter(|literal| looks_like_prose(&literal.text) && !allowed(&literal.text))
        .map(|literal| Offender {
            line: literal.line,
            text: literal.text,
        })
        .collect()
}

/// Whether a literal is a deliberate exception.
fn allowed(text: &str) -> bool {
    let text = text.trim();
    if ALLOWED_LITERALS.contains(&text) {
        return true;
    }
    // Default object names are stored in workspace documents, not rendered from
    // the catalogue.
    if let Some(rest) = text.strip_prefix("New ")
        && ["Request", "Collection", "Folder", "Environment"]
            .iter()
            .any(|noun| {
                rest == *noun
                    || rest
                        .strip_prefix(*noun)
                        .is_some_and(|tail| tail.parse::<u32>().is_ok())
            })
    {
        return true;
    }
    false
}

/// Whether `text` reads as an English sentence rather than a token or an id.
///
/// Placeholders are removed first, so `{name}: {value}` is a template rather
/// than two words. Two words of at least two ASCII letters are then required,
/// so `a/b`, `30000`, `{milliseconds} ms`, and `row-menu-{}-{}` are not prose.
fn looks_like_prose(text: &str) -> bool {
    let mut words = String::with_capacity(text.len());
    let mut depth = 0usize;
    for character in text.chars() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => words.push(character),
            _ => {}
        }
    }
    words
        .split_whitespace()
        .filter(|word| {
            word.chars()
                .filter(|character| character.is_ascii_alphabetic())
                .count()
                >= 2
        })
        .count()
        >= 2
}

fn inside(ranges: &[(usize, usize)], position: usize) -> bool {
    ranges
        .iter()
        .any(|(start, end)| position >= *start && position < *end)
}

/// Why `offenders` no longer matches the pinned count, if it does not.
fn baseline_failure(relative: &str, offenders: &[Offender], baseline: usize) -> Option<String> {
    (offenders.len() != baseline).then(|| {
        let mut message = format!(
            "{relative} holds {} English literals, the baseline is {baseline}:\n",
            offenders.len()
        );
        for offender in offenders {
            message.push_str(&format!(
                "  {relative}:{}: \"{}\"\n",
                offender.line, offender.text
            ));
        }
        message
    })
}

/// A string literal plus where it starts, in character positions.
struct Literal {
    line: usize,
    start: usize,
    text: String,
}

/// The source after lexing.
struct Lexed {
    /// The source with comment and literal characters blanked, so braces and
    /// keywords can be matched without a string confusing them.
    clean: Vec<char>,
    literals: Vec<Literal>,
}

fn lex(source: &str) -> Lexed {
    let chars: Vec<char> = source.chars().collect();
    let mut clean = chars.clone();
    let mut literals = Vec::new();
    let mut line = 1;
    let mut line_at = Vec::with_capacity(chars.len());
    for &character in &chars {
        line_at.push(line);
        if character == '\n' {
            line += 1;
        }
    }
    let at = |index: usize| chars.get(index).copied();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '/' && at(index + 1) == Some('/') {
            while index < chars.len() && chars[index] != '\n' {
                clean[index] = ' ';
                index += 1;
            }
        } else if character == '/' && at(index + 1) == Some('*') {
            let mut depth = 0usize;
            while index < chars.len() {
                if chars[index] == '/' && at(index + 1) == Some('*') {
                    depth += 1;
                    clean[index] = ' ';
                    if index + 1 < chars.len() {
                        clean[index + 1] = ' ';
                    }
                    index += 2;
                } else if chars[index] == '*' && at(index + 1) == Some('/') {
                    depth -= 1;
                    clean[index] = ' ';
                    if index + 1 < chars.len() {
                        clean[index + 1] = ' ';
                    }
                    index += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if chars[index] != '\n' {
                        clean[index] = ' ';
                    }
                    index += 1;
                }
            }
        } else if character == 'r' && raw_string(&chars, index).is_some() {
            let (quote, hashes) = raw_string(&chars, index).expect("checked above");
            let content_start = quote + 1;
            let mut end = chars.len();
            for (candidate, &character) in chars.iter().enumerate().skip(content_start) {
                if character == '"'
                    && (1..=hashes).all(|offset| at(candidate + offset) == Some('#'))
                {
                    end = candidate;
                    break;
                }
            }
            literals.push(Literal {
                line: line_at[index],
                start: index,
                text: chars[content_start..end].iter().collect(),
            });
            let last = (end + hashes).min(chars.len() - 1);
            for blank in index..=last {
                if chars[blank] != '\n' {
                    clean[blank] = ' ';
                }
            }
            index = last + 1;
        } else if character == '"' {
            let mut end = chars.len();
            let mut cursor = index + 1;
            while cursor < chars.len() {
                if chars[cursor] == '\\' {
                    cursor += 2;
                    continue;
                }
                if chars[cursor] == '"' {
                    end = cursor;
                    break;
                }
                cursor += 1;
            }
            let content_end = end.min(chars.len());
            literals.push(Literal {
                line: line_at[index],
                start: index,
                text: chars[index + 1..content_end].iter().collect(),
            });
            let last = content_end.min(chars.len() - 1);
            for blank in index..=last {
                if chars[blank] != '\n' {
                    clean[blank] = ' ';
                }
            }
            index = last + 1;
        } else if character == '\'' {
            if let Some(length) = char_literal(&chars, index) {
                for cell in clean
                    .iter_mut()
                    .take((index + length).min(chars.len()))
                    .skip(index)
                {
                    *cell = ' ';
                }
                index += length;
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    Lexed { clean, literals }
}

/// A raw string begins at `index`: `r"`, `r#"`, `br#"`, and so on.
fn raw_string(chars: &[char], index: usize) -> Option<(usize, usize)> {
    let is_identifier = |position: usize| {
        chars
            .get(position)
            .is_some_and(|character| character.is_alphanumeric() || *character == '_')
    };
    if index > 0 && is_identifier(index - 1) && chars[index - 1] != 'b' {
        return None;
    }
    let mut cursor = index + 1;
    let mut hashes = 0;
    while chars.get(cursor) == Some(&'#') {
        hashes += 1;
        cursor += 1;
    }
    (chars.get(cursor) == Some(&'"')).then_some((cursor, hashes))
}

/// The length of a character literal beginning at `index`, or `None` for a
/// lifetime such as `'a`.
fn char_literal(chars: &[char], index: usize) -> Option<usize> {
    match chars.get(index + 1) {
        Some('\\') => {
            let mut cursor = index + 2;
            while cursor < chars.len() && cursor <= index + 12 {
                if chars[cursor] == '\'' {
                    return Some(cursor - index + 1);
                }
                cursor += 1;
            }
            None
        }
        _ if chars.get(index + 2) == Some(&'\'') => Some(3),
        _ => None,
    }
}

/// The ranges of items introduced by `#[cfg(test)]`, attribute included.
fn cfg_test_ranges(clean: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index + 1 < clean.len() {
        if clean[index] == '#'
            && clean[index + 1] == '['
            && let Some(close) = matching(clean, index + 1, '[', ']')
        {
            let body: String = clean[index + 2..close]
                .iter()
                .filter(|character| !character.is_whitespace())
                .collect();
            if body == "cfg(test)" {
                let end = item_end(clean, close + 1);
                ranges.push((index, end));
                index = end;
                continue;
            }
            index = close + 1;
            continue;
        }
        index += 1;
    }
    ranges
}

/// The end of the item that begins after `from`, braces or semicolon included.
fn item_end(clean: &[char], from: usize) -> usize {
    let mut index = from;
    loop {
        while index < clean.len() && clean[index].is_whitespace() {
            index += 1;
        }
        if index + 1 < clean.len() && clean[index] == '#' && clean[index + 1] == '[' {
            match matching(clean, index + 1, '[', ']') {
                Some(close) => index = close + 1,
                None => return clean.len(),
            }
        } else {
            break;
        }
    }
    let mut parens = 0usize;
    while index < clean.len() {
        match clean[index] {
            '(' => parens += 1,
            ')' => parens = parens.saturating_sub(1),
            '{' if parens == 0 => {
                return matching(clean, index, '{', '}').map_or(clean.len(), |close| close + 1);
            }
            ';' if parens == 0 => return index + 1,
            _ => {}
        }
        index += 1;
    }
    clean.len()
}

/// The ranges of macro and `expect` arguments whose strings never reach a frame.
fn quiet_ranges(clean: &[char]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < clean.len() {
        let Some((name, after)) = identifier_at(clean, index) else {
            index += 1;
            continue;
        };
        let mut cursor = after;
        while cursor < clean.len() && clean[cursor].is_whitespace() {
            cursor += 1;
        }
        let quiet = clean.get(cursor) == Some(&'!')
            && (QUIET_MACROS.contains(&name.as_str())
                || (is_tracing_path(clean, index)
                    && QUIET_TRACING_MACROS.contains(&name.as_str())));
        if clean.get(cursor) == Some(&'!') {
            cursor += 1;
        } else if name == "expect" {
            if clean.get(cursor) != Some(&'(') {
                index = after;
                continue;
            }
            ranges.extend(paren_range(clean, cursor));
            index = after;
            continue;
        } else {
            index = after;
            continue;
        }
        while cursor < clean.len() && clean[cursor].is_whitespace() {
            cursor += 1;
        }
        if quiet && clean.get(cursor) == Some(&'(') {
            ranges.extend(paren_range(clean, cursor));
        }
        index = after;
    }
    ranges
}

/// Whether the identifier at `index` is the last segment of `tracing::…`.
fn is_tracing_path(clean: &[char], index: usize) -> bool {
    let mut cursor = index;
    loop {
        // Walk back over `::` and the identifier before it.
        let mut before = cursor;
        while before > 0 && clean[before - 1].is_whitespace() {
            before -= 1;
        }
        if before < 2 || clean[before - 1] != ':' || clean[before - 2] != ':' {
            return false;
        }
        let end = before - 2;
        let mut start = end;
        while start > 0 && (clean[start - 1].is_alphanumeric() || clean[start - 1] == '_') {
            start -= 1;
        }
        let segment: String = clean[start..end].iter().collect();
        if segment == "tracing" {
            return true;
        }
        cursor = start;
    }
}

/// The identifier starting at `index` and the position after it.
fn identifier_at(clean: &[char], index: usize) -> Option<(String, usize)> {
    if index > 0 && (clean[index - 1].is_alphanumeric() || clean[index - 1] == '_') {
        return None;
    }
    if !(clean[index].is_alphabetic() || clean[index] == '_') {
        return None;
    }
    let mut end = index;
    while end < clean.len() && (clean[end].is_alphanumeric() || clean[end] == '_') {
        end += 1;
    }
    Some((clean[index..end].iter().collect(), end))
}

/// The range from the opening parenthesis at `open` through its match.
fn paren_range(clean: &[char], open: usize) -> Option<(usize, usize)> {
    matching(clean, open, '(', ')').map(|close| (open, close + 1))
}

/// The position of the `close` matching the `open` at `open_index`.
fn matching(clean: &[char], open_index: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (index, &character) in clean.iter().enumerate().skip(open_index) {
        if character == open {
            depth += 1;
        } else if character == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planted(source: &str) -> Vec<Offender> {
        offenders_in(source)
    }

    #[test]
    fn a_planted_sentence_in_production_is_reported_with_its_line() {
        let offenders = planted("fn save() {\n    let message = \"Save failed\";\n}\n");
        assert_eq!(
            offenders,
            vec![Offender {
                line: 2,
                text: "Save failed".to_owned(),
            }]
        );
    }

    #[test]
    fn a_cfg_test_item_is_skipped_to_the_end_of_its_braces() {
        let source = concat!(
            "fn production() {}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    fn helper() {\n",
            "        let message = \"Save failed\";\n",
            "    }\n",
            "}\n",
            "fn after() { let message = \"Save failed\"; }\n",
        );
        let offenders = planted(source);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert_eq!(offenders[0].line, 8);
    }

    #[test]
    fn a_cfg_test_helper_between_production_lines_is_skipped() {
        let source = concat!(
            "fn first() {}\n",
            "#[cfg(test)]\n",
            "fn helper() { let message = \"Save failed\"; }\n",
            "fn second() { let message = \"Save failed\"; }\n",
        );
        let offenders = planted(source);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert_eq!(offenders[0].line, 4);
    }

    #[test]
    fn comments_strings_and_tokens_are_not_prose() {
        let source = concat!(
            "// Save failed in a comment\n",
            "/* Save failed in a block comment */\n",
            "fn example() {\n",
            "    let url = \"https://example.com\";\n",
            "    let selector = \"row-menu-{}-{}\";\n",
            "    let default_name = \"New Request\";\n",
            "    let sample = \"graphql-ws, chat\";\n",
            "}\n",
        );
        assert_eq!(planted(source), Vec::new());
    }

    #[test]
    fn assertion_and_diagnostic_arguments_are_skipped() {
        let source = concat!(
            "fn example() {\n",
            "    assert!(\n",
            "        ready,\n",
            "        \"the server could not be reached\"\n",
            "    );\n",
            "    let value = load().expect(\"the file must load\");\n",
            "    tracing::warn!(\"the request was not sent\");\n",
            "    let message = \"Save failed\";\n",
            "}\n",
        );
        let offenders = planted(source);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert_eq!(offenders[0].line, 8);
    }

    #[test]
    fn the_baseline_lists_only_scanned_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let scanned = scanned_sources(&root);
        for (path, _) in BASELINES {
            assert!(
                scanned.iter().any(|candidate| candidate == path),
                "{path} is pinned but not scanned"
            );
        }
        for path in TEST_ONLY_SOURCES {
            assert!(
                root.join(path).is_file(),
                "{path} is listed as test-only but does not exist"
            );
            assert!(
                !scanned.iter().any(|candidate| candidate == path),
                "{path} is test-only but was scanned"
            );
        }
    }

    #[test]
    fn production_sources_hold_no_new_english_prose() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut failures = Vec::new();
        for relative in scanned_sources(&root) {
            let source = std::fs::read_to_string(root.join(&relative))
                .unwrap_or_else(|error| panic!("read {relative}: {error}"));
            let offenders = offenders_in(&source);
            let baseline = BASELINES
                .iter()
                .find(|(path, _)| *path == relative)
                .map_or(0, |(_, count)| *count);
            if let Some(failure) = baseline_failure(&relative, &offenders, baseline) {
                failures.push(failure);
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn the_ratchet_fails_on_a_new_literal_and_on_an_unlowered_baseline() {
        let offenders = offenders_in("fn save() { let message = \"Save failed\"; }");
        assert!(baseline_failure("synthetic.rs", &offenders, 0).is_some());
        assert!(baseline_failure("synthetic.rs", &offenders, 1).is_none());
        assert!(
            baseline_failure("synthetic.rs", &offenders, 2).is_some(),
            "a baseline above the current count must be lowered"
        );
    }
}
