//! Lexical source contract (not a SQL parser): `auto_queue_runs.status = 'completed'`
//! is written only by the canonical derived writer and the pinned destructive admin owners.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Production files allowed to spell a literal `status = 'completed'` run
/// update, with the exact number of statements each may contain.
const COMPLETED_LITERAL_OWNERS: &[(&str, usize)] = &[
    // Canonical derived writer `finalize_run_if_ready_on_pg_tx`.
    ("src/db/auto_queue/runs.rs", 1),
    // Destructive admin resets: run-scoped (1) and global (2 branches).
    ("src/services/auto_queue/command.rs", 3),
];

/// Production files allowed to bind the run status from a parameter.
const PARAMETERIZED_STATUS_OWNERS: &[(&str, usize)] = &[
    // Operator cancel/end force-termination `terminalize_selected_runs_on_pg_tx`.
    ("src/services/auto_queue/cancel_run/terminalize_tx.rs", 1),
    // Restore hand-off re-opens a cancelled run.
    ("src/services/auto_queue/fsm.rs", 1),
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn collect_files(root: &Path, extension: &str, skip_dir: &str) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("read source directory") {
            let entry = entry.expect("source entry");
            let path = entry.path();
            if entry.file_type().expect("source type").is_dir() {
                if path.file_name().is_some_and(|name| name == skip_dir) {
                    continue;
                }
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == extension) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Test files and inline test modules are skipped: their fixtures legitimately seed terminal rows.
fn is_test_only_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    name == "tests.rs" || name.ends_with("_tests.rs") || name == "test_support.rs"
}

/// Removes `#[cfg(test)] mod name { … }` blocks, tracking string literals and
/// comments so braces inside SQL or JSON fixtures do not unbalance the scan.
fn strip_inline_test_modules(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(attr_at) = rest.find("#[cfg(test)]") {
        let after_attr = &rest[attr_at + "#[cfg(test)]".len()..];
        let header = after_attr.trim_start();
        let is_inline_mod = header
            .strip_prefix("pub(crate) ")
            .or_else(|| header.strip_prefix("pub "))
            .unwrap_or(header)
            .strip_prefix("mod ")
            .and_then(|tail| tail.split_once('{'))
            .is_some_and(|(name, _)| !name.contains(';'));
        out.push_str(&rest[..attr_at]);
        if !is_inline_mod {
            out.push_str("#[cfg(test)]");
            rest = after_attr;
            continue;
        }
        let open = attr_at
            + "#[cfg(test)]".len()
            + (after_attr.len() - header.len())
            + header.find('{').expect("inline module brace");
        let close = matching_brace(rest, open).unwrap_or_else(|| {
            panic!(
                "unbalanced inline test module near: {}",
                &rest[attr_at..(attr_at + 80).min(rest.len())]
            )
        });
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

fn matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index = source[index..]
                    .find('\n')
                    .map_or(bytes.len(), |n| index + n);
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = source[index..]
                    .find("*/")
                    .map_or(bytes.len(), |n| index + n + 2);
                continue;
            }
            b'"' => {
                index = skip_string(bytes, index);
                continue;
            }
            b'\'' => {
                index = skip_char_literal(source, index);
                continue;
            }
            b'r' if source[index..].starts_with("r#") || source[index..].starts_with("r\"") => {
                index = skip_raw_string(source, index);
                continue;
            }
            b'b' if source[index..].starts_with("br#") || source[index..].starts_with("br\"") => {
                index = skip_raw_string(source, index + 1);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// Skips `'x'`, `'\n'` and `'"'` char literals; a lifetime tick advances one byte.
fn skip_char_literal(source: &str, start: usize) -> usize {
    let rest = &source[start..];
    if rest.as_bytes().get(1) == Some(&b'\\') {
        return rest[2..].find('\'').map_or(source.len(), |n| start + n + 3);
    }
    let mut chars = rest.chars();
    let (_tick, literal, close) = (chars.next(), chars.next(), chars.next());
    match (literal, close) {
        (Some(literal), Some('\'')) => start + 1 + literal.len_utf8() + 1,
        _ => start + 1,
    }
}

fn skip_raw_string(source: &str, start: usize) -> usize {
    let hashes = source[start + 1..]
        .chars()
        .take_while(|c| *c == '#')
        .count();
    let open = start + 1 + hashes;
    if source.as_bytes().get(open) != Some(&b'"') {
        return start + 1;
    }
    let terminator = format!("\"{}", "#".repeat(hashes));
    source[open + 1..]
        .find(&terminator)
        .map_or(source.len(), |n| open + 1 + n + terminator.len())
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns, for each `UPDATE auto_queue_runs` statement, the SET clause text up
/// to the end of its string literal.
fn run_update_statements(normalized: &str) -> Vec<String> {
    normalized
        .match_indices("UPDATE auto_queue_runs")
        .map(|(at, _)| {
            let tail = &normalized[at..];
            let end = tail.find('"').unwrap_or(tail.len());
            tail[..end].to_string()
        })
        .collect()
}

fn sets_status_to(statement: &str, value: &str) -> bool {
    let compact = statement.replace(' ', "");
    compact.contains(&format!("SETstatus={value}")) || compact.contains(&format!(",status={value}"))
}

fn sets_parameterized_status(statement: &str) -> bool {
    let compact = statement.replace(' ', "");
    compact.contains("SETstatus=$") || compact.contains(",status=$")
}

fn production_rust_sources() -> Vec<(String, String)> {
    let root = repo_root();
    collect_files(&root.join("src"), "rs", "__never__")
        .into_iter()
        .filter(|path| !is_test_only_file(path))
        .map(|path| {
            let relative = path
                .strip_prefix(&root)
                .expect("source below repo root")
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(&path).expect("read rust source");
            (relative, normalize(&strip_inline_test_modules(&source)))
        })
        .collect()
}

fn count_matching(
    statements_by_file: &[(String, String)],
    predicate: fn(&str) -> bool,
) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for (relative, normalized) in statements_by_file {
        let matched = run_update_statements(normalized)
            .iter()
            .filter(|statement| predicate(statement))
            .count();
        if matched > 0 {
            counts.insert(relative.clone(), matched);
        }
    }
    counts
}

fn expected(owners: &[(&str, usize)]) -> BTreeMap<String, usize> {
    owners
        .iter()
        .map(|(path, count)| ((*path).to_string(), *count))
        .collect()
}

#[test]
fn completed_run_status_literal_writers_are_pinned() {
    let sources = production_rust_sources();
    let actual = count_matching(&sources, |statement| {
        sets_status_to(statement, "'completed'")
    });
    assert_eq!(
        actual,
        expected(COMPLETED_LITERAL_OWNERS),
        "a production file writes auto_queue_runs.status = 'completed' outside the pinned owners; \
         route derived completion through finalize_run_if_ready_on_pg_tx"
    );
}

#[test]
fn parameterized_run_status_writers_are_pinned() {
    let sources = production_rust_sources();
    let actual = count_matching(&sources, sets_parameterized_status);
    assert_eq!(
        actual,
        expected(PARAMETERIZED_STATUS_OWNERS),
        "a production file binds auto_queue_runs.status from a parameter outside the pinned owners"
    );
}

#[test]
fn policies_never_write_run_status_directly() {
    let root = repo_root();
    for path in collect_files(&root.join("policies"), "js", "__tests__") {
        let source = std::fs::read_to_string(&path).expect("read policy source");
        let joined = normalize(&source).replace("\" + \"", "");
        for statement in run_update_statements(&joined) {
            assert!(
                !statement.replace(' ', "").contains("status="),
                "{} writes auto_queue_runs.status directly: {statement}",
                path.display()
            );
        }
    }
}

#[test]
fn inline_test_module_stripper_skips_fixture_sql() {
    let source = r##"
        fn production() { sqlx::query("UPDATE auto_queue_runs SET status = 'paused' WHERE id = $1"); }
        #[cfg(test)]
        mod tests {
            // a brace in a comment: {
            const JSON: &str = r#"{"a": "}"}"#;
            fn seed() { sqlx::query("UPDATE auto_queue_runs SET status = 'completed' WHERE id = '{'"); }
        }
        fn after() {}
    "##;
    let stripped = strip_inline_test_modules(source);
    assert!(stripped.contains("fn production()"));
    assert!(stripped.contains("fn after()"));
    assert!(!stripped.contains("'completed'"));
    assert!(
        strip_inline_test_modules("#[cfg(test)]\nmod tests;\nfn x() {}").contains("mod tests;")
    );
}

#[test]
fn status_predicates_read_set_clause_only() {
    let statement =
        "UPDATE auto_queue_runs SET completed_at = NOW() WHERE id = $1 AND status = 'completed'";
    assert!(!sets_status_to(statement, "'completed'"));
    assert!(sets_status_to(
        "UPDATE auto_queue_runs SET status = 'completed', completed_at = NOW()",
        "'completed'"
    ));
    assert!(sets_status_to(
        "UPDATE auto_queue_runs SET completed_at = NOW(), status = 'completed'",
        "'completed'"
    ));
    assert!(sets_parameterized_status(
        "UPDATE auto_queue_runs SET status = $2, completed_at = NOW()"
    ));
}
