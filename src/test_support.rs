//! Test-only serialized runtimes and Rust source scanning for `include_str!` guards.
//! The lexer handles comments and literals without resolving names or evaluating `cfg`.

#[path = "test_support/input_owner/tests/strict_effect.rs"]
pub(crate) mod input_owner;
pub(crate) mod serial_runtime;

use std::fmt;
use std::ops::Range;

/// A comment or literal that never closes; the guard must fail, not match nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnterminatedSource {
    pub(crate) kind: &'static str,
    pub(crate) offset: usize,
}

impl fmt::Display for UnterminatedSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unterminated {} starting at byte {}",
            self.kind, self.offset
        )
    }
}

/// Source with every comment byte blanked to a space; literals, CR and LF stay.
pub(crate) fn strip_rust_comments(source: &str) -> Result<String, UnterminatedSource> {
    blank(source, false)
}

/// Code-only view: comments and string/char literals are blanked to spaces.
pub(crate) fn rust_code_mask(source: &str) -> Result<String, UnterminatedSource> {
    blank(source, true)
}

/// Byte range from the first `signature` in a code mask through its matching `}`.
/// `None` when the signature is absent, ends in `;` first, or never closes.
pub(crate) fn braced_item_span(code: &str, signature: &str) -> Option<Range<usize>> {
    let start = code.find(signature)?;
    let open = start + code[start..].find(['{', ';'])?;
    if code.as_bytes()[open] == b';' {
        return None;
    }
    let mut depth = 0usize;
    for (index, byte) in code.bytes().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start..index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// How a forgery test hides code: a `//` comment ending the line, or a nested block.
#[derive(Clone, Copy, Debug)]
pub(crate) enum CommentForgery {
    Line,
    NestedBlock,
}

impl CommentForgery {
    pub(crate) const ALL: [Self; 2] = [Self::Line, Self::NestedBlock];

    fn wrap(self, text: &str) -> String {
        match self {
            Self::Line => format!("// {text}\n"),
            Self::NestedBlock => format!("/* outer /* {text} */ */"),
        }
    }
}

/// Rewrites every non-comment occurrence of `needle` as a comment that still
/// contains it verbatim, so a raw `contains` keeps matching the forged source.
pub(crate) fn comment_out_code(source: &str, needle: &str, forgery: CommentForgery) -> String {
    let code = strip_rust_comments(source).expect("forgery input lexes");
    let starts: Vec<usize> = code.match_indices(needle).map(|(start, _)| start).collect();
    assert!(
        !starts.is_empty(),
        "no code occurrence of `{needle}` to forge"
    );
    let mut forged = source.to_string();
    for start in starts.into_iter().rev() {
        forged.replace_range(start..start + needle.len(), &forgery.wrap(needle));
    }
    let forged_code = strip_rust_comments(&forged).expect("forged source lexes");
    assert!(forged.contains(needle) && !forged_code.contains(needle));
    forged
}

/// Comment and literal byte ranges of a source, each list in source order; a literal
/// range covers its prefix, quotes and suffix.
#[derive(Debug, Default)]
pub(crate) struct RustSpans {
    pub(crate) comments: Vec<Range<usize>>,
    pub(crate) literals: Vec<Range<usize>>,
}

pub(crate) fn rust_spans(source: &str) -> Result<RustSpans, UnterminatedSource> {
    let bytes = source.as_bytes();
    let mut spans = RustSpans::default();
    let mut index = 0;
    while index < bytes.len() {
        let (end, comment) = match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => (line_comment_end(bytes, index), true),
            b'/' if bytes.get(index + 1) == Some(&b'*') => (block_comment_end(bytes, index)?, true),
            b'"' => (
                suffix_end(bytes, quoted_end(bytes, index, index + 1)?),
                false,
            ),
            b'\'' => match char_literal_end(bytes, index)? {
                Some(end) => (suffix_end(bytes, end), false),
                None => {
                    index += 1;
                    continue;
                }
            },
            _ if word_char_len(bytes, index).is_some() => {
                let word_end = word_end(bytes, index);
                match prefixed_literal_end(bytes, index, word_end)? {
                    Some(end) => (suffix_end(bytes, end), false),
                    None => {
                        index = word_end;
                        continue;
                    }
                }
            }
            lead => {
                index += utf8_len(lead);
                continue;
            }
        };
        if comment {
            spans.comments.push(index..end);
        } else {
            spans.literals.push(index..end);
        }
        index = end;
    }
    Ok(spans)
}

