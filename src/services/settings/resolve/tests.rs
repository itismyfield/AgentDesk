use super::*;
use serde_json::json;

use SettingSource::{Default as D, KvOverride as K, Yaml as Y};

const CASES: [&str; 8] = [
    "unread",
    "absent",
    "explicit",
    "implicit",
    "implicit_default",
    "explicit_default",
    "wrong_type",
    "non_object",
];

fn blob(key: &RuntimeSetting<impl Copy>, case: &str, kv: Value, default: Value) -> Option<Value> {
    let object = |value: Value, explicit: bool| {
        let mut object = serde_json::Map::new();
        object.insert(key.kv_key.to_owned(), value);
        if explicit {
            object.insert(
                "__runtimeConfigExplicitKeys".to_owned(),
                json!([key.kv_key]),
            );
        }
        Some(Value::Object(object))
    };
    match case {
        "unread" => None,
        "absent" => Some(json!({})),
        "explicit" => object(kv, true),
        "implicit" => object(kv, false),
        "implicit_default" => object(default, false),
        "explicit_default" => object(default, true),
        "wrong_type" => object(json!(kv.to_string()), false),
        "non_object" => Some(json!([kv])),
        _ => unreachable!("{case}"),
    }
}

/// Every kv/YAML combination against the values the pre-resolve readers returned.
fn assert_table<T: Copy + PartialEq + std::fmt::Debug + Serialize>(
    key: &RuntimeSetting<T>,
    kv: T,
    yaml: T,
    set_yaml: fn(&mut Config, T),
    expected: [[(T, SettingSource); 8]; 2],
) {
    let mut yaml_config = Config::default();
    set_yaml(&mut yaml_config, yaml);
    for (row, config) in [Config::default(), yaml_config].iter().enumerate() {
        for (column, case) in CASES.iter().enumerate() {
            let runtime_config = blob(key, case, json!(kv), json!(key.default));
            let (value, source) = expected[row][column];
            assert_eq!(
                resolve(key, runtime_config.as_ref(), Some(config)),
                Resolved { value, source },
                "{} yaml={} {case}",
                key.kv_key,
                row == 1
            );
        }
    }
}

#[test]
fn runtime_settings_keep_precedence_for_every_layer_combination() {
    assert_table(
        &DISPATCH_GATE_ENABLED,
        false,
        false,
        |config, value| config.runtime.dispatch_rate_limit_gate_enabled = Some(value),
        [
            [
                (true, D),
                (true, D),
                (false, K),
                (false, K),
                (true, D),
                (true, K),
                (true, D),
                (true, D),
            ],
            [
                (false, Y),
                (false, Y),
                (false, K),
                (false, K),
                (false, Y),
                (true, K),
                (false, Y),
                (false, Y),
            ],
        ],
    );
    assert_table(
        &DISPATCH_GATE_DANGER_PCT,
        80,
        90,
        |config, value| config.runtime.dispatch_rate_limit_gate_danger_pct = Some(value as u8),
        [
            [
                (100, D),
                (100, D),
                (80, K),
                (80, K),
                (100, D),
                (100, K),
                (100, D),
                (100, D),
            ],
            [
                (90, Y),
                (90, Y),
                (80, K),
                (80, K),
                (90, Y),
                (100, K),
                (90, Y),
                (90, Y),
            ],
        ],
    );
    assert_table(
        &RATE_LIMIT_STALE_SEC,
        300,
        900,
        |config, value| config.runtime.rate_limit_stale_sec = Some(value as u64),
        [
            [
                (600, D),
                (600, D),
                (300, K),
                (300, K),
                (600, D),
                (600, K),
                (600, D),
                (600, D),
            ],
            [
                (900, Y),
                (900, Y),
                (300, K),
                (300, K),
                (900, Y),
                (600, K),
                (900, Y),
                (900, Y),
            ],
        ],
    );
    // Any persisted value outranks YAML here, including a seeded default.
    assert_table(
        &MAX_ENTRY_RETRIES,
        7,
        5,
        |config, value| config.runtime.max_entry_retries = Some(value),
        [
            [
                (3, D),
                (3, D),
                (7, K),
                (7, K),
                (3, K),
                (3, K),
                (3, D),
                (3, D),
            ],
            [
                (5, Y),
                (5, Y),
                (7, K),
                (7, K),
                (3, K),
                (3, K),
                (5, Y),
                (5, Y),
            ],
        ],
    );
}

