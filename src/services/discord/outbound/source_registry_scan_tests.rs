//! Source scan: every outbox producer's `source` must be a registered LoopbackInternal label,
//! because `enqueue_outbox*` rejects any other label at runtime with only a WARN.

use super::{SendCallerClass, validate_send_source_for};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Struct literals whose `source` field becomes `message_outbox.source`.
const CARRIERS: &[&str] = &["OutboxMessage"];

/// Producers whose source is chosen elsewhere; each origin is checked where it is written.
const FORWARDED: &[(&str, &str)] = &[
    // JS policy labels (`agentdesk.message.queue`), validated when the policy enqueues.
    ("engine/ops/message_ops.rs", "&source"),
];

#[test]
fn every_outbox_producer_source_is_registered() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    let sources: Vec<(String, String)> = files
        .iter()
        .filter(|path| !is_test_file(path))
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("read Rust source");
            let rel = path.strip_prefix(&root).expect("source-relative path");
            (
                rel.to_string_lossy().replace('\\', "/"),
                production_text(&text),
            )
        })
        .collect();
    let consts = str_consts(&sources);
    let mut unregistered = Vec::new();
    let mut forwarded = BTreeSet::new();
    for (file, text) in &sources {
        for (line, expr) in carrier_sources(text) {
            let literal = expr
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'));
            let label = literal.or_else(|| consts.get(const_name(&expr))?.as_deref());
            match label {
                Some(label) => {
                    if validate_send_source_for(label, SendCallerClass::LoopbackInternal).is_err() {
                        unregistered.push(format!("{file}:{line} `{label}`"));
                    }
                }
                None => {
                    forwarded.insert((file.clone(), expr));
                }
            }
        }
    }
    assert!(
        unregistered.is_empty(),
        "outbox producers use sources missing from POLICIES, so every enqueue is rejected: {unregistered:#?}"
    );
    let expected: BTreeSet<(String, String)> = FORWARDED
        .iter()
        .map(|(file, expr)| (file.to_string(), expr.to_string()))
        .collect();
    assert_eq!(
        forwarded, expected,
        "a producer with a non-literal source needs its origin checked and a FORWARDED entry"
    );
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("source dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Whole-file test modules by naming convention; inline `#[cfg(test)]` code is blanked instead.
fn is_test_file(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string());
    let stem = stem.unwrap_or_default();
    stem.ends_with("tests")
        || stem.ends_with("_test")
        || stem.starts_with("test_")
        || path.parent().is_some_and(|parent| {
            parent
                .components()
                .any(|part| part.as_os_str().to_string_lossy().ends_with("tests"))
        })
}

/// Blanks `#[cfg(test)]` items (rustfmt layout: a braced item closes at its own indent) and
/// keeps line numbers.
fn production_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let indent = |line: &str| line.len() - line.trim_start().len();
    let mut keep = vec![true; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "#[cfg(test)]" {
            index += 1;
            continue;
        }
        let column = indent(lines[index]);
        let mut end = index;
        while end + 1 < lines.len() && !lines[end].trim_end().ends_with(['{', ';', ',']) {
            end += 1;
        }
        if lines[end].trim_end().ends_with('{') {
            while end + 1 < lines.len()
                && !(indent(lines[end]) == column && lines[end].trim_start().starts_with('}'))
            {
                end += 1;
            }
        }
        keep[index..=end].fill(false);
        index = end + 1;
    }
    let kept = lines.iter().zip(keep);
    let kept = kept.map(|(line, keep)| if keep { *line } else { "" });
    kept.collect::<Vec<_>>().join("\n")
}

/// `&str` constants by name; a name bound to two different values resolves to `None`.
fn str_consts(sources: &[(String, String)]) -> BTreeMap<String, Option<String>> {
    let pattern = regex::Regex::new(
        r#"\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:'static\s+)?str\s*=\s*"([^"\\]*)"\s*;"#,
    )
    .expect("const pattern");
    let mut consts: BTreeMap<String, Option<String>> = BTreeMap::new();
    for (_, text) in sources {
        for capture in pattern.captures_iter(text) {
            let value = Some(capture[2].to_string());
            consts
                .entry(capture[1].to_string())
                .and_modify(|seen| {
                    if *seen != value {
                        *seen = None;
                    }
                })
                .or_insert(value);
        }
    }
    consts
}

fn const_name(expr: &str) -> &str {
    let name = expr.trim_start_matches('&');
    let name = name.rsplit("::").next().unwrap_or(name);
    let is_const = name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if is_const { name } else { "" }
}

/// `(line, source expression)` for each carrier struct literal; a missing field or a struct
/// update reads as an unresolved source so it cannot pass silently.
fn carrier_sources(text: &str) -> Vec<(usize, String)> {
    let pattern = regex::Regex::new(&format!(r"\b(?:{})\s*\{{", CARRIERS.join("|")))
        .expect("carrier pattern");
    let mut found = Vec::new();
    for hit in pattern.find_iter(text) {
        let before = text[..hit.start()].trim_end();
        if ["struct", "impl", "->", "for", "enum"]
            .iter()
            .any(|keyword| before.ends_with(keyword))
        {
            continue;
        }
        let line = text[..hit.start()].matches('\n').count() + 1;
        let fields = top_level_fields(&text[hit.end()..]);
        let source = fields.iter().find_map(|field| {
            if field.starts_with("..") {
                return Some(field.clone());
            }
            match split_field(field) {
                Some(("source", expr)) => Some(expr.to_string()),
                None if field == "source" => Some(field.clone()),
                _ => None,
            }
        });
        found.push((line, source.unwrap_or_else(|| "<no source field>".into())));
    }
    found
}

/// Comma-separated fields up to the literal's closing brace, whitespace collapsed.
fn top_level_fields(body: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                current.push(c);
                while let Some(inner) = chars.next() {
                    current.push(inner);
                    if inner == '\\' {
                        current.extend(chars.next());
                    } else if inner == '"' {
                        break;
                    }
                }
                continue;
            }
            '/' if chars.clone().next() == Some('/') => {
                chars.find(|&inner| inner == '\n');
                current.push(' ');
                continue;
            }
            '(' | '[' | '{' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '}' if depth == 0 => break,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                fields.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    fields.push(current);
    let collapse = |field: &String| field.split_whitespace().collect::<Vec<_>>().join(" ");
    let fields = fields.iter().map(collapse);
    fields.filter(|field| !field.is_empty()).collect()
}

/// `name: expr`, splitting at the first `:` that is not part of a `::` path.
fn split_field(field: &str) -> Option<(&str, &str)> {
    let bytes = field.as_bytes();
    let colon = (0..bytes.len()).find(|&at| {
        bytes[at] == b':' && bytes.get(at + 1) != Some(&b':') && (at == 0 || bytes[at - 1] != b':')
    })?;
    Some((field[..colon].trim(), field[colon + 1..].trim()))
}
