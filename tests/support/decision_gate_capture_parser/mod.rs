use serde_json::Value;

pub mod types {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/decision_gate_capture_parser/generated/types.rs"
    ));
}
pub mod runtime {
    pub(super) mod parser {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/decision_gate_capture_parser/generated/parser.rs"
        ));
    }
}

fn enabled(source: &str, public: bool) -> String {
    let lines: Vec<_> = source
        .lines()
        .filter(|line| line.trim() != "#[cfg(test)]")
        .map(|line| {
            if public && line.trim_start().starts_with("pub(crate) ") {
                line.replacen("pub(crate)", "pub", 1)
            } else {
                line.to_owned()
            }
        })
        .collect();
    lines.join("\n") + "\n"
}

pub fn validate_views(fixture: &Value) -> Result<(), &'static str> {
    // Enable the service's test vocabulary without changing parser logic.
    if enabled(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/services/decision_gate/types.rs"
        )),
        true,
    ) != include_str!("generated/types.rs")
        || enabled(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/services/decision_gate/runtime/parser.rs"
            )),
            false,
        ) != include_str!("generated/parser.rs")
    {
        return Err("capture_parser_source_drift");
    }
    use types::Question;
    let cases = [
        (
            "noul",
            Question::Noul {
                instructions: "Work?".into(),
                true_description: None,
                false_description: None,
            },
        ),
        (
            "choice",
            Question::Choice {
                instructions: "Host?".into(),
                options: vec![
                    ("host-a".into(), "A".into()),
                    ("host-b".into(), "B".into()),
                    ("hold".into(), "Wait".into()),
                ],
            },
        ),
        (
            "choice_255",
            Question::Choice {
                instructions: "Duplicate?".into(),
                options: (0..254)
                    .map(|i| (format!("i{i:03}"), format!("Issue {i}")))
                    .chain([("none".into(), "None".into())])
                    .collect(),
            },
        ),
        (
            "score",
            Question::Score {
                instructions: "Urgency?".into(),
                levels: vec!["Low".into(), "Medium".into(), "High".into()],
            },
        ),
    ];
    for (name, question) in cases {
        if !matches!(
            runtime::parser::parse_response(&question, &fixture[name]),
            Ok(types::DecisionResult::Answer { .. })
        ) {
            return Err("capture_parser_incompatible");
        }
    }
    Ok(())
}
