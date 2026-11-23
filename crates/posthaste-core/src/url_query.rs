//! Splitting and joining the query string an editor shows in its URL field.
//!
//! The stored request keeps its query in rows and its URL as a base URL. The
//! URL field presents the two as one string, so the boundary rules live here
//! once and are shared by the editor, load normalization, history, and search.
//!
//! Encoding here is display encoding, not wire encoding: `{{name}}`
//! placeholders stay readable, and the actual percent-encoding for the wire
//! still happens at send time.

use crate::model::KeyValue;

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Splits a URL into its base URL and the query rows its query string
/// represents.
///
/// The base keeps its fragment. Segments with an empty key are dropped: the
/// URL cannot represent them, and the editor keeps such rows itself.
pub fn split_url_query(url: &str) -> (String, Vec<KeyValue>) {
    let (head, fragment) = match url.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (url, None),
    };
    let (base, query) = match head.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (head, None),
    };

    let mut base = base.to_owned();
    if let Some(fragment) = fragment {
        base.push('#');
        base.push_str(fragment);
    }

    let rows = query.map(parse_query).unwrap_or_default();
    (base, rows)
}

/// Renders a base URL and the rows it represents as the URL field shows them.
///
/// Only enabled rows with a key appear. The query is inserted before the
/// fragment, and no `?` is added when there is nothing to append.
pub fn join_url_query(base: &str, rows: &[KeyValue]) -> String {
    let (head, fragment) = match base.split_once('#') {
        Some((head, fragment)) => (head, Some(fragment)),
        None => (base, None),
    };
    // A base that still carries a query is normalized on load, but joining must
    // stay idempotent if one arrives anyway.
    let head = match head.split_once('?') {
        Some((head, _)) => head,
        None => head,
    };

    let mut url = head.to_owned();
    let mut first = true;
    for row in rows.iter().filter(|row| row.enabled && !row.key.is_empty()) {
        url.push(if first { '?' } else { '&' });
        first = false;
        url.push_str(&encode_component(&row.key));
        url.push('=');
        url.push_str(&encode_component(&row.value));
    }

    if let Some(fragment) = fragment {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

/// Reconciles parsed URL rows with the rows a request already holds.
///
/// The parsed rows are the represented ones. Rows the URL cannot express — a
/// disabled row or an enabled row with an empty key — are kept at the position
/// they had, measured as the number of represented rows that preceded them.
/// A row that follows a represented row but has none after it trails the parsed
/// rows, so a parameter the user just typed appears above it rather than below;
/// a row before every represented row stays at the front.
pub fn merge_query_rows(parsed: Vec<KeyValue>, current: &[KeyValue]) -> Vec<KeyValue> {
    let represented_rows: Vec<bool> = current
        .iter()
        .map(|row| row.enabled && !row.key.is_empty())
        .collect();
    let mut anchors: Vec<Vec<KeyValue>> = vec![Vec::new(); parsed.len() + 1];
    let mut represented = 0;
    for (index, row) in current.iter().enumerate() {
        if represented_rows[index] {
            represented += 1;
        } else if represented > 0 && !represented_rows[index + 1..].iter().any(|row| *row) {
            anchors[parsed.len()].push(row.clone());
        } else {
            anchors[represented.min(parsed.len())].push(row.clone());
        }
    }

    let mut rows = Vec::with_capacity(parsed.len() + current.len());
    for (index, mut anchored) in anchors.into_iter().enumerate() {
        rows.append(&mut anchored);
        if let Some(row) = parsed.get(index) {
            rows.push(row.clone());
        }
    }
    rows
}

/// Combines a URL's query with rows that were stored beside it.
///
/// Used when older data is normalized: a URL written by an earlier version can
/// carry a query while the request also has rows of its own. The parsed pairs
/// are kept, a row that repeats one of them is dropped, and every other row
/// keeps its position. Unlike [`merge_query_rows`], this never discards an
/// enabled row just because the URL does not show it, so replaying an old entry
/// still sends what it sent.
pub fn union_query_rows(parsed: Vec<KeyValue>, current: &[KeyValue]) -> Vec<KeyValue> {
    let mut used = vec![false; parsed.len()];
    let mut anchors: Vec<Vec<KeyValue>> = vec![Vec::new(); parsed.len() + 1];
    let mut represented = 0;
    for row in current {
        let duplicate = if row.enabled && !row.key.is_empty() {
            parsed.iter().enumerate().position(|(index, pair)| {
                !used[index] && pair.key == row.key && pair.value == row.value
            })
        } else {
            None
        };

        match duplicate {
            Some(index) => {
                used[index] = true;
                represented += 1;
            }
            None => anchors[represented.min(parsed.len())].push(row.clone()),
        }
    }

    let mut rows = Vec::with_capacity(parsed.len() + current.len());
    for (index, mut anchored) in anchors.into_iter().enumerate() {
        rows.append(&mut anchored);
        if let Some(row) = parsed.get(index) {
            rows.push(row.clone());
        }
    }
    rows
}

/// Combines a URL's query with rows stored beside it, leading with the URL's.
///
/// Used when a document written by an earlier version is imported: the URL was
/// the request's own text, so its parameters keep the order the document sent
/// them in, and the stored rows follow. A stored row that repeats a parsed pair
/// is dropped rather than sent twice.
pub fn lead_union_query_rows(parsed: Vec<KeyValue>, current: &[KeyValue]) -> Vec<KeyValue> {
    let mut used = vec![false; parsed.len()];
    let mut rows = parsed.clone();
    for row in current {
        let duplicate = if row.enabled && !row.key.is_empty() {
            parsed.iter().enumerate().position(|(index, pair)| {
                !used[index] && pair.key == row.key && pair.value == row.value
            })
        } else {
            None
        };
        match duplicate {
            Some(index) => used[index] = true,
            None => rows.push(row.clone()),
        }
    }
    rows
}

fn parse_query(query: &str) -> Vec<KeyValue> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = decode_component(key);
            if key.is_empty() {
                return None;
            }
            Some(KeyValue::new(key, decode_component(value)))
        })
        .collect()
}

