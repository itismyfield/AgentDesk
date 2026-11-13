mod policy;
mod runtime;
mod types;

pub(crate) use runtime::key::capture_initial_key;
pub(crate) use runtime::{DecisionGate, KeyLookupOutcome, SecretString, installed};
pub(crate) use types::{
    DecisionRequest, DecisionResult, Feature, NoAnswerReason, OffReason, Question, Scope,
};

pub(crate) async fn initialize(
    captured: Option<SecretString>,
    cancel: &tokio_util::sync::CancellationToken,
) -> KeyLookupOutcome {
    let (key, outcome) = runtime::key::load(
        captured,
        &mut tokio::process::Command::new("agent-op"),
        cancel,
    )
    .await;
    if outcome != KeyLookupOutcome::Cancelled {
        let source = std::sync::Arc::new(|| {
            crate::config::snapshot().map(|config| config.decision_gate.clone())
        });
        runtime::install(std::sync::Arc::new(DecisionGate::new(source, key)), outcome);
    }
    outcome
}

#[cfg(test)]
mod tests;
