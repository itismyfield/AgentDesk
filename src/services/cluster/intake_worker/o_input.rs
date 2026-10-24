//! Claimed input keeps its SQL identity and home fence while the writer observes fresh eligibility.

#[cfg(test)]
use super::test_executor::execute_intake_turn_core;
use super::*;
#[cfg(not(test))]
use crate::services::discord::execute_intake_turn_core;
use crate::services::tui_o::cutover::intake_route::IntakeRoute;

pub(super) async fn hold_off_o_gateway(
    pool: &PgPool,
    row: &IntakeOutboxRow,
    claim_owner: &str,
    provider: &str,
    channel_id: ChannelId,
    target: &str,
    phase: &'static str,
) -> Result<bool, sqlx::Error> {
    let selected = intake_route::selected_codex_input(provider, channel_id.get());
    #[cfg(test)]
    if selected && phase == "final" {
        test_executor::checkpoint(test_executor::Checkpoint::PreAccept).await;
    }
    let exact = if selected {
        let current = sqlx::query_as::<_, (String, String, String, String, Option<String>)>(
            "SELECT provider, channel_id, target_instance_id, status, claim_owner
         FROM intake_outbox WHERE id=$1",
        )
        .bind(row.id)
        .fetch_optional(pool)
        .await?;
        row.provider == provider
            && row.channel_id == channel_id.get().to_string()
            && row.target_instance_id == target
            && row.status == IntakeOutboxStatus::Claimed
            && row.claim_owner.as_deref() == Some(claim_owner)
            && current.is_some_and(|(kind, channel, node, status, owner)| {
                kind == row.provider
                    && channel == row.channel_id
                    && node == row.target_instance_id
                    && status == "claimed"
                    && owner.as_deref() == Some(claim_owner)
            })
    } else {
        true
    };
    let detail = if !exact {
        Some("claimed input identity changed".to_owned())
    } else if let Some(detail) =
        super::super::channel_home::intake_hold(&row.channel_id, row.home_epoch)
    {
        Some(detail)
    } else {
        let route = if selected {
            intake_route::for_claimed_input(provider, channel_id.get(), row.id, phase).await
        } else {
            intake_route::route(provider, channel_id.get())
        };
        match route {
            IntakeRoute::Hold(detail) => Some(detail),
            IntakeRoute::Unselected | IntakeRoute::Gateway => None,
        }
    };
    let Some(detail) = detail else {
        return Ok(false);
    };
    let released = return_claimed_to_pending(pool, row.id, claim_owner).await?;
    tracing::warn!(
        row_id = row.id,
        channel_id = row.channel_id,
        released,
        "[intake_worker] O channel held before accept: {detail}"
    );
    Ok(true)
}

/// Issued only between the worker's spawned CAS and the first executor effects.
#[cfg(unix)]
pub(crate) struct BeforeEffects(IntakeOutboxRow);

#[cfg(unix)]
impl BeforeEffects {
    pub(crate) fn row(&self) -> &IntakeOutboxRow {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(row: &IntakeOutboxRow) -> Self {
        Self(row.clone())
    }
}

#[cfg(unix)]
tokio::task_local! {
    static BEFORE_EFFECTS: std::cell::RefCell<Option<BeforeEffects>>;
}

/// The executor consumes this once at entry, including normal paths, before any mutation.
#[cfg(unix)]
pub(crate) fn take_before_effects(request: &IntakeRequest) -> Option<BeforeEffects> {
    BEFORE_EFFECTS
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten()
        .filter(|proof| {
            intake_request_from_row(proof.row()).is_ok_and(|original| original == *request)
        })
}

#[cfg(unix)]
async fn scope_before_effects<T>(
    row: &IntakeOutboxRow,
    execution: impl std::future::Future<Output = T>,
) -> T {
    let mut spawned = row.clone();
    spawned.status = IntakeOutboxStatus::Spawned;
    BEFORE_EFFECTS
        .scope(
            std::cell::RefCell::new(Some(BeforeEffects(spawned))),
            execution,
        )
        .await
}

pub(super) async fn execute_before_effects(
    row: &IntakeOutboxRow,
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    request: IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
) -> Result<IntakeExecution, Box<dyn std::error::Error + Send + Sync>> {
    let execution = execute_intake_turn_core(http, shared, token, request, uploads);
    #[cfg(unix)]
    {
        scope_before_effects(row, execution).await
    }
    #[cfg(not(unix))]
    {
        let _ = row;
        execution.await
    }
}

#[cfg(all(test, unix))]
#[path = "consent_scope_tests.rs"]
mod tests;
