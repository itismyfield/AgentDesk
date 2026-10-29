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

/// Production reads of these keys' kv and YAML layers stay in the resolver; a caller
/// that re-reads `runtime-config` or the YAML field would reintroduce its own precedence.
#[test]
fn runtime_setting_layers_are_read_only_through_resolve() {
    let layers = [
        (
            DISPATCH_GATE_ENABLED.kv_key,
            "dispatch_rate_limit_gate_enabled",
        ),
        (
            DISPATCH_GATE_DANGER_PCT.kv_key,
            "dispatch_rate_limit_gate_danger_pct",
        ),
        (RATE_LIMIT_STALE_SEC.kv_key, "rate_limit_stale_sec"),
        (MAX_ENTRY_RETRIES.kv_key, "max_entry_retries"),
    ];
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
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if owners.contains(&relative.as_str()) {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let (text, code) = production_view(&source, &relative);
        for (kv_key, field) in layers {
            let kv_read = format!(".get(\"{kv_key}\")");
            let field_read = format!(".{field}");
            for (view, needle) in [(&text, &kv_read), (&code, &field_read)] {
                for (offset, _) in view.match_indices(needle.as_str()) {
                    let next = view[offset + needle.len()..].chars().next();
                    if next.is_some_and(|c| c.is_alphanumeric() || c == '_') {
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
