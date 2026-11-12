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
                "/examples/decision_gate_acceptance/capture_parser.rs"
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

mod acceptance_sample_leaks {
    use regex::Regex;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeSet;

    fn fixture() -> Value {
        serde_json::from_str(include_str!(
            "fixtures/decision_gate/acceptance_samples.json"
        ))
        .expect("fixed acceptance samples")
    }

    fn assert_shortcut_bounded(name: &str, predictions: &[&str], fixture: &Value) {
        let samples = fixture["samples"].as_array().expect("samples");
        assert_eq!(predictions.len(), samples.len());
        let correct = predictions
            .iter()
            .zip(samples)
            .filter(|(prediction, sample)| Some(**prediction) == sample["expected"].as_str())
            .count();
        println!(
            "{name}: correct={correct}/{} accuracy={:.6}",
            samples.len(),
            correct as f64 / samples.len() as f64
        );
        assert!(predictions.contains(&"duplicate"));
        assert!(predictions.contains(&"new"));
        assert!(correct * 4 <= samples.len() * 3);
    }

    #[test]
    fn review_normal_path_regex_is_not_a_label_shortcut() {
        let fixture = fixture();
        let rule = Regex::new("정상|무관|관계없이|보존").expect("fixed review rule");
        let predictions: Vec<_> = fixture["samples"]
            .as_array()
            .expect("samples")
            .iter()
            .map(|sample| {
                if rule.is_match(sample["background"].as_str().expect("background")) {
                    "new"
                } else {
                    "duplicate"
                }
            })
            .collect();
        assert_shortcut_bounded("normal_path_regex", &predictions, &fixture);
    }

    fn tokens(rule: &Regex, text: &str) -> BTreeSet<String> {
        rule.find_iter(&text.to_lowercase())
            .map(|word| word.as_str().to_owned())
            .collect()
    }

    #[test]
    fn candidate_word_overlap_is_not_a_label_shortcut() {
        let fixture = fixture();
        let words = Regex::new("[a-z0-9_]+|[가-힣]+").expect("fixed token rule");
        let predictions: Vec<_> = fixture["samples"]
            .as_array()
            .expect("samples")
            .iter()
            .map(|sample| {
                let input = format!(
                    "{} {}",
                    sample["title"].as_str().expect("title"),
                    sample["background"].as_str().expect("background")
                );
                let query = tokens(&words, &input);
                let max_overlap = fixture["issue_sets"][sample["issue_set"].as_str().expect("set")]
                    .as_array()
                    .expect("candidates")
                    .iter()
                    .map(|issue| {
                        let candidate = tokens(&words, issue["title"].as_str().expect("title"));
                        query.intersection(&candidate).count() as f64
                            / query.union(&candidate).count() as f64
                    })
                    .fold(0.0, f64::max);
                // Fixed before sample edits: one-fifth Jaccard, without stemming or label fitting.
                if max_overlap >= 0.20 {
                    "duplicate"
                } else {
                    "new"
                }
            })
            .collect();
        assert_shortcut_bounded("candidate_word_jaccard_ge_0.20", &predictions, &fixture);
    }

    fn sort_keys(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.values_mut().for_each(sort_keys);
                fields.sort_keys();
            }
            Value::Array(values) => values.iter_mut().for_each(sort_keys),
            _ => {}
        }
    }

    #[test]
    fn labels_ids_keys_and_candidate_sets_are_unchanged() {
        let fixture = fixture();
        let labels: Vec<_> = fixture["samples"]
            .as_array()
            .expect("samples")
            .iter()
            .map(|sample| {
                json!({
                    "sample_id": sample["sample_id"],
                    "expected": sample["expected"],
                    "key": sample["key"],
                    "issue_set": sample["issue_set"]
                })
            })
            .collect();
        let mut contract = json!({"issue_sets": fixture["issue_sets"], "labels": labels});
        sort_keys(&mut contract);
        let serialized = serde_json::to_vec(&contract).expect("canonical contract");
        assert_eq!(
            format!("{:x}", Sha256::digest(serialized)),
            "b3a696a62e6dbdbf20a661b7a40ea6c17ad23fc41bafd7bcf77d3bc7ed092f87"
        );
    }
}