#[test]
fn missing_yaml_snapshot_falls_back_to_the_default() {
    assert_eq!(
        resolve(&MAX_ENTRY_RETRIES, None, None),
        Resolved {
            value: 3,
            source: D
        }
    );
    assert_eq!(
        resolve_with_kv(&DISPATCH_GATE_DANGER_PCT, Some(95), None),
        Resolved {
            value: 95,
            source: K
        }
    );
}

/// A row key, its stored rows, the YAML snapshot and the expected value and source.
type RowCase<'a> = (
    &'a RowSetting,
    &'a [(&'a str, &'a str)],
    Option<&'a Config>,
    u64,
    SettingSource,
);

/// Row keys: a parsable row, else YAML, else the default (Claude: the generic value), floored.
#[test]
fn row_settings_resolve_row_then_yaml_then_default() {
    let mut yaml = Config::default();
    yaml.runtime.requested_timeout_min = Some(30);
    yaml.runtime.context_compact_percent = Some(70);
    let mut claude_yaml = Config::default();
    claude_yaml.runtime.context_compact_percent_claude = Some(50);
    let (requested, stale) = (&REQUESTED_TIMEOUT_MIN, &IN_PROGRESS_STALE_MIN);
    let (generic, claude) = (&CONTEXT_COMPACT_PERCENT, &CONTEXT_COMPACT_PERCENT_CLAUDE);
    let cases: [RowCase; 14] = [
        (
            requested,
            &[("requested_timeout_min", " 20 ")],
            Some(&yaml),
            20,
            K,
        ),
        (
            requested,
            &[("requested_timeout_min", "0")],
            Some(&yaml),
            1,
            K,
        ),
        (
            requested,
            &[("requested_timeout_min", "7.5")],
            Some(&yaml),
            30,
            Y,
        ),
        (requested, &[], Some(&yaml), 30, Y),
        (requested, &[], None, 45, D),
        (stale, &[("in_progress_stale_min", "-5")], None, 120, D),
        (
            generic,
            &[("context_compact_percent", "0")],
            Some(&yaml),
            0,
            K,
        ),
        (claude, &[("context_compact_percent", "80")], None, 80, K),
        (claude, &[], Some(&yaml), 70, Y),
        (claude, &[], None, 60, D),
        (
            claude,
            &[("context_compact_percent", "80")],
            Some(&claude_yaml),
            50,
            Y,
        ),
        (
            claude,
            &[("context_compact_percent_claude", "55")],
            Some(&yaml),
            55,
            K,
        ),
        (&CONTEXT_COMPACT_PERCENT_CODEX, &[], Some(&yaml), 100, D),
        (&CONTEXT_COMPACT_LOWER_BOUND_TOKENS, &[], None, 300_000, D),
    ];
    for (key, rows, config, value, source) in cases {
        let rows = rows
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        assert_eq!(
            resolve_row(key, &rows, config),
            Resolved { value, source },
            "{} {rows:?}",
            key.kv_key
        );
    }
}

/// One key-table row for the generated docs and the census.
struct KeyRow {
    kv_key: &'static str,
    /// An individual `kv_meta` row rather than a `runtime-config` field.
    row_key: bool,
    yaml_field: &'static str,
    default: String,
    rule: &'static str,
    /// Whether a non-default `yaml_sample` at `yaml_field` reaches the resolver as YAML.
    yaml_is_read: bool,
}

fn yaml_sample(yaml_field: &str, yaml_sample: &str) -> Config {
    let field = yaml_field.strip_prefix("runtime.").unwrap();
    let mut config = Config::default();
    config.runtime = serde_yaml::from_str(&format!("{field}: {yaml_sample}\n")).unwrap();
    config
}