fn encode_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '%' | '&' | '#' | '+' | '=' => {
                let mut bytes = [0u8; 4];
                for byte in ch.encode_utf8(&mut bytes).bytes() {
                    push_encoded(&mut encoded, byte);
                }
            }
            ch if ch.is_whitespace() || ch.is_control() => {
                let mut bytes = [0u8; 4];
                for byte in ch.encode_utf8(&mut bytes).bytes() {
                    push_encoded(&mut encoded, byte);
                }
            }
            ch => encoded.push(ch),
        }
    }
    encoded
}

fn push_encoded(encoded: &mut String, byte: u8) {
    encoded.push('%');
    encoded.push(HEX[usize::from(byte >> 4)] as char);
    encoded.push(HEX[usize::from(byte & 0xf)] as char);
}

fn decode_component(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        decoded.push(high << 4 | low);
                        index += 3;
                    }
                    _ => {
                        decoded.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, value: &str) -> KeyValue {
        KeyValue::new(key, value)
    }

    fn disabled(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: value.to_owned(),
            enabled: false,
        }
    }

    #[test]
    fn a_url_without_a_query_has_no_rows() {
        let (base, rows) = split_url_query("https://api.example.com/items");
        assert_eq!(base, "https://api.example.com/items");
        assert!(rows.is_empty());
    }

    #[test]
    fn a_fragment_is_part_of_the_base() {
        let (base, rows) = split_url_query("https://api.example.com/items#top");
        assert_eq!(base, "https://api.example.com/items#top");
        assert!(rows.is_empty());
    }

    #[test]
    fn joining_without_rows_adds_no_separator() {
        assert_eq!(
            join_url_query("https://api.example.com/items", &[]),
            "https://api.example.com/items"
        );
        assert_eq!(
            join_url_query("https://api.example.com/items", &[disabled("a", "1")]),
            "https://api.example.com/items"
        );
    }

    #[test]
    fn the_query_goes_before_the_fragment() {
        let rows = [row("b", "2")];
        let joined = join_url_query("https://api.example.com/items?a=1#top", &rows);
        assert_eq!(joined, "https://api.example.com/items?b=2#top");
    }

    #[test]
    fn duplicate_keys_keep_their_order() {
        let rows = [row("tag", "a"), row("tag", "b")];
        let joined = join_url_query("https://api.example.com/items", &rows);
        assert_eq!(joined, "https://api.example.com/items?tag=a&tag=b");

        let (base, parsed) = split_url_query(&joined);
        assert_eq!(base, "https://api.example.com/items");
        assert_eq!(parsed, rows);
    }

    #[test]
    fn a_bare_key_becomes_an_empty_value() {
        let (_, rows) = split_url_query("https://api.example.com/items?flag");
        assert_eq!(rows, [row("flag", "")]);
        assert_eq!(
            join_url_query("https://api.example.com/items", &rows),
            "https://api.example.com/items?flag="
        );
    }

    #[test]
    fn a_value_with_spaces_round_trips() {
        let rows = [row("term", "a b")];
        let joined = join_url_query("https://api.example.com/items", &rows);
        assert_eq!(joined, "https://api.example.com/items?term=a%20b");
        let (_, parsed) = split_url_query(&joined);
        assert_eq!(parsed, rows);
    }

    #[test]
    fn a_placeholder_is_shown_verbatim() {
        let rows = [row("term", "{{term}}")];
        let joined = join_url_query("https://api.example.com/items", &rows);
        assert_eq!(joined, "https://api.example.com/items?term={{term}}");
        let (_, parsed) = split_url_query(&joined);
        assert_eq!(parsed, rows);
    }

    #[test]
    fn reserved_characters_inside_a_value_are_encoded() {
        let rows = [row("q", "a&b=c+d#e%f")];
        let joined = join_url_query("https://api.example.com/items", &rows);
        assert_eq!(
            joined,
            "https://api.example.com/items?q=a%26b%3Dc%2Bd%23e%25f"
        );
        let (_, parsed) = split_url_query(&joined);
        assert_eq!(parsed, rows);
    }

    #[test]
    fn a_plus_decodes_to_a_space() {
        let (_, rows) = split_url_query("https://api.example.com/items?q=a+b");
        assert_eq!(rows, [row("q", "a b")]);
    }

    #[test]
    fn a_malformed_escape_is_left_alone() {
        let (_, rows) = split_url_query("https://api.example.com/items?q=100%");
        assert_eq!(rows, [row("q", "100%")]);
    }

    #[test]
    fn an_unparseable_base_still_joins_textually() {
        let rows = [row("page", "2")];
        let joined = join_url_query("not a url", &rows);
        assert_eq!(joined, "not a url?page=2");
        let (base, parsed) = split_url_query(&joined);
        assert_eq!(base, "not a url");
        assert_eq!(parsed, rows);
    }

    #[test]
    fn rows_without_a_key_stay_out_of_the_url() {
        let rows = [row("", "x"), row("a", "1")];
        assert_eq!(
            join_url_query("https://api.example.com/items", &rows),
            "https://api.example.com/items?a=1"
        );
        let (_, parsed) = split_url_query("https://api.example.com/items?=x&a=1");
        assert_eq!(parsed, [row("a", "1")]);
    }

    #[test]
    fn a_disabled_row_keeps_its_place() {
        let current = [row("a", "1"), disabled("debug", "verbose"), row("b", "2")];
        let parsed = vec![row("a", "1"), row("b", "2")];
        assert_eq!(merge_query_rows(parsed, &current), current);
    }

    #[test]
    fn a_row_without_a_key_keeps_its_place() {
        let current = [row("a", "1"), row("", "draft"), row("b", "2")];
        let parsed = vec![row("a", "1"), row("b", "2")];
        assert_eq!(merge_query_rows(parsed, &current), current);
    }

    #[test]
    fn an_anchored_row_trails_the_new_rows() {
        let current = [row("a", "1"), disabled("debug", "verbose")];
        let parsed = vec![row("a", "1"), row("b", "2")];
        assert_eq!(
            merge_query_rows(parsed, &current),
            [row("a", "1"), row("b", "2"), disabled("debug", "verbose")]
        );
    }

    #[test]
    fn a_row_before_every_represented_row_stays_at_the_front() {
        let current = [disabled("debug", "verbose")];
        let parsed = vec![row("a", "1")];
        assert_eq!(
            merge_query_rows(parsed, &current),
            [disabled("debug", "verbose"), row("a", "1")]
        );
    }

    #[test]
    fn an_anchored_row_can_land_in_the_middle() {
        let current = [row("a", "1"), disabled("debug", "verbose"), row("c", "3")];
        let parsed = vec![row("a", "1"), row("b", "2"), row("c", "3")];
        assert_eq!(
            merge_query_rows(parsed, &current),
            [
                row("a", "1"),
                disabled("debug", "verbose"),
                row("b", "2"),
                row("c", "3")
            ]
        );
    }

    #[test]
    fn an_anchor_is_clamped_to_the_parsed_length() {
        let current = [
            row("a", "1"),
            row("b", "2"),
            row("c", "3"),
            disabled("debug", "verbose"),
        ];
        let parsed = vec![row("a", "1")];
        assert_eq!(
            merge_query_rows(parsed, &current),
            [row("a", "1"), disabled("debug", "verbose")]
        );
    }

    #[test]
    fn a_legacy_row_the_url_does_not_show_survives_normalization() {
        let current = [row("b", "2")];
        let parsed = vec![row("a", "1")];
        assert_eq!(
            union_query_rows(parsed, &current),
            [row("b", "2"), row("a", "1")]
        );
    }

    #[test]
    fn a_row_the_url_repeats_is_not_duplicated() {
        let current = [row("a", "1"), row("b", "2")];
        let parsed = vec![row("a", "1"), row("b", "2")];
        assert_eq!(union_query_rows(parsed, &current), current);

        let parsed = vec![row("a", "1")];
        let current = [row("a", "1")];
        assert_eq!(union_query_rows(parsed, &current), [row("a", "1")]);
    }

    #[test]
    fn normalization_keeps_matching_pair_consumption_in_order() {
        let current = [row("tag", "a"), row("tag", "b")];
        let parsed = vec![row("tag", "a"), row("tag", "b")];

        assert_eq!(union_query_rows(parsed, &current), current);
    }

    #[test]
    fn normalization_keeps_an_unrepresentable_row_in_place() {
        let current = [row("a", "1"), disabled("debug", "verbose"), row("c", "3")];
        let parsed = vec![row("a", "1"), row("c", "3")];

        assert_eq!(union_query_rows(parsed, &current), current);
    }

    #[test]
    fn an_imported_url_leads_its_stored_rows() {
        let current = [row("limit", "10")];
        let parsed = vec![row("page", "2")];
        assert_eq!(
            lead_union_query_rows(parsed, &current),
            [row("page", "2"), row("limit", "10")]
        );

        let current = [row("page", "2"), disabled("debug", "verbose")];
        let parsed = vec![row("page", "2")];
        assert_eq!(
            lead_union_query_rows(parsed, &current),
            [row("page", "2"), disabled("debug", "verbose")],
            "a stored repeat of a URL pair is not sent twice"
        );
    }

    #[test]
    fn joining_replaces_a_base_that_still_carries_a_query() {
        let rows = [row("b", "2")];
        assert_eq!(
            join_url_query("https://api.example.com/items?a=1", &rows),
            "https://api.example.com/items?b=2"
        );
    }
}
