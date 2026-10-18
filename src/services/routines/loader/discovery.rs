use super::{LoadedRoutineScript, RoutineScriptCandidate, full_source_version};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

pub(super) fn stable_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn root_config_identity(root: &Path) -> PathBuf {
    stable_absolute_path(root)
}

pub(super) fn candidate_failure_key(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| stable_absolute_path(path))
}

pub(super) fn routine_roots_identity(roots: &[PathBuf]) -> PathBuf {
    let identities = roots
        .iter()
        .map(|root| root_config_identity(root).to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\0");
    PathBuf::from(full_source_version(&identities))
}

pub(super) fn script_ref(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn add_cached_candidates_for_root(
    existing_scripts: &HashMap<String, LoadedRoutineScript>,
    candidates_by_ref: &mut BTreeMap<String, Vec<RoutineScriptCandidate>>,
    seen_refs: &mut HashSet<String>,
    root_index: usize,
    root: &Path,
) {
    for (script_ref, script) in existing_scripts
        .iter()
        .filter(|(_, script)| script.file.starts_with(root))
    {
        seen_refs.insert(script_ref.clone());
        candidates_by_ref
            .entry(script_ref.clone())
            .or_default()
            .push(RoutineScriptCandidate {
                root_index,
                root: root.to_path_buf(),
                path: script.file.clone(),
                cached: Some(script.clone()),
            });
    }
}

pub(super) fn collect_routine_script_paths(
    root: &Path,
    loaded: &HashMap<String, LoadedRoutineScript>,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    collect_routine_script_paths_inner(root, root, loaded, out)
}

fn collect_routine_script_paths_inner(
    root: &Path,
    current_dir: &Path,
    loaded: &HashMap<String, LoadedRoutineScript>,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(current_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_routine_script_paths_inner(root, &path, loaded, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "js") {
            if is_routine_entry(root, &path, loaded) {
                out.push(path);
            } else {
                tracing::debug!(
                    routine_script = %path.display(),
                    "skipped .js without an agentdesk.routines.register( call"
                );
            }
        }
    }
    Ok(())
}

// Unreadable files and already-loaded scripts stay candidates so the loader's
// failure path keeps their last-known-good registration.
fn is_routine_entry(
    root: &Path,
    path: &Path,
    loaded: &HashMap<String, LoadedRoutineScript>,
) -> bool {
    loaded
        .get(&script_ref(root, path))
        .is_some_and(|script| script.file == path)
        || std::fs::read_to_string(path).map_or(true, |source| declares_quickjs_routine(&source))
}

/// Whether code (not comments, strings, template text or regex literals) calls
/// `agentdesk.routines.register(`, optionally through `globalThis.`.
fn declares_quickjs_routine(source: &str) -> bool {
    let tokens = code_tokens(source);
    tokens.windows(6).enumerate().any(|(start, window)| {
        let [agentdesk, dot1, routines, dot2, register, open] = window else {
            return false;
        };
        agentdesk.is_ident("agentdesk")
            && *dot1 == Token::Punct('.')
            && routines.is_ident("routines")
            && *dot2 == Token::Punct('.')
            && register.is_ident("register")
            && *open == Token::Punct('(')
            && match start.checked_sub(1).map(|index| &tokens[index]) {
                Some(Token::Punct('.')) => start >= 2 && tokens[start - 2].is_ident("globalThis"),
                _ => true,
            }
    })
}

#[derive(Debug, PartialEq)]
enum Token {
    Ident(String),
    Punct(char),
    /// A `)` closing an `if`/`while`/`for`/`with` head, so a statement follows.
    HeadClose,
    Literal,
}

impl Token {
    fn is_ident(&self, name: &str) -> bool {
        matches!(self, Token::Ident(ident) if ident == name)
    }
}

const REGEX_PRECEDING_KEYWORDS: [&str; 14] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

const CONTROL_HEAD_KEYWORDS: [&str; 4] = ["if", "while", "for", "with"];

/// Minimal JS lexer: drops comments and literal contents, keeps identifiers and
/// punctuation, and lexes `${...}` template substitutions as code.
fn code_tokens(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    // Brace depth at which each open template substitution resumes its template.
    let mut templates: Vec<usize> = Vec::new();
    let mut depth = 0usize;
    // Whether each open `(` starts a control-statement head.
    let mut parens: Vec<bool> = Vec::new();
    let mut i = usize::from(chars.first() == Some(&'\u{feff}'));
    if chars.get(i) == Some(&'#') && chars.get(i + 1) == Some(&'!') {
        i = skip_line(&chars, i);
    }
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() || c == '\u{feff}' {
            i += 1;
        } else if c == '/' && next == Some('/') {
            i = skip_line(&chars, i);
        } else if c == '/' && next == Some('*') {
            i = skip_block_comment(&chars, i + 2);
        } else if c == '/' && regex_allowed(&tokens) {
            i = skip_regex(&chars, i + 1);
            tokens.push(Token::Literal);
        } else if c == '\'' || c == '"' {
            i = skip_string(&chars, i + 1, c);
            tokens.push(Token::Literal);
        } else if c == '`' {
            i = lex_template(&chars, i + 1, &mut templates, &mut depth, &mut tokens);
        } else if c == '(' {
            parens.push(opens_control_head(&tokens));
            tokens.push(Token::Punct(c));
            i += 1;
        } else if c == ')' {
            tokens.push(if parens.pop() == Some(true) {
                Token::HeadClose
            } else {
                Token::Punct(c)
            });
            i += 1;
        } else if c == '{' {
            depth += 1;
            tokens.push(Token::Punct(c));
            i += 1;
        } else if c == '}' {
            depth = depth.saturating_sub(1);
            if templates.last() == Some(&depth) {
                templates.pop();
                i = lex_template(&chars, i + 1, &mut templates, &mut depth, &mut tokens);
            } else {
                tokens.push(Token::Punct(c));
                i += 1;
            }
        } else if is_ident_char(c) {
            let start = i;
            while i < chars.len() && is_ident_char(chars[i]) {
                i += 1;
            }
            tokens.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            tokens.push(Token::Punct(c));
            i += 1;
        }
    }
    tokens
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || (!c.is_ascii() && !c.is_whitespace())
}

// `if (`, `while (`, `for (`, `for await (` or `with (`, but not a property
// call such as `o.if(`.
fn opens_control_head(tokens: &[Token]) -> bool {
    let mut end = tokens.len();
    if end >= 2 && tokens[end - 1].is_ident("await") && tokens[end - 2].is_ident("for") {
        end -= 1;
    }
    end >= 1
        && CONTROL_HEAD_KEYWORDS
            .iter()
            .any(|keyword| tokens[end - 1].is_ident(keyword))
        && (end < 2 || tokens[end - 2] != Token::Punct('.'))
}

// A `/` starts a regex unless it follows something that ends an expression,
// including a postfix `++`/`--` and a `)` other than a control-statement head's.
fn regex_allowed(tokens: &[Token]) -> bool {
    match tokens.last() {
        None | Some(Token::HeadClose) => true,
        Some(Token::Punct(c @ ('+' | '-'))) => {
            tokens.len() < 2 || tokens[tokens.len() - 2] != Token::Punct(*c)
        }
        Some(Token::Punct(c)) => !matches!(c, ')' | ']'),
        Some(Token::Ident(word)) => REGEX_PRECEDING_KEYWORDS.contains(&word.as_str()),
        Some(Token::Literal) => false,
    }
}

fn skip_line(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() {
        if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            return i + 2;
        }
        i += 1;
    }
    i
}

