//! The async owner runs blocking file publication and the bounded full checker off the gate.

use std::sync::Arc;

use super::super::{PeerSource, apply, checker};
use super::Preview;
use super::preview::Prepared;

pub(crate) async fn apply(
    prepared: Prepared,
    expected: &Preview,
    source: Arc<dyn PeerSource>,
) -> Result<apply::Applied, apply::ApplyFailure> {
    let refused = |detail: String| apply::ApplyFailure {
        stage: apply::Stage::Preview,
        published: false,
        backup: None,
        detail,
    };
    if prepared.token != *expected {
        return Err(refused("consent preview changed".into()));
    }
    tokio::task::spawn_blocking(move || {
        prepared.peer_unchanged(source.as_ref()).map_err(refused)?;
        prepared.plan.unchanged().map_err(refused)?;
        let check = checker::FullCandidate::new(&prepared.peer, source.as_ref());
        let result = apply::apply(&prepared.plan, &check)?;
        prepared
            .peer_unchanged(source.as_ref())
            .map_err(|detail| apply::ApplyFailure {
                stage: apply::Stage::PostCheck,
                published: prepared.plan.requires_write(),
                backup: result.backup.clone(),
                detail,
            })?;
        Ok(result)
    })
    .await
    .map_err(|e| refused(format!("apply owner: {e}")))?
}

#[cfg(test)]
#[path = "apply_owner_tests.rs"]
mod tests;
