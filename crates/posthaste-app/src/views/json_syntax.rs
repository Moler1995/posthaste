//! Lexical JSON highlighting for one editor line.
//!
//! The tokenizer never spans a newline: a JSON string cannot contain a raw
//! newline, so tokenizing each hard-wrapped line is enough even when the text
//! is malformed. Tokens always cover the whole line so a caller can shape them
//! as runs without changing character indices.

use std::ops::Range;

/// How one span of a line is colored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A string followed by `:` (after optional whitespace).
    Key,
    String,
    Number,
    /// `true`, `false`, or `null`.
    Literal,
    /// Braces, brackets, commas, and colons.
    Punctuation,
    /// Whitespace and anything the lexer does not recognize.
    Plain,
}

/// Lexical state carried from one display row to the next.
///
/// A display row can begin inside a string that an earlier row opened, either
/// because a physical line was split at [`super::code_lines::MAX_DISPLAY_CHARS`]
/// or because a streamed body arrived in pieces. The escape flag says whether
/// the character before the row is a backslash escaping the row's first
/// character.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ScanState {
    /// Outside a string.
    #[default]
    Outside,
    /// Inside a string, after a character that does not escape the next one.
    InString,
    /// Inside a string whose last character was a backslash, so the next
    /// character is escaped.
    InStringEscaped,
}

impl ScanState {
    /// State after reading one more character.
    pub(crate) fn advance(self, ch: char) -> Self {
        match (self, ch) {
            (Self::InStringEscaped, _) => Self::InString,
            (Self::InString, '\\') => Self::InStringEscaped,
            (Self::InString, '"') => Self::Outside,
            (Self::InString, _) => Self::InString,
            (Self::Outside, '"') => Self::InString,
            (Self::Outside, _) => Self::Outside,
        }
    }
}

/// Splits one line into contiguous, whole-line-covering spans.
pub fn tokenize(line: &str) -> Vec<(Range<usize>, TokenKind)> {
    tokenize_from(line, ScanState::Outside)
}

/// Splits a row that may begin inside a string into contiguous spans.
///
/// The part before the closing quote continues the earlier row's string and is
/// one [`TokenKind::String`] span. It is never a key: the `:` that would mark a
/// key falls on a later row. Once that string closes, the rest of the row is
/// tokenized normally.
pub fn tokenize_from(line: &str, start: ScanState) -> Vec<(Range<usize>, TokenKind)> {
    let mut tokens: Vec<(Range<usize>, TokenKind)> = Vec::new();
    let mut index = 0;
    if start != ScanState::Outside {
        index = scan_continuation(line, start == ScanState::InStringEscaped);
        if index > 0 {
            tokens.push((0..index, TokenKind::String));
        }
    }
    tokenize_rest(line, index, &mut tokens);
    tokens
}

/// Appends the tokens of every line that starts outside a string.
fn tokenize_rest(line: &str, mut index: usize, tokens: &mut Vec<(Range<usize>, TokenKind)>) {
    while index < line.len() {
        let start = index;
        let ch = line[index..]
            .chars()
            .next()
            .expect("index is inside the line");
        let (end, kind) = match ch {
            ch if ch.is_whitespace() => {
                let mut end = index + ch.len_utf8();
                while end < line.len() {
                    let next = line[end..].chars().next().expect("boundary");
                    if !next.is_whitespace() {
                        break;
                    }
                    end += next.len_utf8();
                }
                (end, TokenKind::Plain)
            }
            '"' => {
                let end = scan_string(line, index);
                (end, string_kind(line, end))
            }
            '-' if !line[index + 1..].starts_with(|next: char| next.is_ascii_digit()) => {
                (index + 1, TokenKind::Plain)
            }
            '-' | '0'..='9' => (scan_number(line, index), TokenKind::Number),
            ch if ch.is_ascii_alphabetic() => {
                let mut end = index;
                while end < line.len() {
                    let next = line[end..].chars().next().expect("boundary");
                    if !next.is_ascii_alphanumeric() && next != '_' {
                        break;
                    }
                    end += next.len_utf8();
                }
                let kind = match &line[index..end] {
                    "true" | "false" | "null" => TokenKind::Literal,
                    _ => TokenKind::Plain,
                };
                (end, kind)
            }
            '{' | '}' | '[' | ']' | ',' | ':' => (index + 1, TokenKind::Punctuation),
            ch => (index + ch.len_utf8(), TokenKind::Plain),
        };

        // Adjacent plain spans paint identically, so they merge into one run.
        if kind == TokenKind::Plain
            && let Some((range, last_kind)) = tokens.last_mut()
            && *last_kind == TokenKind::Plain
            && range.end == start
        {
            range.end = end;
            index = end;
            continue;
        }
        tokens.push((start..end, kind));
        index = end;
    }
}

