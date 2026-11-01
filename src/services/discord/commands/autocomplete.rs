use std::{collections::HashSet, future::Future, time::Duration};

use poise::serenity_prelude::{AutocompleteChoice, CreateAutocompleteResponse};

use super::super::{Context, check_auth};

pub(super) async fn authorized(ctx: Context<'_>) -> bool {
    check_auth(
        ctx.author().id,
        &ctx.author().name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
}

// Leave time for the framework's channel check and the Discord response transport.
pub(super) async fn complete(
    partial: &str,
    candidates: impl Future<Output = Vec<(String, String)>>,
) -> CreateAutocompleteResponse {
    let candidates = tokio::time::timeout(Duration::from_secs(2), candidates)
        .await
        .unwrap_or_default();
    let partial = partial.to_lowercase();
    let mut seen = HashSet::new();
    let choices = candidates
        .into_iter()
        .filter(|(label, value)| {
            !value.is_empty()
                && value.chars().count() <= 100
                && (label.to_lowercase().contains(&partial)
                    || value.to_lowercase().contains(&partial))
                && seen.insert(value.clone())
        })
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
