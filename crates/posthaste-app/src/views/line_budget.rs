//! Test-only guard: production functions in `src/views` stay under 200 lines.
//!
//! The measurement is the line span of the item, from the `fn` line through the
//! closing brace. Test modules are ignored, so a large test body never trips
//! the guard.

#[cfg(test)]
mod guard {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// The budget for one production function, in lines.
    const MAX_LINES: usize = 200;

    /// Functions that were already over budget when this guard was added.
    ///
    /// Every entry predates the decompose-view-models change and lives outside
    /// its scope; the response viewer and the request editor were split instead
    /// of listed here. The guard fails when an entry stops naming an oversize
    /// function, so the list cannot rot silently.
    const PRE_EXISTING_OVERSIZE: &[(&str, &str)] = &[
        // The shell's root render mounts every pane.
        ("mod.rs", "render"),
        // The sidebar's render and its three row builders.
        ("sidebar.rs", "render"),
        ("sidebar.rs", "variables_popover"),
        ("sidebar.rs", "collection_row"),
        ("sidebar.rs", "folder_row"),
        ("sidebar.rs", "request_row"),
        // The text area's prepaint.
        ("text_area.rs", "prepaint"),
        // One WebSocket frame row.
        ("websocket_panel.rs", "frame_row"),
    ];

    #[test]
    fn production_functions_in_views_stay_under_the_line_budget() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("views");
        let test_only = test_only_module_files(&root);
        let mut files = Vec::new();
        collect_rust_files(&root, &mut files);
        files.sort();

        let mut oversize = Vec::new();
        for path in files {
            if test_only.contains(&path) {
                continue;
            }
            let relative = path
                .strip_prefix(&root)
                .expect("the file lives under src/views")
                .to_string_lossy()
                .replace('\\', "/");
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            let code = blank_comments_and_literals(&source);
            let test_modules = test_module_spans(&code);
            for function in functions(&code) {
                if function.length <= MAX_LINES
                    || test_modules
                        .iter()
                        .any(|(start, end)| function.start >= *start && function.start <= *end)
                {
                    continue;
                }
                oversize.push((
                    relative.clone(),
                    function.name.to_owned(),
                    function.line,
                    function.length,
                ));
            }
        }

        let mut unexpected = Vec::new();
        let mut allowed_seen = BTreeSet::new();
        for (file, name, line, length) in &oversize {
            if PRE_EXISTING_OVERSIZE.contains(&(file.as_str(), name.as_str())) {
                allowed_seen.insert(format!("{file}:{name}"));
            } else {
                unexpected.push(format!("  {file}:{line}: `{name}` is {length} lines"));
            }
        }
        assert!(
            unexpected.is_empty(),
            "production functions in src/views exceed {MAX_LINES} lines; split them:\n{}",
            unexpected.join("\n")
        );

