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

/// One key-table row for the generated docs and the census.
struct KeyRow {
    kv_key: &'static str,
    yaml_field: &'static str,
    default: String,
    authority: KvAuthority,
    /// Whether a non-default `yaml_sample` at `yaml_field` reaches the resolver as YAML.
    yaml_is_read: bool,
}

/// `yaml_field` is checked against the key's accessor, so the docs cannot name another field.
fn row<T: Copy + PartialEq + std::fmt::Display>(
    key: &RuntimeSetting<T>,
    yaml_field: &'static str,
    yaml_sample: &str,
) -> KeyRow {
    let field = yaml_field.strip_prefix("runtime.").unwrap();
    let mut config = Config::default();
    config.runtime = serde_yaml::from_str(&format!("{field}: {yaml_sample}\n")).unwrap();
    KeyRow {
        kv_key: key.kv_key,
        yaml_field,
        default: key.default.to_string(),
        authority: key.authority,
        yaml_is_read: resolve(key, None, Some(&config)).source == Y,
    }
}

fn key_rows() -> [KeyRow; 4] {
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
         Rust readers resolve these `runtime-config` keys with `settings::resolve`: the persisted \
         value when it outranks YAML, else the node's YAML snapshot, else the default.\n\n\
         | `runtime-config` field | YAML field | Default | Persisted value outranks YAML when |\n\
         |---|---|---|---|\n"
    );
    for row in key_rows() {
        let rule = match row.authority {
            KvAuthority::AnyValue => "it has the expected JSON type",
            KvAuthority::ExplicitOrNonDefault => {
                "it is listed in `__runtimeConfigExplicitKeys` or differs from the default"
            }
        };
        out.push_str(&format!(
            "| `{}` | `{}` | `{}` | {rule} |\n",
            row.kv_key, row.yaml_field, row.default
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
            let field_read = format!(".{}", row.yaml_field.strip_prefix("runtime.").unwrap());
            let needles = [
                (&text, format!(".get(\"{}\")", row.kv_key)),
                (&text, format!("[\"{}\"]", row.kv_key)),
                (&code, field_read),
            ];
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

/// Comment-free text and code mask with every `#[cfg(test)]` item blanked out.
fn production_view(source: &str, path: &str) -> (String, String) {
    let text = crate::test_support::strip_rust_comments(source)
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    let code = crate::test_support::rust_code_mask(source)
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    let (mut text, mut masked) = (text.into_bytes(), code.clone().into_bytes());
    let mut from = 0;
    while let Some(offset) = code[from..].find("#[cfg(test)]") {
        let start = from + offset;
        match crate::test_support::braced_item_span(&code[start..], "#[cfg(test)]") {
            Some(span) => {
                for index in start + span.start..start + span.end {
                    if text[index] != b'\n' {
                        text[index] = b' ';
                        masked[index] = b' ';
                    }
                }
                from = start + span.end;
            }
            None => from = start + 1,
        }
    }
    (
        String::from_utf8_lossy(&text).into_owned(),
        String::from_utf8_lossy(&masked).into_owned(),
    )
}
