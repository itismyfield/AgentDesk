//! Finds a pattern in this crate's production Rust code: test files, test modules and
//! `#[cfg(test)]` items are skipped, and owner functions may hold it.

use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// A production function allowed to hold the pattern: file, signature and why. An empty
/// signature covers the whole file.
pub(crate) type Owner = (&'static str, &'static str, &'static str);

/// Every match outside test code and `owners` as `file:line`, then the owners no match needs.
pub(crate) fn unowned_matches<'o>(
    pattern: &Regex,
    owners: &'o [Owner],
) -> (Vec<String>, Vec<&'o Owner>) {
    let files = source_files(Path::new(env!("CARGO_MANIFEST_DIR")));
    let test_only = test_only_paths(&files);
    let mut matches = Vec::new();
    let mut used = BTreeSet::new();
    for (path, text) in &files {
        if test_only.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        let name = path.to_str().unwrap();
        let scan = scan(name, text, pattern, owners).unwrap();
        matches.extend(scan.lines.iter().map(|line| format!("{name}:{line}")));
        used.extend(
            scan.owners_used
                .iter()
                .map(|signature| (name.to_string(), *signature)),
        );
    }
    let stale = owners
        .iter()
        .filter(|(file, signature, _)| {
            !signature.is_empty() && !used.contains(&(file.to_string(), *signature))
        })
        .collect();
    (matches, stale)
}

static TEST_CFG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#\[cfg\((?:test|all\([^\]]*\btest\b[^\]]*)\)\]").unwrap());
static TEST_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*#!\[cfg\(test\)\]").unwrap());
static MOD_DECL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;").unwrap());
static PATH_ATTR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*#\[path\s*=\s*"([^"]+)"\]"#).unwrap());

/// What one file's scan found: match lines outside test items and owner functions, and
/// the owner signatures whose function holds at least one match.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Scan<'a> {
    pub(crate) lines: Vec<usize>,
    pub(crate) owners_used: Vec<&'a str>,
}

/// Scans production code of `file`; `Err` when the source does not lex or an owner function
/// is missing, so a scan never silently skips code.
pub(crate) fn scan<'a>(
    file: &str,
    text: &str,
    pattern: &Regex,
    owners: &[(&str, &'a str, &str)],
) -> Result<Scan<'a>, String> {
    let code = crate::test_support::rust_code_mask(text).map_err(|e| format!("{file}: {e:?}"))?;
    let literals =
        crate::test_support::strip_rust_comments(text).map_err(|e| format!("{file}: {e:?}"))?;
    if TEST_FILE_RE.is_match(&code) {
        return Ok(Scan::default());
    }
    let tests = test_item_spans(&code);
    let mut owned = Vec::new();
    for (owner, signature, _) in owners {
        if *owner != file {
            continue;
        }
        if signature.is_empty() {
            return Ok(Scan::default());
        }
        let span = crate::test_support::braced_item_span(&code, signature)
            .ok_or_else(|| format!("{file}: owner `{signature}` not found"))?;
        owned.push((*signature, span));
    }
    let mut scan = Scan::default();
    for found in pattern.find_iter(&literals) {
        let at = found.start();
        if tests.iter().any(|span| span.contains(&at)) {
            continue;
        }
        match owned.iter().find(|(_, span)| span.contains(&at)) {
            Some((signature, _)) if !scan.owners_used.contains(signature) => {
                scan.owners_used.push(signature)
            }
            Some(_) => {}
            None => scan.lines.push(text[..at].matches('\n').count() + 1),
        }
    }
    Ok(scan)
}

/// Byte ranges of items gated by a test-only `#[cfg(..)]` in a code mask.
fn test_item_spans(code: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = code.as_bytes();
    let mut spans = Vec::new();
    for attr in TEST_CFG_RE.find_iter(code) {
        if attr.as_str().contains("not(") {
            continue;
        }
        let (mut parens, mut depth, mut end) = (0_i64, 0_i64, code.len());
        for (index, &byte) in bytes.iter().enumerate().skip(attr.end()) {
            match byte {
                b'(' | b'[' => parens += 1,
                b')' | b']' => parens -= 1,
                b'{' if parens <= 0 => depth += 1,
                b'}' if parens <= 0 => {
                    depth -= 1;
                    if depth == 0 {
                        end = index + 1;
                        break;
                    }
                }
                b';' | b',' if parens <= 0 && depth == 0 => {
                    end = index + 1;
                    break;
                }
                _ => {}
            }
        }
        spans.push(attr.start()..end);
    }
    spans
}