        let stale: Vec<String> = PRE_EXISTING_OVERSIZE
            .iter()
            .filter(|(file, name)| !allowed_seen.contains(&format!("{file}:{name}")))
            .map(|(file, name)| format!("  {file}: `{name}`"))
            .collect();
        assert!(
            stale.is_empty(),
            "these allowlisted functions are no longer over {MAX_LINES} lines; remove their entries:\n{}",
            stale.join("\n")
        );
    }

    /// One function definition found in the blanked source.
    struct Function<'a> {
        name: &'a str,
        /// Byte offset of the `fn` keyword in the blanked source.
        start: usize,
        /// One-based line of the `fn` keyword.
        line: usize,
        /// Lines from the signature through the closing brace.
        length: usize,
    }

    /// Finds every function with a body and measures it.
    fn functions(code: &str) -> Vec<Function<'_>> {
        let bytes = code.as_bytes();
        let mut found = Vec::new();
        let mut i = 0;
        while i + 2 <= bytes.len() {
            if &bytes[i..i + 2] != b"fn" {
                i += 1;
                continue;
            }
            let before = i.checked_sub(1).map(|k| bytes[k]);
            let after = bytes.get(i + 2).copied();
            if before.is_some_and(is_ident_byte)
                || !after.is_some_and(|byte| byte.is_ascii_whitespace())
            {
                i += 1;
                continue;
            }
            let mut name_start = i + 2;
            while name_start < bytes.len() && bytes[name_start].is_ascii_whitespace() {
                name_start += 1;
            }
            let mut name_end = name_start;
            while name_end < bytes.len() && is_ident_byte(bytes[name_end]) {
                name_end += 1;
            }
            if name_end == name_start {
                i += 1;
                continue;
            }
            if let Some(end) = body_end(code, name_end) {
                let start_line = line_of(code, i);
                found.push(Function {
                    name: &code[name_start..name_end],
                    start: i,
                    line: start_line,
                    length: line_of(code, end) - start_line + 1,
                });
            }
            i = name_end;
        }
        found
    }

    fn is_ident_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }

    /// The offset of a function body's closing brace, or `None` for a
    /// declaration without a body.
    fn body_end(code: &str, from: usize) -> Option<usize> {
        let bytes = code.as_bytes();
        let mut i = from;
        // Array lengths in parameter types contain a semicolon inside `[...]`,
        // so only a top-level `;` ends a declaration.
        let mut brackets = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                b'(' | b'[' => brackets += 1,
                b')' | b']' => brackets = brackets.saturating_sub(1),
                b';' if brackets == 0 => return None,
                b'{' if brackets == 0 => return closing_brace(code, i),
                _ => {}
            }
            i += 1;
        }
        None
    }

    fn closing_brace(code: &str, open: usize) -> Option<usize> {
        let bytes = code.as_bytes();
        let mut depth = 0usize;
        let mut i = open;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    fn line_of(code: &str, offset: usize) -> usize {
        code.as_bytes()[..offset]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            + 1
    }

    /// Byte spans of inline `#[cfg(test)] mod ... { ... }` bodies.
    fn test_module_spans(code: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        let mut offset = 0;
        for line in code.split_inclusive('\n') {
            if line.trim() == "#[cfg(test)]"
                && let Some(span) = test_module_after(code, offset + line.len())
            {
                spans.push(span);
            }
            offset += line.len();
        }
        spans
    }

    /// Looks past the attribute line for an inline module and returns the span
    /// from its `mod` line through the closing brace.
    fn test_module_after(code: &str, from: usize) -> Option<(usize, usize)> {
        let mut line_start = from;
        for line in code[from..].split_inclusive('\n') {
            let trimmed = line.trim();
            // Other attributes and doc comments may sit between the cfg and the
            // item; they do not change what the item is.
            if trimmed.is_empty() || trimmed.starts_with("#[") || trimmed.starts_with("//") {
                line_start += line.len();
                continue;
            }
            if !declares_module(trimmed) {
                return None;
            }
            let brace = line.find('{')?;
            let close = closing_brace(code, line_start + brace)?;
            return Some((line_start, close));
        }
        None
    }

    fn declares_module(line: &str) -> bool {
        let rest = strip_visibility(line);
        rest.strip_prefix("mod ")
            .is_some_and(|tail| tail.chars().next().is_some_and(is_ident_char))
    }

    fn strip_visibility(line: &str) -> &str {
        let Some(rest) = line.strip_prefix("pub") else {
            return line;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('(') {
            return rest;
        }
        match rest.find(')') {
            Some(close) => rest[close + 1..].trim_start(),
            None => rest,
        }
    }

    /// Files that exist only as the body of a `#[cfg(test)] mod name;`
    /// declaration.
    fn test_only_module_files(root: &Path) -> BTreeSet<PathBuf> {
        let mut files = Vec::new();
        collect_rust_files(root, &mut files);
        let mut excluded = BTreeSet::new();
        for path in files {
            let Ok(source) = fs::read_to_string(&path) else {
                continue;
            };
            let dir = module_dir(&path);
            let mut pending = false;
            for line in source.lines() {
                let trimmed = line.trim();
                if trimmed == "#[cfg(test)]" {
                    pending = true;
                    continue;
                }
                if !pending {
                    continue;
                }
                if trimmed.is_empty() || trimmed.starts_with("#[") || trimmed.starts_with("//") {
                    continue;
                }
                pending = false;
                if let Some(name) = test_only_declaration(trimmed) {
                    excluded.insert(dir.join(format!("{name}.rs")));
                    excluded.insert(dir.join(name).join("mod.rs"));
                }
            }
        }
        excluded
    }

    /// The module name of a `mod name;` declaration, which names a whole file.
    fn test_only_declaration(line: &str) -> Option<&str> {
        let rest = strip_visibility(line);
        let tail = rest.strip_prefix("mod ")?;
        let name = tail.strip_suffix(';')?.trim();
        (!name.is_empty() && name.chars().all(is_ident_char)).then_some(name)
    }

    /// Where a file's child modules live.
    fn module_dir(path: &Path) -> PathBuf {
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        match path.file_name().and_then(|name| name.to_str()) {
            Some("mod.rs") => parent.to_path_buf(),
            Some(name) => parent.join(name.trim_end_matches(".rs")),
            None => parent.to_path_buf(),
        }
    }

    fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rust_files(&path, out);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                out.push(path);
            }
        }
    }

    fn is_ident_char(character: char) -> bool {
        character.is_alphanumeric() || character == '_'
    }

    /// Replaces comments, string literals, char literals, and raw strings with
    /// spaces, keeping newlines so offsets still map to source lines. Braces
    /// and semicolons in literals therefore cannot confuse the scanner, and
    /// rustfmt's line breaks cannot hide a function.
    fn blank_comments_and_literals(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            match c {
                '/' if next == Some('/') => {
                    while i < chars.len() && chars[i] != '\n' {
                        out.push(' ');
                        i += 1;
                    }
                    if i < chars.len() {
                        out.push('\n');
                        i += 1;
                    }
                }
                '/' if next == Some('*') => {
                    i = blank_block_comment(&chars, i, &mut out);
                }
                '"' => {
                    i = blank_string(&chars, i, &mut out);
                }
                '\'' => match char_literal_end(&chars, i) {
                    Some(end) => {
                        for character in &chars[i..=end] {
                            out.push(if *character == '\n' { '\n' } else { ' ' });
                        }
                        i = end + 1;
                    }
                    None => {
                        // A lifetime, not a char literal.
                        out.push(c);
                        i += 1;
                    }
                },
                'r' => match raw_string_start(&chars, i) {
                    Some(hashes) => {
                        i = blank_raw_string(&chars, i, hashes, &mut out);
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                },
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    fn blank_block_comment(chars: &[char], start: usize, out: &mut String) -> usize {
        let mut depth = 0usize;
        let mut i = start;
        while i < chars.len() {
            let next = chars.get(i + 1).copied();
            if chars[i] == '/' && next == Some('*') {
                depth += 1;
                out.push_str("  ");
                i += 2;
            } else if chars[i] == '*' && next == Some('/') {
                depth -= 1;
                out.push_str("  ");
                i += 2;
                if depth == 0 {
                    break;
                }
            } else {
                out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                i += 1;
            }
        }
        i
    }

    fn blank_string(chars: &[char], start: usize, out: &mut String) -> usize {
        out.push(' ');
        let mut i = start + 1;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() {
                out.push(' ');
                out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                i += 2;
            } else if c == '"' {
                out.push(' ');
                return i + 1;
            } else {
                out.push(if c == '\n' { '\n' } else { ' ' });
                i += 1;
            }
        }
        i
    }

    /// The index of a char literal's closing quote, or `None` for a lifetime.
    fn char_literal_end(chars: &[char], start: usize) -> Option<usize> {
        match chars.get(start + 1)? {
            '\\' => {
                // The escaped character cannot itself terminate the literal, so
                // the next quote after it is the terminator.
                let mut i = start + 3;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        return Some(i);
                    }
                    if chars[i] == '\n' {
                        return None;
                    }
                    i += 1;
                }
                None
            }
            character if *character != '\'' && chars.get(start + 2) == Some(&'\'') => {
                Some(start + 2)
            }
            _ => None,
        }
    }

    /// The hash count of an `r"..."` / `r#"..."#` prefix, if one starts here.
    fn raw_string_start(chars: &[char], i: usize) -> Option<usize> {
        if let Some(previous) = i.checked_sub(1).map(|k| chars[k]) {
            // `br"..."` and `cr"..."` are raw too, but an identifier that merely
            // ends in `r` is not.
            if previous != 'b' && previous != 'c' && is_ident_char(previous) {
                return None;
            }
        }
        let mut j = i + 1;
        let mut hashes = 0;
        while chars.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
        (chars.get(j) == Some(&'"')).then_some(hashes)
    }

    fn blank_raw_string(chars: &[char], start: usize, hashes: usize, out: &mut String) -> usize {
        out.push(' ');
        let mut i = start + 1;
        for _ in 0..hashes {
            out.push(' ');
            i += 1;
        }
        out.push(' ');
        i += 1;
        while i < chars.len() {
            if chars[i] == '"' && (0..hashes).all(|offset| chars.get(i + 1 + offset) == Some(&'#'))
            {
                out.push(' ');
                for _ in 0..hashes {
                    out.push(' ');
                }
                return i + 1 + hashes;
            }
            out.push(if chars[i] == '\n' { '\n' } else { ' ' });
            i += 1;
        }
        i
    }
}
