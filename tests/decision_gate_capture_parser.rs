use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const MODEL: &str = "gemini-decision-1";
type Result<T> = std::result::Result<T, &'static str>;

pub mod services {
    pub mod decision_gate {
        pub mod capture_parser {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/decision_gate_capture_parser/mod.rs"
            ));
        }
        pub use capture_parser::types;
        pub(crate) use types::Question;
        pub mod runtime {
            mod request {
                include!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/services/decision_gate/runtime/request.rs"
                ));
            }
            pub(crate) fn choice_body(question: super::Question) -> serde_json::Value {
                use super::types::*;
                request::body(&DecisionRequest {
                    scope: Scope {
                        feature: Feature::IssueDuplicate,
                        repository: Some("ADK".into()),
                    },
                    template_version: "raw_contract_v1".into(),
                    input_text: "Synthetic capture".into(),
                    question,
                })
                .expect("fixed request")
            }
        }
    }
}

mod transport {
    use super::*;
    pub(super) struct Relay {
        pub(super) response: Value,
    }
    pub(super) struct Record {
        pub(super) response: Option<Value>,
        pub(super) raw_body: Option<String>,
        pub(super) status: u16,
    }
    impl Relay {
        pub(super) async fn post(&self, body: Value, _: Duration) -> Result<Record> {
            if body["questions"].as_object().map(|q| q.len()) != Some(4) {
                return Err("not_four_questions");
            }
            Ok(Record {
                response: Some(self.response.clone()),
                raw_body: Some(self.response.to_string()),
                status: 200,
            })
        }
    }
}

#[path = "../examples/decision_gate_acceptance/capture.rs"]
mod capture;
fn check_safe(_: &str) -> Result<()> {
    Ok(())
}
fn write_safe(out: &Path, name: &str, text: &str) -> Result<()> {
    std::fs::write(out.join(name), text).map_err(|_| "test_write_failed")
}
fn write_json(out: &Path, name: &str, value: &Value) -> Result<()> {
    write_safe(out, name, &value.to_string())
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/decision_gate/comment_shape.json"))
        .expect("fixed fixture")
}
fn wire(value: &Value) -> Value {
    json!({"answers": {
        "q_noul":value["noul"]["answers"]["q"],
        "q_choice_current":value["choice"]["answers"]["q"],
        "q_choice_255":value["choice_255"]["answers"]["q"],
        "q_score":value["score"]["answers"]["q"]
    }})
}
fn invalid_views() -> Vec<Value> {
    let mut cases = Vec::new();
    for name in ["noul", "choice", "choice_255", "score"] {
        let mut value = fixture();
        value[name]["answers"]["q"] = json!({"type":value[name]["answers"]["q"]["type"]});
        cases.push(value);
        let mut value = fixture();
        value[name]["answers"]["q"]
            .as_object_mut()
            .expect("answer")
            .remove("confidence");
        cases.push(value);
    }
    for (name, key) in [("choice", "host-a"), ("choice_255", "i000"), ("score", "0")] {
        let mut value = fixture();
        value[name]["answers"]["q"]["probabilities"]
            .as_object_mut()
            .expect("probabilities")
            .remove(key);
        cases.push(value);
        let mut value = fixture();
        for probability in value[name]["answers"]["q"]["probabilities"]
            .as_object_mut()
            .expect("probabilities")
            .values_mut()
        {
            *probability = json!(0.0);
        }
        cases.push(value);
    }
    let mut value = fixture();
    value["noul"]["answers"]["q"]["noul"] = json!(1.5);
    cases.push(value);
    cases
}

#[test]
fn common_parser_accepts_all_four_views() {
    assert_eq!(
        services::decision_gate::capture_parser::validate_views(&fixture()),
        Ok(())
    );
}
#[test]
fn common_parser_rejects_incomplete_and_invalid_views() {
    for value in invalid_views() {
        assert_eq!(
            services::decision_gate::capture_parser::validate_views(&value),
            Err("capture_parser_incompatible")
        );
    }
}
#[tokio::test]
async fn capture_rejects_invalid_views_before_compatibility_manifest() {
    for value in invalid_views() {
        let out = tempfile::tempdir().expect("test directory");
        let relay = transport::Relay {
            response: wire(&value),
        };
        assert_eq!(
            capture::capture(out.path(), &relay, true).await,
            Err("capture_parser_incompatible")
        );
        assert!(!out.path().join("capture-manifest.json").exists());
        assert!(!out.path().join("comment_shape.json").exists());
    }
}
#[tokio::test]
async fn valid_capture_marks_common_parser_compatibility() {
    let out = tempfile::tempdir().expect("test directory");
    let relay = transport::Relay {
        response: wire(&fixture()),
    };
    capture::capture(out.path(), &relay, true)
        .await
        .expect("valid capture");
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(out.path().join("capture-manifest.json")).expect("manifest"),
    )
    .expect("json");
    assert_eq!(manifest["parser_compatible"], true);
    assert!(manifest["fixture_mapping"].is_string());
}