/// `yaml_field` is checked against the key's accessor, so the docs cannot name another field.
fn row<T: Copy + PartialEq + std::fmt::Display>(
    key: &RuntimeSetting<T>,
    yaml_field: &'static str,
    yaml_sample: &str,
) -> KeyRow {
    let config = self::yaml_sample(yaml_field, yaml_sample);
    KeyRow {
        kv_key: key.kv_key,
        row_key: false,
        yaml_field,
        default: format!("`{}`", key.default),
        rule: match key.authority {
            KvAuthority::AnyValue => "it has the expected JSON type",
            KvAuthority::ExplicitOrNonDefault => {
                "it is listed in `__runtimeConfigExplicitKeys` or differs from the default"
            }
        },
        yaml_is_read: resolve(key, None, Some(&config)).source == Y,
    }
}

fn row_key(key: &RowSetting, yaml_field: &'static str, yaml_sample: &str) -> KeyRow {
    let config = self::yaml_sample(yaml_field, yaml_sample);
    KeyRow {
        kv_key: key.kv_key,
        row_key: true,
        yaml_field,
        default: match key.inherits {
            Some(parent) => format!("the `{}` value", parent.kv_key),
            None => format!("`{}`", key.default),
        },
        rule: match key.floor {
            0 => "it parses as a non-negative integer",
            _ => "it parses as a non-negative integer; every layer is raised to at least 1",
        },
        yaml_is_read: resolve_row(key, &HashMap::new(), Some(&config)).source == Y,
    }
}

fn key_rows() -> [KeyRow; 10] {
    [
        row(
            &DISPATCH_GATE_ENABLED,
            "runtime.dispatch_rate_limit_gate_enabled",
            "false",
        ),
        row(
            &DISPATCH_GATE_DANGER_PCT,
            "runtime.dispatch_rate_limit_gate_danger_pct",
            "90",
        ),
        row(&RATE_LIMIT_STALE_SEC, "runtime.rate_limit_stale_sec", "900"),
        row(&MAX_ENTRY_RETRIES, "runtime.max_entry_retries", "5"),
        row_key(
            &REQUESTED_TIMEOUT_MIN,
            "runtime.requested_timeout_min",
            "30",
        ),
        row_key(
            &IN_PROGRESS_STALE_MIN,
            "runtime.in_progress_stale_min",
            "90",
        ),
        row_key(
            &CONTEXT_COMPACT_PERCENT,
            "runtime.context_compact_percent",
            "70",
        ),
        row_key(
            &CONTEXT_COMPACT_PERCENT_CODEX,
            "runtime.context_compact_percent_codex",
            "90",
        ),
        row_key(
            &CONTEXT_COMPACT_PERCENT_CLAUDE,
            "runtime.context_compact_percent_claude",
            "50",
        ),
        row_key(
            &CONTEXT_COMPACT_LOWER_BOUND_TOKENS,
            "runtime.context_compact_lower_bound_tokens",
            "200000",
        ),
    ]
}

#[test]
fn key_table_yaml_fields_name_the_fields_the_resolver_reads() {
    for row in key_rows() {
        assert!(
            row.yaml_is_read,
            "{} does not read {}",
            row.kv_key, row.yaml_field
        );
    }
}

const DOC_BEGIN: &str = "<!-- BEGIN GENERATED: runtime setting precedence -->";
const DOC_END: &str = "<!-- END GENERATED: runtime setting precedence -->";

fn render_precedence_table() -> String {
    let mut out = format!(
        "{DOC_BEGIN}\n\
         <!-- Generated from the key table in src/services/settings/resolve; regenerate: \
         ADK_WRITE_SETTINGS_PRECEDENCE_DOC=1 cargo test --lib services::settings::resolve:: -->\n\n\
         Rust readers and policy `agentdesk.config.get` resolve these keys with \
         `settings::resolve`: the persisted value when it outranks YAML, else the node's YAML \
         snapshot, else the default. An unreadable kv layer falls through to YAML and the \
         default.\n\n\
         | Persisted value | YAML field | Default | Persisted value outranks YAML when |\n\
         |---|---|---|---|\n"
    );
    for row in key_rows() {
        let persisted = match row.row_key {
            true => "`kv_meta` row",
            false => "`runtime-config` field",
        };
        out.push_str(&format!(
            "| {persisted} `{}` | `{}` | {} | {} |\n",
            row.kv_key, row.yaml_field, row.default, row.rule
        ));
    }
    out.push_str(DOC_END);
    out
}