/// One byte per byte: blanking whole comments or literals keeps byte offsets,
/// line breaks and UTF-8 validity, so offsets in the result index `source`.
fn blank(source: &str, literals: bool) -> Result<String, UnterminatedSource> {
    let spans = rust_spans(source)?;
    let mut out = source.as_bytes().to_vec();
    let quoted = if literals { &spans.literals[..] } else { &[] };
    for range in spans.comments.iter().chain(quoted) {
        for byte in &mut out[range.clone()] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    Ok(String::from_utf8(out).expect("blanking whole UTF-8 sequences keeps UTF-8"))
}

fn unterminated<T>(kind: &'static str, offset: usize) -> Result<T, UnterminatedSource> {
    Err(UnterminatedSource { kind, offset })
}

/// Byte length of the identifier, keyword or number char at `index`. Non-ASCII
/// counts as identifier text except Rust's Pattern_White_Space characters.
fn word_char_len(bytes: &[u8], index: usize) -> Option<usize> {
    let lead = *bytes.get(index)?;
    if lead.is_ascii() {
        return (lead.is_ascii_alphanumeric() || lead == b'_').then_some(1);
    }
    let len = utf8_len(lead);
    let ch = std::str::from_utf8(bytes.get(index..index + len)?)
        .ok()?
        .chars()
        .next()?;
    let space = matches!(
        ch,
        '\u{85}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
    );
    (!space).then_some(len)
}

fn word_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while let Some(len) = word_char_len(bytes, index) {
        index += len;
    }
    index
}

/// A literal's suffix (the `r` in `"a"r"b"`) belongs to the literal, so it never
/// opens the next raw string; a digit cannot start a suffix.
fn suffix_end(bytes: &[u8], end: usize) -> usize {
    match bytes.get(end) {
        Some(byte) if byte.is_ascii_digit() => end,
        _ => word_end(bytes, end),
    }
}

fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    start
        + bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(bytes.len() - start)
}

fn block_comment_end(bytes: &[u8], start: usize) -> Result<usize, UnterminatedSource> {
    let mut depth = 1;
    let mut index = start + 2;
    while depth > 0 {
        match (bytes.get(index), bytes.get(index + 1)) {
            (None, _) => return unterminated("block comment", start),
            (Some(b'/'), Some(b'*')) => (depth, index) = (depth + 1, index + 2),
            (Some(b'*'), Some(b'/')) => (depth, index) = (depth - 1, index + 2),
            _ => index += 1,
        }
    }
    Ok(index)
}

fn quoted_end(bytes: &[u8], start: usize, body: usize) -> Result<usize, UnterminatedSource> {
    let mut index = body;
    loop {
        match bytes.get(index) {
            None => return unterminated("string literal", start),
            Some(b'\\') => index += 2,
            Some(b'"') => return Ok(index + 1),
            Some(_) => index += 1,
        }
    }
}

/// `Some(end)` for a char literal, `None` for a lifetime or label such as `'a`.
fn char_literal_end(bytes: &[u8], quote: usize) -> Result<Option<usize>, UnterminatedSource> {
    let body = quote + 1;
    let close = match bytes.get(body) {
        None => return unterminated("char literal", quote),
        Some(b'\\') => match bytes.get(body + 1) {
            Some(b'u') if bytes.get(body + 2) == Some(&b'{') => {
                match bytes[body..].iter().position(|byte| *byte == b'}') {
                    Some(brace) => body + brace + 1,
                    None => return unterminated("char literal", quote),
                }
            }
            Some(b'x') => body + 4,
            Some(escaped) => body + 1 + utf8_len(*escaped),
            None => return unterminated("char literal", quote),
        },
        Some(first) => {
            let close = body + utf8_len(*first);
            if bytes.get(close) != Some(&b'\'')
                && word_char_len(bytes, body).is_some()
                && !first.is_ascii_digit()
            {
                return Ok(None);
            }
            close
        }
    };
    match bytes.get(close) {
        Some(b'\'') => Ok(Some(close + 1)),
        _ => unterminated("char literal", quote),
    }
}

fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0.. => 4,
        0xE0.. => 3,
        0xC0.. => 2,
        _ => 1,
    }
}

/// Literal end for `b"`, `c"`, `b'`, `r"`, `r#"`, `br`/`cr` forms, or `None` for a
/// plain word such as `br` or the raw identifier `r#type`.
fn prefixed_literal_end(
    bytes: &[u8],
    start: usize,
    word_end: usize,
) -> Result<Option<usize>, UnterminatedSource> {
    let word = &bytes[start..word_end];
    match (word, bytes.get(word_end)) {
        (b"b" | b"c", Some(b'"')) => quoted_end(bytes, start, word_end + 1).map(Some),
        (b"b", Some(b'\'')) => match char_literal_end(bytes, word_end)? {
            Some(end) => Ok(Some(end)),
            None => unterminated("byte literal", start),
        },
        (b"r" | b"br" | b"cr", Some(b'"' | b'#')) => {
            let hashes = bytes[word_end..]
                .iter()
                .take_while(|byte| **byte == b'#')
                .count();
            let open = word_end + hashes;
            if bytes.get(open) != Some(&b'"') {
                if word == b"r" && hashes == 1 && word_char_len(bytes, open).is_some() {
                    return Ok(None);
                }
                return unterminated("raw string literal", start);
            }
            let mut index = open + 1;
            loop {
                match bytes.get(index) {
                    None => return unterminated("raw string literal", start),
                    Some(b'"')
                        if bytes[index + 1..]
                            .iter()
                            .take(hashes)
                            .filter(|byte| **byte == b'#')
                            .count()
                            == hashes =>
                    {
                        return Ok(Some(index + 1 + hashes));
                    }
                    Some(_) => index += 1,
                }
            }
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod source_text_tests;