/// Returns the index just past the closing quote, or the end of the line when
/// the string is unterminated.
fn scan_string(line: &str, start: usize) -> usize {
    let mut end = start + 1;
    let mut escaped = false;
    while end < line.len() {
        let ch = line[end..].chars().next().expect("boundary");
        end += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => break,
            _ => {}
        }
    }
    end
}

/// Returns the index just past the quote that closes a string opened by an
/// earlier row, or the end of the row when it stays open. `escaped` says the
/// row's first character follows an escaping backslash.
fn scan_continuation(line: &str, mut escaped: bool) -> usize {
    let mut end = 0;
    while end < line.len() {
        let ch = line[end..].chars().next().expect("boundary");
        end += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => break,
            _ => {}
        }
    }
    end
}

/// A closed string followed by `:` (after optional whitespace) is a key.
fn string_kind(line: &str, end: usize) -> TokenKind {
    let mut after = end;
    while after < line.len() {
        let next = line[after..].chars().next().expect("boundary");
        if !next.is_whitespace() {
            break;
        }
        after += next.len_utf8();
    }
    if line[after..].starts_with(':') {
        TokenKind::Key
    } else {
        TokenKind::String
    }
}

/// Scans a JSON number: optional sign, digits, optional fraction, optional
/// exponent. The result is lexical, not validated.
fn scan_number(line: &str, start: usize) -> usize {
    let bytes = line.as_bytes();
    let mut end = start;
    if bytes[end] == b'-' {
        end += 1;
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if end < bytes.len() && matches!(bytes[end], b'e' | b'E') {
        let mut probe = end + 1;
        if probe < bytes.len() && matches!(bytes[probe], b'+' | b'-') {
            probe += 1;
        }
        if probe < bytes.len() && bytes[probe].is_ascii_digit() {
            end = probe;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<TokenKind> {
        tokenize(line).into_iter().map(|(_, kind)| kind).collect()
    }

    fn texts(line: &str) -> Vec<&str> {
        tokenize(line)
            .into_iter()
            .map(|(range, _)| &line[range])
            .collect()
    }

    #[test]
    fn tokens_cover_the_whole_line() {
        for line in [
            r#"{"name": "posthaste", "count": 2, "ok": true, "none": null}"#,
            "  [1, -2.5e3, \"a\\\"b\"]  ",
            "not json: 你好",
            "",
            "{",
        ] {
            let tokens = tokenize(line);
            let mut cursor = 0;
            for (range, _) in &tokens {
                assert_eq!(range.start, cursor, "gap in {line:?}");
                cursor = range.end;
            }
            assert_eq!(cursor, line.len(), "trailing gap in {line:?}");
        }
    }

    #[test]
    fn keys_and_string_values_differ() {
        assert_eq!(
            texts(r#"{"name": "posthaste"}"#),
            vec!["{", "\"name\"", ":", " ", "\"posthaste\"", "}"]
        );
        assert_eq!(
            kinds(r#"{"name": "posthaste"}"#),
            vec![
                TokenKind::Punctuation,
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::String,
                TokenKind::Punctuation,
            ]
        );
        // Whitespace between the name and colon still marks the name as a key.
        assert_eq!(kinds(r#""name"  : 1"#)[0], TokenKind::Key);
    }

    #[test]
    fn escapes_stay_inside_the_string() {
        let line = r#""a\\": "b\""#;
        assert_eq!(
            kinds(line),
            vec![
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::String,
            ]
        );
    }

    #[test]
    fn numbers_and_literals_are_split() {
        assert_eq!(
            kinds("[-1.5e+3, true, false, null, 0]"),
            vec![
                TokenKind::Punctuation,
                TokenKind::Number,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::Literal,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::Literal,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::Literal,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::Number,
                TokenKind::Punctuation,
            ]
        );
    }

    #[test]
    fn invalid_input_stays_plain_and_does_not_panic() {
        assert_eq!(kinds("{"), vec![TokenKind::Punctuation]);
        assert_eq!(kinds("truely"), vec![TokenKind::Plain]);
        // Adjacent plain spans merge into one.
        assert_eq!(kinds("- x"), vec![TokenKind::Plain]);
        assert_eq!(
            texts("unterminated \"text"),
            vec!["unterminated ", "\"text"],
            "the word and the space merge into one plain span"
        );
        assert_eq!(
            kinds("unterminated \"text"),
            vec![TokenKind::Plain, TokenKind::String]
        );
    }

    fn state_after(text: &str) -> ScanState {
        text.chars().fold(ScanState::Outside, ScanState::advance)
    }

    fn kind_at(tokens: &[(Range<usize>, TokenKind)], position: usize) -> TokenKind {
        tokens
            .iter()
            .find(|(range, _)| range.start <= position && position < range.end)
            .map(|(_, kind)| *kind)
            .expect("the tokens cover the text")
    }

    #[test]
    fn scan_state_tracks_strings_and_escapes() {
        assert_eq!(state_after("plain"), ScanState::Outside);
        assert_eq!(state_after(r#""open"#), ScanState::InString);
        assert_eq!(state_after(r#""esc\"#), ScanState::InStringEscaped);
        assert_eq!(state_after(r#""esc\""#), ScanState::InString);
        assert_eq!(state_after(r#""closed""#), ScanState::Outside);
    }

    #[test]
    fn a_value_string_split_at_every_boundary_keeps_its_tones() {
        let line = r#"{"k":"ab\\\"cd\\ne long tail","n":-1.5e+3,"ok":true,"x":null}"#;
        let whole = tokenize(line);
        // Display rows split long values, which are strings. A boundary inside
        // a short key, number, or literal is not a split the splitter makes.
        let value = whole
            .iter()
            .find(|(_, kind)| *kind == TokenKind::String)
            .expect("the line has a value string")
            .0
            .clone();
        for (index, _) in line[value.clone()].char_indices() {
            let index = value.start + index;
            let suffix = &line[index..];
            let tokens = tokenize_from(suffix, state_after(&line[..index]));

            let mut cursor = 0;
            for (range, _) in &tokens {
                assert_eq!(range.start, cursor, "gap at boundary {index}");
                cursor = range.end;
            }
            assert_eq!(cursor, suffix.len(), "trailing gap at boundary {index}");

            for (offset, _) in suffix.char_indices() {
                assert_eq!(
                    kind_at(&tokens, offset),
                    kind_at(&whole, index + offset),
                    "tone at byte {} after boundary {index}",
                    index + offset
                );
            }
        }
    }

    #[test]
    fn an_escaped_quote_at_the_split_stays_escaped() {
        let line = r#""a\""#;
        // The boundary sits between the backslash and the quote it escapes,
        // so that quote cannot close the string; the row stays open.
        let tokens = tokenize_from(&line[3..], state_after(&line[..3]));
        assert_eq!(tokens, vec![(0..1, TokenKind::String)]);
    }

    #[test]
    fn a_closed_continuation_returns_to_normal_tokens() {
        let line = r#""long value": 1, "x"}"#;
        let tokens = tokenize_from(&line[3..], state_after(&line[..3]));
        assert_eq!(
            kinds_from(&tokens),
            vec![
                TokenKind::String,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::Number,
                TokenKind::Punctuation,
                TokenKind::Plain,
                TokenKind::String,
                TokenKind::Punctuation,
            ]
        );
    }

    fn kinds_from(tokens: &[(Range<usize>, TokenKind)]) -> Vec<TokenKind> {
        tokens.iter().map(|(_, kind)| *kind).collect()
    }

    #[test]
    fn multi_byte_text_keeps_indices_on_boundaries() {
        let line = r#"{"键": "值"}"#;
        for (range, _) in tokenize(line) {
            assert!(line.is_char_boundary(range.start));
            assert!(line.is_char_boundary(range.end));
        }
        assert_eq!(texts(line), vec!["{", "\"键\"", ":", " ", "\"值\"", "}"]);
    }
}