// Unterminated strings and regexes end at the line break, as in JS.
fn skip_string(chars: &[char], mut i: usize, quote: char) -> usize {
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\n' => return i,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    i
}

fn skip_regex(chars: &[char], mut i: usize) -> usize {
    let mut in_class = false;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\n' => return i,
            '[' => {
                in_class = true;
                i += 1;
            }
            ']' => {
                in_class = false;
                i += 1;
            }
            '/' if !in_class => {
                i += 1;
                while i < chars.len() && is_ident_char(chars[i]) {
                    i += 1;
                }
                return i;
            }
            _ => i += 1,
        }
    }
    i
}

// Skips template text up to its closing backtick or the next `${`, which opens a
// code substitution that the matching `}` returns from.
fn lex_template(
    chars: &[char],
    mut i: usize,
    templates: &mut Vec<usize>,
    depth: &mut usize,
    tokens: &mut Vec<Token>,
) -> usize {
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '`' => {
                tokens.push(Token::Literal);
                return i + 1;
            }
            '$' if chars.get(i + 1) == Some(&'{') => {
                templates.push(*depth);
                *depth += 1;
                tokens.push(Token::Punct('{'));
                return i + 2;
            }
            _ => i += 1,
        }
    }
    tokens.push(Token::Literal);
    i
}

