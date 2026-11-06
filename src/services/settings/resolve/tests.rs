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
