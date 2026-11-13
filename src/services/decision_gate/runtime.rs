use super::policy;
use super::types::*;
use crate::config::decision_gate::DecisionGateConfig;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

mod gemini;
pub(super) mod key;
mod parser;
mod request;
pub(crate) use key::{KeyLookupOutcome, SecretString};
pub(super) use parser::parse_response;

pub(super) type PolicySource = Arc<dyn Fn() -> Option<DecisionGateConfig> + Send + Sync>;
static INSTALLED: OnceLock<Arc<DecisionGate>> = OnceLock::new();

pub(crate) struct DecisionGate {
    source: PolicySource,
    key: Option<SecretString>,
    client: Option<reqwest::Client>,
    endpoint: String,
}
impl std::fmt::Debug for DecisionGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionGate")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl DecisionGate {
    pub(super) fn new(source: PolicySource, key: Option<SecretString>) -> Self {
        Self {
            source,
            key,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .connect_timeout(Duration::from_secs(1))
                .build()
                .ok(),
            endpoint: gemini::ENDPOINT.into(),
        }
    }

    pub(crate) fn availability(&self, scope: &Scope) -> Result<Duration, OffReason> {
        self.policy_for(scope)
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        config: DecisionGateConfig,
        key: Option<&str>,
        endpoint: String,
    ) -> Self {
        let mut gate = Self::new(
            Arc::new(move || Some(config.clone())),
            key.and_then(|value| SecretString::new(value.into())),
        );
        gate.endpoint = endpoint;
        gate
    }

    fn policy_for(&self, scope: &Scope) -> Result<Duration, OffReason> {
        if self.key.is_none() {
            return Err(OffReason::MissingKey);
        }
        let config = (self.source)().ok_or(OffReason::PolicyUnavailable)?;
        policy::resolve(&config, scope)
    }

    pub(crate) async fn decide(&self, request: &DecisionRequest) -> DecisionAttempt {
        let start = Instant::now();
        let result = self.attempt(request, start).await;
        let (result, input_tokens) = match result {
            Ok(value) => value,
            Err(reason) => (DecisionResult::NoAnswer(reason), None),
        };
        DecisionAttempt {
            result,
            observation: DecisionObservation {
                template_version: request.template_version.clone(),
                elapsed_ms: start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                input_tokens,
            },
        }
    }

    async fn attempt(
        &self,
        request: &DecisionRequest,
        start: Instant,
    ) -> Result<(DecisionResult, Option<u64>), NoAnswerReason> {
        let budget = match self.policy_for(&request.scope) {
            Ok(budget) => budget,
            Err(reason) => return Ok((DecisionResult::Off(reason), None)),
        };
        let body = request::body(request)?;
        let deadline = start + budget;
        let result = tokio::time::timeout_at(deadline, self.send(request, body, budget))
            .await
            .map_err(|_| NoAnswerReason::Timeout)?;
        if Instant::now() >= deadline {
            return Err(NoAnswerReason::Timeout);
        }
        if let Err(reason) = self.policy_for(&request.scope) {
            return Ok((DecisionResult::Off(reason), None));
        }
        result
    }
}

pub(super) fn install(gate: Arc<DecisionGate>, outcome: KeyLookupOutcome) -> bool {
    if INSTALLED.set(gate).is_err() {
        return false;
    }
    tracing::info!(outcome = outcome.boot_label(), "Decision gate availability");
    true
}

pub(crate) fn installed() -> Option<&'static Arc<DecisionGate>> {
    INSTALLED.get()
}

#[cfg(test)]
mod tests;