#[cfg(test)]
mod tests {
    use super::super::tests::capture_debug_logs;
    use super::super::{RoutineScriptLoader, discover_routine_script_refs, recover_poisoned_lock};
    use super::candidate_failure_key;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    fn entry(name: &str) -> String {
        format!(
            "agentdesk.routines.register({{ name: '{name}', tick() {{ return {{ action: 'skip' }}; }} }});"
        )
    }

    // Node-only helpers that mention the register call only outside code.
    const HELPERS: [(&str, &str); 7] = [
        (
            "line-comment.js",
            "const fs = require('node:fs'); // agentdesk.routines.register(\nmodule.exports = { fs };\n",
        ),
        (
            "string.js",
            "const usage = \"agentdesk.routines.register({ tick })\";\nmodule.exports = { usage: 'agentdesk.routines.register(' + usage };\n",
        ),
        (
            "template.js",
            "const id = 1;\nconsole.log(`agentdesk.routines.register( ${id + `agentdesk.routines.register(`} done`);\n",
        ),
        (
            "block-comment.js",
            "/*\n * agentdesk.routines.register({ tick() {} })\n */\nmodule.exports = require('node:path');\n",
        ),
        (
            "regex.js",
            "const pattern = /agentdesk.routines.register(\\{)/u;\nmodule.exports = pattern;\n",
        ),
        (
            "control-regex.js",
            "const fs = require(\"node:fs\");\nif (process.argv[2])\n  /agentdesk.routines.register(\\{)/u.test(\n    fs.readFileSync(process.argv[2], \"utf8\")\n  );\n",
        ),
        (
            "for-await-regex.js",
            "async function scan(lines) {\n  for await (const line of lines) /agentdesk.routines.register(\\{)/u.test(line);\n}\nmodule.exports = scan;\n",
        ),
    ];

