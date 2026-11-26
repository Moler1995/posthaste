//! Binary response classification and suggested file names.
//!
//! [`classify`] answers one question — "is this body binary?" — for the
//! completed response, a streaming prefix, and a stored history entry, so the
//! Body section, the executor, and history all agree. It is pure: no I/O, no
//! decoding, and the same headers always give the same answer.

use percent_encoding::percent_decode_str;

/// The largest prefix [`classify`] sniffs for an unlabeled body.
pub const SNIFF_LIMIT: usize = 8 * 1024;

/// A format identified from a body's leading bytes when the response declared
/// no useful media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectedFormat {
    /// A technical display name such as `PNG image`, not a localized sentence.
    pub label: &'static str,
    /// The file extension without the dot.
    pub extension: &'static str,
}

/// What the viewer needs to present and save a binary body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryMeta {
    /// The response `Content-Type` media type without parameters, when the
    /// response carried one.
    pub media_type: Option<String>,
    /// The suggested save name: from `Content-Disposition` when it has one,
    /// otherwise `response.<ext>` or `response.bin`.
    pub file_name: Option<String>,
    /// Whether the bytes are still available to save. History keeps metadata
    /// only, so an entry loaded from history reports false.
    pub body_stored: bool,
    /// The format identified from the bytes when the declared type was absent
    /// or `application/octet-stream`. `None` for a declared type and for
    /// history, which carries no bytes.
    pub detected: Option<DetectedFormat>,
}

/// Returns the binary metadata for a response, or `None` when it is text.
///
/// `prefix` is the start of the body (at most [`SNIFF_LIMIT`] bytes are
/// inspected) and `body_len` is the retained body size. An empty body is never
/// binary, and an explicit textual media type keeps a body textual even when
/// its bytes are not valid UTF-8.
pub fn classify(
    headers: &[(String, String)],
    prefix: &[u8],
    body_len: usize,
) -> Option<BinaryMeta> {
    if body_len == 0 {
        return None;
    }

    let media_type = content_type(headers);
    match header_disposition(headers) {
        Some(false) => return None,
        Some(true) => {}
        // Absent or unrecognized: only the bytes can decide.
        None => {
            if !looks_binary(prefix) {
                return None;
            }
        }
    }

    let detected = identify(media_type.as_deref(), prefix);
    Some(BinaryMeta {
        file_name: Some(suggested_name(headers, media_type.as_deref(), detected)),
        media_type,
        body_stored: true,
        detected,
    })
}

/// How `Content-Type` alone classifies a body: `Some(true)` for a binary
/// media type, `Some(false)` for a textual one, and `None` when only the bytes
/// can decide. The streaming presentation uses this before any body arrives.
pub fn header_disposition(headers: &[(String, String)]) -> Option<bool> {
    let media = content_type(headers)?;
    if is_textual(&media) {
        Some(false)
    } else if is_binary(&media) {
        Some(true)
    } else {
        None
    }
}

/// The binary metadata the headers alone establish, before the body arrives:
/// the media type and suggested name for a binary `Content-Type`. Returns
/// `None` for a textual or unrecognized type.
pub fn declared_binary(headers: &[(String, String)]) -> Option<BinaryMeta> {
    if header_disposition(headers) != Some(true) {
        return None;
    }
    let media_type = content_type(headers);
    Some(BinaryMeta {
        file_name: Some(suggested_name(headers, media_type.as_deref(), None)),
        media_type,
        body_stored: true,
        detected: None,
    })
}

/// The file name a `Content-Disposition` header declares, sanitized.
///
/// Binary responses get a name from [`suggested_name`] even without the header;
/// this is for textual responses, which otherwise have no file to save and are
/// offered one only when the server names it.
pub fn attachment_name(headers: &[(String, String)]) -> Option<String> {
    disposition_name(headers).and_then(|name| sanitize(&name))
}

