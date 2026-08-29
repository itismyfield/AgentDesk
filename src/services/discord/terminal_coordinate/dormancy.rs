use std::fs;
use std::path::{Path, PathBuf};

const API_IDENTIFIERS: &[&str] = &[
    "terminal_coordinate",
    "CanonicalC",
    "SourceRange",
    "RouteFamily",
    "TurnIdentity",
    "TerminalCoordinate",
    "TerminalCoordinateCandidate",
    "TerminalCoordinateError",
    "validate_terminal_coordinate",
];

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read discord source directory") {
        let path = entry.expect("read discord entry").path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) != Some("terminal_coordinate") {
                rust_sources(&path, out);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs")
            && path.file_name().and_then(|name| name.to_str()) != Some("terminal_coordinate.rs")
        {
            out.push(path);
        }
    }
}

fn strip_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut clean = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut block_depth = 0usize;

    while index < bytes.len() {
        if block_depth != 0 {
            if bytes[index..].starts_with(b"/*") {
                block_depth += 1;
                clean.extend_from_slice(b"  ");
                index += 2;
            } else if bytes[index..].starts_with(b"*/") {
                block_depth -= 1;
                clean.extend_from_slice(b"  ");
                index += 2;
            } else {
                clean.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                index += 1;
            }
            continue;
        }

        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                clean.push(b' ');
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            block_depth = 1;
            clean.extend_from_slice(b"  ");
            index += 2;
            continue;
        }
        let raw_prefix = if bytes[index] == b'r' {
            Some(index + 1)
        } else if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'r') {
            Some(index + 2)
        } else {
            None
        };
        if let Some(mut quote) = raw_prefix {
            while bytes.get(quote) == Some(&b'#') {
                quote += 1;
            }
            if bytes.get(quote) == Some(&b'"') {
                let hashes = quote - raw_prefix.unwrap();
                let terminator = format!("\"{}", "#".repeat(hashes));
                let terminator = terminator.as_bytes();
                while index <= quote {
                    clean.push(b' ');
                    index += 1;
                }
                while index < bytes.len() {
                    if bytes[index..].starts_with(terminator) {
                        clean.extend(std::iter::repeat(b' ').take(terminator.len()));
                        index += terminator.len();
                        break;
                    }
                    clean.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                    index += 1;
                }
                continue;
            }
        }

        if bytes[index] == b'b' && bytes.get(index + 1) == Some(&b'"') {
            clean.push(b' ');
            index += 1;
        }
        if bytes[index] == b'"' {
            clean.push(b' ');
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    clean.push(b' ');
                    index += 1;
                    if index < bytes.len() {
                        clean.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                        index += 1;
                    }
                } else if bytes[index] == b'"' {
                    clean.push(b' ');
                    index += 1;
                    break;
                } else {
                    clean.push(if bytes[index] == b'\n' { b'\n' } else { b' ' });
                    index += 1;
                }
            }
            continue;
        }

        clean.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(clean).expect("source remains utf-8")
}

fn identifiers(source: &str) -> impl Iterator<Item = &str> {
    source
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
}

fn forbidden_uses(source: &str, parent_module: bool) -> Vec<String> {
    let mut clean = strip_comments_and_strings(source);
    if parent_module {
        clean = clean.replacen("mod terminal_coordinate;", "", 1);
    }
    identifiers(&clean)
        .filter(|token| API_IDENTIFIERS.contains(token))
        .map(str::to_owned)
        .collect()
}

#[test]
fn terminal_coordinate_api_has_no_sibling_callers() {
    let discord = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/discord");
    let mut sources = Vec::new();
    rust_sources(&discord, &mut sources);
    let mut violations = Vec::new();
    for path in sources {
        let source = fs::read_to_string(&path).expect("read discord rust source");
        for identifier in forbidden_uses(&source, path == discord.join("mod.rs")) {
            violations.push(format!("{}: {identifier}", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "terminal-coordinate substrate must remain dormant:\n{}",
        violations.join("\n")
    );
}

#[test]
fn caller_zero_gate_kills_representative_mutations() {
    for mutation in [
        "let _ = CanonicalC::new(1);",
        "let _ = RouteFamily::Watcher;",
        "let _ = TerminalCoordinateCandidate::new(1, None, 0, identity, route);",
        "let _ = validate_terminal_coordinate(candidate);",
        "use crate::services::discord::terminal_coordinate::TurnIdentity;",
    ] {
        assert!(
            !forbidden_uses(mutation, false).is_empty(),
            "mutation survived: {mutation}"
        );
    }

    let inert = r##"
        // CanonicalC RouteFamily::Watcher
        /* TerminalCoordinateCandidate::new */
        let normal = "validate_terminal_coordinate";
        let raw = r#"terminal_coordinate::TurnIdentity"#;
    "##;
    assert!(forbidden_uses(inert, false).is_empty());
    assert!(forbidden_uses("mod terminal_coordinate;", true).is_empty());
}
