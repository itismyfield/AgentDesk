// Compile the production boundary unchanged; no installed runtime or CLI is used.
pub(crate) mod config {
    pub(crate) mod decision_gate {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/config/decision_gate.rs"
        ));
    }
}

pub(crate) mod utils {
    pub(crate) mod redact {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/utils/redact.rs"));
    }
}

pub(crate) mod github {
    pub(crate) async fn run_async(
        _: Vec<String>,
        _: std::time::Duration,
        _: String,
    ) -> std::result::Result<String, String> {
        Err("live_github_forbidden".into())
    }
}

pub(crate) mod services {
    pub(crate) mod decision_gate {
        pub(crate) mod types {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/services/decision_gate/types.rs"
            ));
        }
        mod policy {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/services/decision_gate/policy.rs"
            ));
        }
        pub(crate) mod runtime {
            const _: () = {
                let original = include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/src/services/decision_gate/runtime.rs"
                ));
                let snapshot = include_bytes!("runtime/mod.rs");
                assert!(original.len() == snapshot.len(), "run sync_runtime.py");
                let mut index = 0;
                while index < original.len() {
                    assert!(original[index] == snapshot[index], "run sync_runtime.py");
                    index += 1;
                }
            };

            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/decision_gate_acceptance/runtime/mod.rs"
            ));

            pub(crate) fn acceptance_gate(key: &str, endpoint: Option<String>) -> DecisionGate {
                let config = DecisionGateConfig::default();
                config.validate().expect("fixed policy");
                let mut gate = DecisionGate::new(
                    Arc::new(move || Some(config.clone())),
                    SecretString::new(key.to_string()),
                );
                if let Some(endpoint) = endpoint {
                    gate.endpoint = endpoint;
                    gate.client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .retry(reqwest::retry::never())
                        .no_proxy()
                        .connect_timeout(Duration::from_secs(1))
                        .build()
                        .ok();
                }
                // Keep boot symbols linked without invoking key lookup or installation.
                let _ = (installed, install, key::load, key::capture_initial_key);
                let _ = (
                    crate::utils::redact::register_common_env_secrets,
                    crate::utils::redact::register_secret_or_dsn,
                    crate::utils::redact::serialize_json_with_redacted_cookie_headers,
                    crate::utils::redact::redact_known_secrets,
                    crate::issue_duplicate::check,
                );
                gate
            }

            pub(crate) fn choice_body(question: super::types::Question) -> serde_json::Value {
                request::body(&DecisionRequest {
                    scope: Scope { feature: Feature::IssueDuplicate, repository: Some("ADK".into()) },
                    template_version: "raw_contract_v1".into(),
                    input_text: "Synthetic contract capture. Host A is idle, Host B is full. No duplicate synthetic issue. Urgency medium.".into(),
                    question,
                }).expect("fixed capture question")
            }
            pub(crate) fn request_body(request: &DecisionRequest) -> Option<serde_json::Value> {
                request::body(request).ok()
            }
        }
        pub(crate) use types::{
            DecisionRequest, DecisionResult, Feature, NoAnswerReason, OffReason, Question, Scope,
        };

        pub(crate) struct Observed {
            pub(crate) observation: types::DecisionObservation,
            pub(crate) record: crate::transport::Record,
            pub(crate) deadline_ms: Option<u64>,
        }

        pub(crate) struct DecisionGate {
            inner: runtime::DecisionGate,
            observation: std::sync::Mutex<Option<Observed>>,
        }

        impl DecisionGate {
            pub(crate) fn new(key: &str, endpoint: Option<String>) -> Self {
                Self {
                    inner: runtime::acceptance_gate(key, endpoint),
                    observation: std::sync::Mutex::new(None),
                }
            }

            pub(crate) fn availability(
                &self,
                scope: &Scope,
            ) -> Result<std::time::Duration, OffReason> {
                self.inner.availability(scope)
            }

            pub(crate) async fn decide(&self, request: &DecisionRequest) -> types::DecisionAttempt {
                let deadline_ms = self
                    .inner
                    .availability(&request.scope)
                    .ok()
                    .map(|budget| budget.as_millis().min(u128::from(u64::MAX)) as u64);
                let attempt = self.inner.decide(request).await;
                let response = match &attempt.result {
                    types::DecisionResult::Answer {
                        choice: types::DecisionValue::Choice(choice),
                        confidence,
                        probabilities,
                    } => Some(serde_json::json!({"answers":{"q":{
                        "type":"choice", "choice":choice, "confidence":confidence, "probabilities":probabilities
                    }}})),
                    _ => None,
                };
                *self.observation.lock().expect("observation lock") = Some(Observed {
                    deadline_ms,
                    observation: types::DecisionObservation {
                        template_version: attempt.observation.template_version.clone(),
                        elapsed_ms: attempt.observation.elapsed_ms,
                        input_tokens: attempt.observation.input_tokens,
                    },
                    record: crate::transport::Record {
                        request: runtime::request_body(request).unwrap_or(serde_json::Value::Null),
                        response,
                        raw_body: None,
                        status: 0,
                    },
                });
                attempt
            }

            pub(crate) fn take_observation(&self) -> Option<Observed> {
                self.observation.lock().expect("observation lock").take()
            }
        }
    }
}