#[test]
fn precedence_tables_in_readme_and_adr_match_the_key_table() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rendered = render_precedence_table();
    let write = std::env::var_os("ADK_WRITE_SETTINGS_PRECEDENCE_DOC").is_some();
    for doc in ["README.md", "docs/adr-settings-precedence.md"] {
        let path = root.join(doc);
        let text = std::fs::read_to_string(&path).unwrap();
        let (Some(begin), Some(end)) = (text.find(DOC_BEGIN), text.find(DOC_END)) else {
            panic!("{doc} has no generated precedence block");
        };
        assert!(
            begin < end,
            "{doc}: generated block markers are out of order"
        );
        let block = &text[begin..end + DOC_END.len()];
        if write && block != rendered {
            std::fs::write(&path, text.replacen(block, &rendered, 1)).unwrap();
            continue;
        }
        assert!(
            block == rendered,
            "{doc} precedence table is stale; regenerate with \
             ADK_WRITE_SETTINGS_PRECEDENCE_DOC=1 cargo test --lib services::settings::resolve::"
        );
    }
}

fn workspace_relative(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

/// Production reads of these keys' kv and YAML layers stay in the resolver; a caller
/// that re-reads `runtime-config` or the YAML field would reintroduce its own precedence.
#[test]
fn runtime_setting_layers_are_read_only_through_resolve() {
    let owners = [
        "src/services/settings/resolve.rs",
        // Seeds the persisted blob and builds the runtime-config GET defaults.
        "src/services/settings.rs",
        "src/config/runtime_settings.rs",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_production_sources(&root.join("src"), &mut files);
    assert!(files.len() > 100, "source walk found {} files", files.len());

    let mut hits = Vec::new();
    for path in files {
        let relative = workspace_relative(root, &path);
        if owners.contains(&relative.as_str()) {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let (text, code) = production_view(&source, &relative);
        for row in key_rows() {
            let field = row.yaml_field.strip_prefix("runtime.").unwrap();
            // Row-key fields share names with turn context structs, so only `runtime.` reads count.
            let mut needles = vec![
                (&text, format!(".get(\"{}\")", row.kv_key)),
                (&text, format!("[\"{}\"]", row.kv_key)),
            ];
            if row.row_key {
                needles.push((&code, format!("runtime.{field}")));
                needles.push((&text, format!(".bind(\"{}\")", row.kv_key)));
            } else {
                needles.push((&code, format!(".{field}")));
            }
            for (view, needle) in needles {
                for (offset, _) in view.match_indices(needle.as_str()) {
                    let next = view[offset + needle.len()..].chars().next();
                    // `["key"]` is an index only after an expression, not inside an array literal.
                    let indexed = view[..offset]
                        .trim_end()
                        .chars()
                        .last()
                        .is_some_and(|c| c.is_alphanumeric() || "_)]".contains(c));
                    if next.is_some_and(|c| c.is_alphanumeric() || c == '_')
                        || (needle.starts_with('[') && !indexed)
                    {
                        continue;
                    }
                    let line = view[..offset].matches('\n').count() + 1;
                    hits.push(format!("{relative}:{line} {needle}"));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "read these through crate::services::settings::resolve: {hits:#?}"
    );
}

/// Each migrated reader and the resolver calls it must keep; a reader that drops them
/// would re-derive precedence in a form the read census above cannot see.
const RESOLVER_BINDINGS: &[(&str, &str, &[&str])] = &[
    (
        "src/services/auto_queue/planning.rs",
        "fn effective_max_entry_retries(",
        &["settings::max_entry_retries("],
    ),
    (
        "src/services/discord/turn_bridge/completion_guard/completion_postgres.rs",
        "fn runtime_max_entry_retries_pg(",
        &["settings::max_entry_retries("],
    ),
    (
        "src/services/settings/resolve.rs",
        "fn max_entry_retries(",
        &["resolve(&MAX_ENTRY_RETRIES,"],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn live_setting<",
        &["settings::resolve_with_kv(key,kv,"],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn stale_sec(",
        &["live_setting(&settings::RATE_LIMIT_STALE_SEC,"],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn persisted_runtime_overrides(",
        &[
            "settings::DISPATCH_GATE_ENABLED.kv_override(",
            "settings::DISPATCH_GATE_DANGER_PCT.kv_override(",
            "settings::RATE_LIMIT_STALE_SEC.kv_override(",
        ],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn evaluate_agent_provider_pressure_with_overrides(",
        &[
            "live_setting(&settings::DISPATCH_GATE_ENABLED,enabled_override)",
            "live_setting(&settings::DISPATCH_GATE_DANGER_PCT,danger_override)",
            "live_setting(&settings::RATE_LIMIT_STALE_SEC,stale_override)",
        ],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn effective_danger_pct_pg(",
        &["live_setting(&settings::DISPATCH_GATE_DANGER_PCT,"],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn effective_stale_sec_pg(",
        &["live_setting(&settings::RATE_LIMIT_STALE_SEC,"],
    ),
    (
        "src/services/dispatch_gate.rs",
        "fn diagnostics_with_overrides(",
        &[
            "settings::resolve_with_kv(&settings::DISPATCH_GATE_ENABLED,enabled_override,",
            "settings::resolve_with_kv(&settings::DISPATCH_GATE_DANGER_PCT,danger_override,",
        ],
    ),
    (
        "src/services/dispatch_gate/auth_profiles.rs",
        "fn selection_pressure_policy(",
        &[
            "live_setting(&settings::DISPATCH_GATE_ENABLED,enabled)",
            "live_setting(&settings::DISPATCH_GATE_DANGER_PCT,danger)",
            "live_setting(&settings::RATE_LIMIT_STALE_SEC,stale)",
        ],
    ),
    (
        "src/services/auto_queue/activate_command.rs",
        "fn activate_with_deps_pg(",
        &[
            "dispatch_gate::persisted_runtime_overrides(",
            "dispatch_gate::evaluate_agent_provider_pressure_with_overrides(",
        ],
    ),
    (
        "src/services/health_diagnostics/request_batch.rs",
        "fn dispatch_gate_overrides(",
        &["dispatch_gate::persisted_runtime_overrides("],
    ),
    (
        "src/engine/ops/config_ops.rs",
        "fn resolve_config_get_raw(",
        &["settings::runtime_config_field("],
    ),
    (
        "src/engine/ops/config_ops.rs",
        "fn select<",
        &[
            "settings::policy_kv_rows(key)",
            "resolved_config_value(key,&rows)",
        ],
    ),
    (
        "src/engine/ops/config_ops.rs",
        "fn config_get_without_kv(",
        &["resolved_config_value(key,"],
    ),
    (
        "src/engine/ops/config_ops.rs",
        "fn resolved_config_value(",
        &["settings::policy_value(key,rows,live.as_deref())"],
    ),
    (
        "src/services/discord/adk_session.rs",
        "fn fetch_context_thresholds(",
        &["resolve_context_thresholds(&read,live.as_deref(),"],
    ),
    (
        "src/services/discord/adk_session.rs",
        "fn resolve_context_thresholds(",
        &["settings::resolve_row(key,rows,yaml)"],
    ),
    (
        "src/services/settings/resolve.rs",
        "fn resolve_with_kv<",
        &["layered("],
    ),
    (
        "src/services/settings/resolve.rs",
        "fn resolve_row(",
        &["layered(kv,yaml_value,key.default)"],
    ),
];

#[test]
fn migrated_readers_keep_their_resolver_calls() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut missing = Vec::new();
    for (file, signature, calls) in RESOLVER_BINDINGS {
        let source = std::fs::read_to_string(root.join(file)).unwrap();
        let (_, code) = production_view(&source, file);
        let Some(span) = crate::test_support::braced_item_span(&code, signature) else {
            missing.push(format!("{file}: production `{signature}` not found"));
            continue;
        };
        let body: String = code[span].split_whitespace().collect();
        for call in *calls {
            if !body.contains(call) {
                missing.push(format!("{file} `{signature}` no longer calls `{call}`"));
            }
        }
    }
    assert!(missing.is_empty(), "resolver bindings broken: {missing:#?}");
}

fn collect_production_sources(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "tests" {
                collect_production_sources(&path, files);
            }
        } else if name.ends_with(".rs")
            && name != "tests.rs"
            && !name.ends_with("_tests.rs")
            && !name.ends_with("_test.rs")
        {
            files.push(path);
        }
    }
}

/// Comment-free text and code mask with every `#[cfg(test)]` item or statement blanked out.
fn production_view(source: &str, path: &str) -> (String, String) {
    let text = crate::test_support::strip_rust_comments(source)
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    let code = crate::test_support::rust_code_mask(source)
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    let (mut text, mut masked) = (text.into_bytes(), code.clone().into_bytes());
    let mut from = 0;
    while let Some(offset) = code[from..].find("#[cfg(test)]") {
        let start = from + offset;
        let end = cfg_test_item_end(&code, start)
            .unwrap_or_else(|| panic!("{path}: unparsable #[cfg(test)] form at byte {start}"));
        for index in start..end {
            if text[index] != b'\n' {
                text[index] = b' ';
                masked[index] = b' ';
            }
        }
        from = end;
    }
    (
        String::from_utf8_lossy(&text).into_owned(),
        String::from_utf8_lossy(&masked).into_owned(),
    )
}

/// End of the item, statement, field or arm a `#[cfg(test)]` at `start` gates: its `;` or `,`,
/// the brace that closes a block not continued by an operator, or the enclosing close.
fn cfg_test_item_end(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut index = start;
    // Skip this and any further outer attributes.
    while code[index..].trim_start().starts_with("#[") {
        index += code[index..].find("#[")? + 1;
        let mut depth = 0usize;
        loop {
            match *bytes.get(index)? {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        index += 1;
    }
    let (mut depth, mut angle) = (0usize, 0usize);
    while let Some(&byte) = bytes.get(index) {
        let prev = index.checked_sub(1).map(|i| bytes[i]);
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => return Some(index),
            b')' | b']' => depth -= 1,
            b'}' => {
                depth -= 1;
                let rest = code[index + 1..].trim_start();
                let continued = rest.starts_with(|c: char| ".?;,=+-*/%&|^<>!".contains(c))
                    || ["else", "as "].iter().any(|word| rest.starts_with(word));
                if depth == 0 && !continued {
                    return Some(index + 1);
                }
            }
            b'<' if depth == 0 => angle += 1,
            b'>' if depth == 0 && !matches!(prev, Some(b'-' | b'=')) => {
                angle = angle.saturating_sub(1);
            }
            b';' if depth == 0 => return Some(index + 1),
            b',' if depth == 0 && angle == 0 => return Some(index + 1),
            _ => {}
        }
        index += 1;
    }
    None
}

/// Test-only statements are blanked like items, and a form the census cannot bound fails closed.
#[test]
fn production_view_blanks_test_only_statements_and_rejects_unbounded_forms() {
    let source = "fn f() -> i64 {\n    #[cfg(test)]\n    let _ = settings::max_entry_retries(a, b);\n    \
                  #[cfg(test)] pub use x::y;\n    read(\"k\")\n}\n#[cfg(test)]\nstatic M: Map<K, Vec<V>> = m();\n\
                  struct S { #[cfg(test)] a: Vec<(u8, u8)>, b: u8 }\n";
    let (_, code) = production_view(source, "fixture.rs");
    assert_eq!(
        code.split_whitespace().collect::<Vec<_>>(),
        [
            "fn", "f()", "->", "i64", "{", "read(", ")", "}", "struct", "S", "{", "b:", "u8", "}"
        ]
    );
    let unbounded = std::panic::catch_unwind(|| production_view("#[cfg(test)]\nlet x = 1", "x.rs"));
    assert!(
        unbounded.is_err(),
        "an unterminated #[cfg(test)] form must fail closed"
    );
}

#[test]
fn env_flag_value_and_presence_share_one_snapshot() {
    const NAME: &str = "ADK_SETTINGS_ENV_FLAG_TEST";
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _restore = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock(NAME);
    for default in [false, true] {
        unsafe { std::env::remove_var(NAME) };
        assert_eq!(
            resolve_env_flag(NAME, default),
            Resolved {
                value: default,
                source: SettingSource::Default
            }
        );
        for (raw, expected) in [
            ("", default),
            ("invalid", default),
            (" YeS ", true),
            ("Off", false),
        ] {
            unsafe { std::env::set_var(NAME, raw) };
            assert_eq!(
                resolve_env_flag(NAME, default),
                Resolved {
                    value: expected,
                    source: SettingSource::EnvOverride
                }
            );
        }
    }
}