/// The first `Content-Type` value's media type, lowercased and trimmed.
fn content_type(headers: &[(String, String)]) -> Option<String> {
    let value = header(headers, "content-type")?;
    let media = value.split(';').next()?.trim();
    (!media.is_empty()).then(|| media.to_ascii_lowercase())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The textual types the lossy text path owns, even for invalid UTF-8 bytes.
fn is_textual(media: &str) -> bool {
    media.starts_with("text/")
        || media == "application/json"
        || media.ends_with("+json")
        || media == "application/xml"
        || media.ends_with("+xml")
        || matches!(
            media,
            "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/x-ecmascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/graphql"
                | "application/x-www-form-urlencoded"
        )
}

/// The binary types an explicit `Content-Type` marks without sniffing.
fn is_binary(media: &str) -> bool {
    media.starts_with("image/")
        || media.starts_with("audio/")
        || media.starts_with("video/")
        || media.starts_with("font/")
        || media.starts_with("application/vnd.")
        || matches!(
            media,
            "application/octet-stream"
                | "application/pdf"
                | "application/zip"
                | "application/gzip"
                | "application/x-gzip"
                | "application/x-tar"
                | "application/x-7z-compressed"
                | "application/x-rar-compressed"
                | "application/vnd.rar"
                | "application/x-bzip2"
                | "application/x-zstd"
                | "application/zstd"
                | "application/x-xz"
                | "application/x-compress"
                | "application/wasm"
                | "application/msword"
        )
}

/// Whether the sniffed prefix is binary: it holds a NUL byte, or it is not
/// valid UTF-8. An incomplete sequence that ends exactly at the sniff boundary
/// is not evidence either way and is ignored.
fn looks_binary(prefix: &[u8]) -> bool {
    let sniff = &prefix[..prefix.len().min(SNIFF_LIMIT)];
    if sniff.contains(&0) {
        return true;
    }
    match std::str::from_utf8(sniff) {
        Ok(_) => false,
        Err(error) => error.error_len().is_some(),
    }
}

/// Identifies a format from the bytes only when the declared type says
/// nothing useful. A declared type is authoritative, even if the bytes
/// disagree with it.
fn identify(media_type: Option<&str>, prefix: &[u8]) -> Option<DetectedFormat> {
    match media_type {
        None | Some("application/octet-stream") => sniff_format(prefix),
        Some(_) => None,
    }
}

/// Recognizes common file signatures in the leading bytes.
pub fn sniff_format(prefix: &[u8]) -> Option<DetectedFormat> {
    const FORMATS: &[(&[u8], DetectedFormat)] = &[
        (
            b"\x89PNG\r\n\x1a\n",
            DetectedFormat {
                label: "PNG image",
                extension: "png",
            },
        ),
        (
            b"\xff\xd8\xff",
            DetectedFormat {
                label: "JPEG image",
                extension: "jpg",
            },
        ),
        (
            b"GIF87a",
            DetectedFormat {
                label: "GIF image",
                extension: "gif",
            },
        ),
        (
            b"GIF89a",
            DetectedFormat {
                label: "GIF image",
                extension: "gif",
            },
        ),
        (
            b"%PDF-",
            DetectedFormat {
                label: "PDF document",
                extension: "pdf",
            },
        ),
        (
            b"PK\x03\x04",
            DetectedFormat {
                label: "ZIP archive",
                extension: "zip",
            },
        ),
        (
            b"PK\x05\x06",
            DetectedFormat {
                label: "ZIP archive",
                extension: "zip",
            },
        ),
        (
            b"PK\x07\x08",
            DetectedFormat {
                label: "ZIP archive",
                extension: "zip",
            },
        ),
        (
            b"\x1f\x8b",
            DetectedFormat {
                label: "Gzip archive",
                extension: "gz",
            },
        ),
        (
            b"\x28\xb5\x2f\xfd",
            DetectedFormat {
                label: "Zstandard archive",
                extension: "zst",
            },
        ),
        (
            b"7z\xbc\xaf\x27\x1c",
            DetectedFormat {
                label: "7z archive",
                extension: "7z",
            },
        ),
        (
            b"Rar!\x1a\x07",
            DetectedFormat {
                label: "RAR archive",
                extension: "rar",
            },
        ),
    ];
    FORMATS
        .iter()
        .find(|(signature, _)| prefix.starts_with(signature))
        .map(|(_, format)| *format)
        .or_else(|| {
            // RIFF containers carry their type at offset 8.
            (prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP")
                .then_some(DetectedFormat {
                    label: "WebP image",
                    extension: "webp",
                })
        })
}

/// The name the Save dialog suggests.
fn suggested_name(
    headers: &[(String, String)],
    media_type: Option<&str>,
    detected: Option<DetectedFormat>,
) -> String {
    if let Some(name) = attachment_name(headers) {
        return name;
    }
    // A detected format is more specific than the generic type it was sniffed
    // from, so its extension wins for `application/octet-stream`.
    let extension = match detected {
        Some(detected) => Some(detected.extension),
        None => media_type.and_then(extension_for),
    };
    match extension {
        Some(extension) => format!("response.{extension}"),
        None => "response.bin".to_owned(),
    }
}

/// The name carried by `Content-Disposition`, with the RFC 5987 extended form
/// preferred over the plain one.
fn disposition_name(headers: &[(String, String)]) -> Option<String> {
    let value = header(headers, "content-disposition")?;
    let mut plain = None;
    for parameter in value.split(';').skip(1) {
        let parameter = parameter.trim();
        if let Some(encoded) = strip_prefix_ignore_case(parameter, "filename*=") {
            if let Some(name) = extended_name(encoded) {
                return Some(name);
            }
            continue;
        }
        if let Some(quoted) = strip_prefix_ignore_case(parameter, "filename=") {
            plain.get_or_insert_with(|| unquote(quoted));
        }
    }
    plain
}

/// Decodes `filename*`: `charset'language'percent-encoded-value`.
fn extended_name(value: &str) -> Option<String> {
    let mut parts = value.trim().splitn(3, '\'');
    let _charset = parts.next()?;
    let _language = parts.next()?;
    let encoded = parts.next()?;
    Some(percent_decode_str(encoded).decode_utf8_lossy().into_owned())
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

/// Removes the surrounding quotes and their backslash escapes.
fn unquote(value: &str) -> String {
    let value = value.trim();
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(value);
    let mut unescaped = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                unescaped.push(next);
            }
        } else {
            unescaped.push(ch);
        }
    }
    unescaped
}

