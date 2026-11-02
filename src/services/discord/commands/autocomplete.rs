use super::super::{Context, check_auth};
use poise::serenity_prelude::{AutocompleteChoice, CreateAutocompleteResponse};
use std::{
    collections::HashSet,
    future::Future,
    time::{Duration, Instant},
};

struct InvocationBudget {
    deadline: Instant,
    routed: bool,
}

// Parent command checks and the completion callback share the interaction's original budget.
pub(in crate::services::discord) async fn query_deadline(ctx: Context<'_>) -> Instant {
    if let Some(budget) = ctx.invocation_data::<InvocationBudget>().await {
        return budget.deadline;
    }
    let age = match ctx {
        poise::Context::Application(app) => Duration::from_millis(
            chrono::Utc::now()
                .timestamp_millis()
                .saturating_sub(app.interaction.id.created_at().timestamp_millis())
                .max(0) as u64,
        ),
        poise::Context::Prefix(_) => Duration::ZERO,
    };
    let deadline = Instant::now() + Duration::from_millis(2300).saturating_sub(age);
    ctx.set_invocation_data(InvocationBudget {
        deadline,
        routed: false,
    })
    .await;
    deadline
}

pub(in crate::services::discord) async fn already_routed(ctx: Context<'_>) -> bool {
    ctx.invocation_data::<InvocationBudget>()
        .await
        .is_some_and(|budget| budget.routed)
}

pub(in crate::services::discord) async fn mark_routed(ctx: Context<'_>) {
    if let Some(mut budget) = ctx.invocation_data::<InvocationBudget>().await {
        budget.routed = true;
    }
}

pub(super) async fn authorized(ctx: Context<'_>) -> bool {
    check_auth(
        ctx.author().id,
        &ctx.author().name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
}

pub(super) async fn complete(
    ctx: Context<'_>,
    partial: &str,
    candidates: impl Future<Output = Vec<(String, String)>>,
) -> CreateAutocompleteResponse {
    let deadline = query_deadline(ctx).await;
    let candidates = tokio::time::timeout_at(deadline.into(), candidates)
        .await
        .unwrap_or_default();
    let partial = partial.to_lowercase();
    let mut seen = HashSet::new();
    let mut candidates: Vec<_> = candidates
        .into_iter()
        .filter(|(label, value)| {
            !value.is_empty()
                && value.chars().count() <= 100
                && (label.to_lowercase().contains(&partial)
                    || value.to_lowercase().contains(&partial))
                && seen.insert(value.clone())
        })
        .collect();
    candidates.sort_by_cached_key(|(_, value)| {
        let value = value.to_lowercase();
        if value.starts_with(&partial) {
            0
        } else if value.contains(&partial) {
            1
        } else {
            2
        }
    });
    let choices = candidates
        .into_iter()
        .take(25)
        .map(|(label, value)| {
            let label = if label.trim().is_empty() {
                &value
            } else {
                &label
            };
            let label: String = label
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(100)
                .collect();
            AutocompleteChoice::new(label, value)
        })
        .collect();
    CreateAutocompleteResponse::new().set_choices(choices)
}