/// The rest of `code` after a test-only `#[cfg(..)]` attribute, or `None` for other lines.
fn after_test_cfg(code: &str) -> Option<&str> {
    let found = TEST_CFG_RE.find(code).filter(|found| found.start() == 0)?;
    (!found.as_str().contains("not("))
        .then(|| code[found.end()..].split("//").next().unwrap().trim())
}

/// One `mod name;` declaration in `file`.
struct ModDecl {
    file: PathBuf,
    test_only: bool,
    path_attr: Option<String>,
    name: String,
}

fn mod_decls(file: &Path, text: &str) -> Vec<ModDecl> {
    let (mut decls, mut gated, mut attr) = (Vec::new(), false, None);
    for line in text.lines() {
        let line = match after_test_cfg(line.trim()) {
            Some(rest) => {
                gated = true;
                rest
            }
            None => line,
        };
        if line.trim().is_empty() || line.trim().starts_with("//") {
            continue;
        } else if let Some(found) = PATH_ATTR_RE.captures(line) {
            attr = Some(found[1].to_string());
        } else if let Some(found) = MOD_DECL_RE.captures(line) {
            decls.push(ModDecl {
                file: file.to_path_buf(),
                test_only: gated,
                path_attr: attr.take(),
                name: found[1].to_string(),
            });
            gated = false;
        } else if !line.trim().starts_with("#[") {
            (gated, attr) = (false, None);
        }
    }
    decls
}

/// `a/b/../c` as `a/c`, the way `#[path = "../c"]` resolves.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Files and directories that only test-only modules (and modules they declare) live in.
pub(crate) fn test_only_paths(files: &[(PathBuf, String)]) -> Vec<PathBuf> {
    let decls: Vec<ModDecl> = files
        .iter()
        .flat_map(|(path, text)| mod_decls(path, text))
        .collect();
    // A `#[path]`-loaded file owns its directory like `mod.rs` does.
    let path_loaded: BTreeSet<PathBuf> = decls
        .iter()
        .filter_map(|decl| Some(lexical(&decl.file.parent()?.join(decl.path_attr.as_ref()?))))
        .collect();
    let target = |decl: &ModDecl| {
        let dir = decl.file.parent().unwrap();
        let file = match &decl.path_attr {
            Some(path) => lexical(&dir.join(path)),
            None => {
                let owns_dir = path_loaded.contains(&decl.file)
                    || matches!(
                        decl.file.file_name().and_then(|name| name.to_str()),
                        Some("mod.rs" | "lib.rs" | "main.rs")
                    );
                let base = match owns_dir {
                    true => dir.to_path_buf(),
                    false => dir.join(decl.file.file_stem().unwrap()),
                };
                base.join(format!("{}.rs", decl.name))
            }
        };
        let children = match file.file_name().and_then(|name| name.to_str()) {
            Some("mod.rs") => file.parent().unwrap().to_path_buf(),
            _ => file.with_extension(""),
        };
        [file, children]
    };
    let mut test_only: Vec<PathBuf> = Vec::new();
    loop {
        let before = test_only.len();
        for decl in &decls {
            let inside = test_only.iter().any(|prefix| decl.file.starts_with(prefix));
            if decl.test_only || inside {
                for path in target(decl) {
                    if !test_only.contains(&path) {
                        test_only.push(path);
                    }
                }
            }
        }
        if test_only.len() == before {
            return test_only;
        }
    }
}

fn source_files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut pending = vec![root.join("src")];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            files.push((path.strip_prefix(root).unwrap().to_path_buf(), text));
        }
    }
    files
}