/// Replaces characters Windows refuses in a file name and trailing dots or
/// spaces. Returns `None` when nothing usable remains.
fn sanitize(name: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']);
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then(|| cleaned.to_owned())
}

/// The first known extension for a media type.
fn extension_for(media_type: &str) -> Option<&'static str> {
    mime_guess::get_mime_extensions_str(media_type)?
        .first()
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn png() -> Vec<(String, String)> {
        headers(&[("Content-Type", "image/png")])
    }

    #[test]
    fn an_image_is_binary_and_suggests_its_extension() {
        let meta = classify(&png(), &[0x89, b'P', b'N', b'G'], 4).expect("png is binary");
        assert_eq!(meta.media_type.as_deref(), Some("image/png"));
        assert_eq!(meta.file_name.as_deref(), Some("response.png"));
        assert!(meta.body_stored);
    }

    #[test]
    fn an_explicit_binary_type_wins_over_textual_bytes() {
        let meta = classify(
            &headers(&[("content-type", "application/pdf; charset=binary")]),
            b"%PDF but utf-8",
            14,
        )
        .expect("pdf is binary");
        assert_eq!(meta.media_type.as_deref(), Some("application/pdf"));
        assert_eq!(meta.file_name.as_deref(), Some("response.pdf"));
    }

    #[test]
    fn an_unlabeled_nul_body_is_binary_with_no_media_type() {
        let meta = classify(&[], b"abc\0def", 7).expect("a NUL byte is binary");
        assert_eq!(meta.media_type, None);
        assert_eq!(meta.file_name.as_deref(), Some("response.bin"));
    }

    #[test]
    fn unlabeled_utf8_is_text() {
        assert_eq!(classify(&[], "hello 你好".as_bytes(), 12), None);
    }

    #[test]
    fn a_textual_type_stays_text_even_for_invalid_utf8() {
        let csv = headers(&[("Content-Type", "text/csv")]);
        assert_eq!(classify(&csv, &[b'a', 0xff, b'b'], 3), None);

        let json = headers(&[("Content-Type", "application/vnd.api+json")]);
        assert_eq!(classify(&json, &[0xff], 1), None);
    }

    #[test]
    fn svg_is_text_even_though_it_is_an_image_type() {
        let svg = headers(&[("Content-Type", "image/svg+xml")]);
        assert_eq!(classify(&svg, b"<svg xmlns='x'/>", 15), None);
    }

    #[test]
    fn an_empty_body_is_never_binary() {
        assert_eq!(classify(&png(), b"", 0), None);
        assert_eq!(classify(&[], b"", 0), None);
    }

    #[test]
    fn an_incomplete_character_at_the_sniff_boundary_is_not_binary() {
        // "你好" is three bytes per character; the prefix stops after the
        // first two bytes of the second character.
        let full = "你好".as_bytes();
        assert_eq!(classify(&[], &full[..5], full.len()), None);
    }

    #[test]
    fn an_invalid_sequence_before_the_boundary_is_binary() {
        assert_eq!(
            classify(&[], &[b'a', 0xff, 0xfe, b'b'], 4)
                .expect("invalid UTF-8 is binary")
                .media_type,
            None
        );
    }

    #[test]
    fn sniffing_stops_at_the_limit() {
        // The invalid bytes sit past the sniff window, so the prefix reads as
        // text.
        let mut body = vec![b'x'; SNIFF_LIMIT];
        body.push(0xff);
        assert_eq!(classify(&[], &body, body.len()), None);
    }

    #[test]
    fn an_rfc5987_name_is_percent_decoded() {
        let headers = headers(&[
            ("Content-Type", "application/pdf"),
            (
                "Content-Disposition",
                "attachment; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf",
            ),
        ]);
        let meta = classify(&headers, b"%PDF", 4).expect("pdf is binary");
        assert_eq!(meta.file_name.as_deref(), Some("报告.pdf"));
    }

    #[test]
    fn a_quoted_name_is_used() {
        let headers = headers(&[
            ("Content-Type", "application/pdf"),
            ("Content-Disposition", "attachment; filename=\"report.pdf\""),
        ]);
        let meta = classify(&headers, b"%PDF", 4).expect("pdf is binary");
        assert_eq!(meta.file_name.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn a_textual_attachment_names_the_file_to_save() {
        let attachment = headers(&[
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Disposition", "attachment; filename=\"notes.txt\""),
        ]);
        assert_eq!(classify(&attachment, b"hello", 5), None, "text stays text");
        assert_eq!(attachment_name(&attachment).as_deref(), Some("notes.txt"));
        assert_eq!(
            attachment_name(&headers(&[("Content-Type", "text/plain")])),
            None,
            "a text response without a disposition names no file"
        );
    }

    #[test]
    fn path_separators_are_replaced_in_the_suggestion() {
        let headers = headers(&[
            ("Content-Type", "application/octet-stream"),
            (
                "Content-Disposition",
                "attachment; filename=\"../../evil.exe\"",
            ),
        ]);
        let meta = classify(&headers, &[0], 1).expect("octet-stream is binary");
        let name = meta.file_name.expect("a name is suggested");
        assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        assert!(name.ends_with("evil.exe"), "{name}");
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        let headers = headers(&[
            ("Content-Type", "application/octet-stream"),
            (
                "Content-Disposition",
                "attachment; filename=\"report.pdf. \"",
            ),
        ]);
        let meta = classify(&headers, &[0], 1).expect("octet-stream is binary");
        assert_eq!(meta.file_name.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn an_unknown_type_without_a_disposition_suggests_response_bin() {
        let headers = headers(&[("Content-Type", "application/x-posthaste-test")]);
        let meta = classify(&headers, &[0], 1).expect("a NUL byte is binary");
        assert_eq!(meta.file_name.as_deref(), Some("response.bin"));
    }

    #[test]
    fn headers_alone_mark_a_binary_or_textual_body() {
        assert_eq!(header_disposition(&png()), Some(true));
        assert_eq!(
            header_disposition(&headers(&[("Content-Type", "text/csv")])),
            Some(false)
        );
        assert_eq!(
            header_disposition(&headers(&[("Content-Type", "image/svg+xml")])),
            Some(false)
        );
        assert_eq!(
            header_disposition(&headers(&[(
                "Content-Type",
                "application/x-posthaste-test"
            )])),
            None
        );
        assert_eq!(header_disposition(&[]), None);
    }

    #[test]
    fn declared_binary_carries_the_name_before_the_body_arrives() {
        let meta = declared_binary(&headers(&[
            ("Content-Type", "image/png"),
            ("Content-Disposition", "attachment; filename=\"plot.png\""),
        ]))
        .expect("png is binary from the headers alone");
        assert_eq!(meta.media_type.as_deref(), Some("image/png"));
        assert_eq!(meta.file_name.as_deref(), Some("plot.png"));

        assert_eq!(
            declared_binary(&headers(&[("Content-Type", "text/csv")])),
            None
        );
        assert_eq!(declared_binary(&[]), None);
    }

    #[test]
    fn a_missing_disposition_still_suggests_the_extension() {
        let meta = classify(&png(), &[0], 1).expect("png is binary");
        assert_eq!(meta.file_name.as_deref(), Some("response.png"));
    }

    #[test]
    fn known_signatures_are_recognized() {
        let cases: &[(&[u8], &str, &str)] = &[
            (b"\x89PNG\r\n\x1a\n", "PNG image", "png"),
            (b"\xff\xd8\xff\xe0", "JPEG image", "jpg"),
            (b"GIF87a", "GIF image", "gif"),
            (b"GIF89a", "GIF image", "gif"),
            (b"%PDF-1.7", "PDF document", "pdf"),
            (b"PK\x03\x04", "ZIP archive", "zip"),
            (b"\x1f\x8b\x08", "Gzip archive", "gz"),
            (b"\x28\xb5\x2f\xfd", "Zstandard archive", "zst"),
            (b"7z\xbc\xaf\x27\x1c", "7z archive", "7z"),
            (b"Rar!\x1a\x07\x00", "RAR archive", "rar"),
            (b"RIFF\x24\x00\x00\x00WEBPVP8 ", "WebP image", "webp"),
        ];
        for (prefix, label, extension) in cases {
            let format = sniff_format(prefix).unwrap_or_else(|| {
                panic!("{label} signature was not recognized");
            });
            assert_eq!(format.label, *label);
            assert_eq!(format.extension, *extension);
        }
    }

    #[test]
    fn a_body_without_a_known_signature_has_no_detected_format() {
        assert_eq!(sniff_format(b"plain text"), None);
        assert_eq!(sniff_format(b""), None);
        // A RIFF container that is not WebP.
        assert_eq!(sniff_format(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
    }

    #[test]
    fn a_generic_type_is_identified_from_its_bytes() {
        let headers = headers(&[("Content-Type", "application/octet-stream")]);
        let body = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let meta = classify(&headers, body, body.len()).expect("octet-stream is binary");
        assert_eq!(
            meta.detected,
            Some(DetectedFormat {
                label: "PNG image",
                extension: "png",
            })
        );
        assert_eq!(meta.file_name.as_deref(), Some("response.png"));
    }

    #[test]
    fn a_disposition_name_still_wins_over_a_detected_extension() {
        let headers = headers(&[
            ("Content-Type", "application/octet-stream"),
            ("Content-Disposition", "attachment; filename=\"plot.png\""),
        ]);
        let meta = classify(&headers, b"\x89PNG\r\n\x1a\n", 8).expect("octet-stream is binary");
        assert_eq!(meta.file_name.as_deref(), Some("plot.png"));
    }

    #[test]
    fn a_declared_type_is_not_overridden_by_the_bytes() {
        // The headers say PNG; the bytes say GIF. The declared type wins and
        // no detection is reported.
        let headers = headers(&[("Content-Type", "image/png")]);
        let meta = classify(&headers, b"GIF89a", 6).expect("png is binary");
        assert_eq!(meta.detected, None);
        assert_eq!(meta.file_name.as_deref(), Some("response.png"));
    }

    #[test]
    fn an_unlabeled_body_is_identified_from_its_bytes() {
        let meta = classify(&[], b"%PDF-1.7\n\0", 10).expect("pdf bytes are binary");
        assert_eq!(meta.media_type, None);
        assert_eq!(
            meta.detected.map(|format| format.label),
            Some("PDF document")
        );
        assert_eq!(meta.file_name.as_deref(), Some("response.pdf"));
    }

    #[test]
    fn the_answer_does_not_change_between_a_prefix_and_the_whole_body() {
        let signature = [0x89, b'P', b'N', b'G'];
        for prefix_len in 1..=signature.len() {
            let short = classify(&png(), &signature[..prefix_len], 4);
            let whole = classify(&png(), &signature, 4);
            assert_eq!(short, whole, "prefix of {prefix_len} bytes");
        }
    }
}