    fn write(root: &Path, relative: &str, source: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, source).unwrap();
        path
    }

    fn attempts(loader: &RoutineScriptLoader) -> usize {
        loader.state.evaluation_attempts.load(Ordering::Relaxed)
    }

    fn failure_count(loader: &RoutineScriptLoader) -> usize {
        loader
            .state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .len()
    }

    #[test]
    fn node_helpers_are_never_evaluated_in_primary_or_override_roots() {
        let primary = tempfile::tempdir().unwrap();
        let operator = tempfile::tempdir().unwrap();
        for (name, source) in HELPERS {
            write(primary.path(), &format!("monitoring/{name}"), source);
            write(operator.path(), &format!("helpers/{name}"), source);
        }
        write(primary.path(), "top.js", &entry("Top"));
        write(primary.path(), "ops/daily/nested.js", &entry("Nested"));
        write(
            primary.path(),
            "split.js",
            "agentdesk.routines.\n  register({ name: 'Split', tick() { return { action: 'skip' }; } });",
        );
        write(
            primary.path(),
            "star.js",
            "const n = 1\n  * 2; agentdesk.routines.register({ name: 'Star', tick() { return { action: 'skip' }; } });",
        );
        write(
            operator.path(),
            "global.js",
            "globalThis.agentdesk.routines.register({ name: 'Global', tick() { return { action: 'skip' }; } });",
        );
        write(
            operator.path(),
            "after-literals.js",
            "const label = `a ${ `b` } c`; const re = /\\/[/]/; // agentdesk\nlet i = 4; i++ / 2; agentdesk.routines.register({ name: 'After', tick() { return { action: 'skip' }; } });",
        );
        write(
            primary.path(),
            "control-regex-entry.js",
            "if (true) /`/.test('a');\nagentdesk.routines.register({ name: 'Control', tick() { return { action: 'skip' }; } });",
        );
        write(
            operator.path(),
            "call-division.js",
            "const f = () => 4; f() / 2; agentdesk.routines.register({ name: 'Call', tick() { return { action: 'skip' }; } });",
        );
        write(
            operator.path(),
            "property-if-division.js",
            "const o = { if: (x) => x }; o.if(4) / 2; agentdesk.routines.register({ name: 'Prop', tick() { return { action: 'skip' }; } });",
        );
        write(
            operator.path(),
            "paren-division.js",
            "const a = 2; (a) / 2; agentdesk.routines.register({ name: 'Paren', tick() { return { action: 'skip' }; } });",
        );
        let roots = [primary.path().to_path_buf(), operator.path().to_path_buf()];
        let expected = vec![
            "after-literals.js",
            "call-division.js",
            "control-regex-entry.js",
            "global.js",
            "ops/daily/nested.js",
            "paren-division.js",
            "property-if-division.js",
            "split.js",
            "star.js",
            "top.js",
        ];

        let loader = RoutineScriptLoader::new().unwrap();
        let loaded = loader.load_dirs(&roots).unwrap();
        assert_eq!(
            (loaded, attempts(&loader), failure_count(&loader)),
            (expected.len(), expected.len(), 0),
            "(loaded, evaluated, failed): only entries are evaluated"
        );
        assert_eq!(loader.script_refs().unwrap(), expected);
        assert_eq!(discover_routine_script_refs(&roots), expected);
    }

    #[test]
    fn override_conflicts_select_the_declared_file_cold_and_warm() {
        let bundled = tempfile::tempdir().unwrap();
        let operator = tempfile::tempdir().unwrap();
        let bundled_shared = write(bundled.path(), "shared.js", &entry("Bundled Shared"));
        write(operator.path(), "shared.js", HELPERS[0].1);
        write(
            bundled.path(),
            "monitoring/local_worktree_inventory.js",
            HELPERS[1].1,
        );
        let operator_inventory = write(
            operator.path(),
            "monitoring/local_worktree_inventory.js",
            &entry("Operator Inventory"),
        );
        let roots = [bundled.path().to_path_buf(), operator.path().to_path_buf()];
        let assert_selected = |loader: &RoutineScriptLoader| {
            let shared = loader.get_script("shared.js").unwrap().unwrap();
            assert_eq!(
                (shared.name.as_str(), &shared.file),
                ("Bundled Shared", &bundled_shared)
            );
            let inventory = loader
                .get_script("monitoring/local_worktree_inventory.js")
                .unwrap()
                .unwrap();
            assert_eq!(
                (inventory.name.as_str(), &inventory.file),
                ("Operator Inventory", &operator_inventory)
            );
            assert_eq!(failure_count(loader), 0);
        };

        let cold = RoutineScriptLoader::new().unwrap();
        assert_eq!(cold.load_dirs(&roots).unwrap(), 2);
        assert_eq!(attempts(&cold), 2);
        assert_selected(&cold);

        let warm = RoutineScriptLoader::new().unwrap();
        assert_eq!(warm.load_dirs(&roots[..1]).unwrap(), 1);
        assert_eq!(warm.load_dirs(&roots).unwrap(), 1);
        assert_eq!(
            attempts(&warm),
            2,
            "a loaded bundled ref must not pull in the override helper"
        );
        assert_selected(&warm);
    }

    #[test]
    fn unreadable_script_stays_a_candidate_on_the_read_failure_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("binary.js");
        std::fs::write(&path, b"agentdesk.routines.register(\xff\xfe);").unwrap();

        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dir(root.path()).unwrap(), 0);
        let failures = loader
            .state
            .failed_scripts
            .lock()
            .unwrap_or_else(recover_poisoned_lock)
            .clone();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[&candidate_failure_key(&path)].source_version, None);
        assert_eq!(
            discover_routine_script_refs(&[root.path().to_path_buf()]),
            vec!["binary.js"]
        );
    }

    #[test]
    fn undeclared_rewrite_keeps_last_known_good_until_the_file_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let roots = [root.path().to_path_buf()];
        let path = write(root.path(), "retiring.js", &entry("Live"));
        let loader = RoutineScriptLoader::new().unwrap();
        assert_eq!(loader.load_dirs(&roots).unwrap(), 1);

        std::fs::write(&path, "const retired = true;").unwrap();
        let logs = capture_debug_logs(|| assert_eq!(loader.load_dirs(&roots).unwrap(), 0));
        assert!(
            logs.contains("did not call agentdesk.routines.register()"),
            "logs={logs}"
        );
        assert_eq!(
            loader.get_script("retiring.js").unwrap().unwrap().name,
            "Live"
        );
        assert_eq!(failure_count(&loader), 1);
        assert!(discover_routine_script_refs(&roots).is_empty());

        std::fs::remove_file(&path).unwrap();
        assert_eq!(loader.load_dirs(&roots).unwrap(), 0);
        assert!(loader.script_refs().unwrap().is_empty());
        assert_eq!(failure_count(&loader), 0);
    }
}
