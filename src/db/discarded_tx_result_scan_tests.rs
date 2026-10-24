//! Production code must not discard a sqlx result inside a transaction: a failed statement
//! aborts it, and COMMIT then rolls back while sqlx still returns `Ok`.

use regex::Regex;
use std::path::{Path, PathBuf};

#[test]
fn transaction_statements_never_discard_their_result() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    let discarded = Regex::new(
        r"let\s+_\s*=\s*sqlx::query\w*(?:::<[^;]*?>)?\([^;]*?\.await\s*;|sqlx::query\w*(?:::<[^;]*?>)?\([^;]*?\.await\s*\.ok\(\)\s*;",
    )
    .expect("statement regex");
    let on_transaction =
        Regex::new(r"\.(?:execute|fetch\w*)\(\s*&mut\s+\*{1,2}\s*(?:tx|txn|transaction)\s*\)")
            .expect("executor regex");
    let mut offenders = Vec::new();
    for path in files.iter().filter(|path| !is_test_file(path)) {
        let text = production_text(&std::fs::read_to_string(path).expect("read Rust source"));
        for statement in discarded.find_iter(&text) {
            if on_transaction.is_match(statement.as_str()) {
                let line = text[..statement.start()].lines().count() + 1;
                let rel = path.strip_prefix(&root).expect("source-relative path");
                offenders.push(format!("src/{}:{line}", rel.to_string_lossy()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "propagate these transaction statement errors with `?` (or wrap a best-effort \
         write in a SAVEPOINT) instead of discarding them: {offenders:#?}"
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
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
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
    lines
        .iter()
        .zip(keep)
        .map(|(line, keep)| if keep { *line } else { "" })
        .collect::<Vec<_>>()
        .join("\n")
}
