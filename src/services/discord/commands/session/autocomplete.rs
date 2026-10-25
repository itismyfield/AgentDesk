use poise::serenity_prelude::{AutocompleteChoice, CreateAutocompleteResponse};

use super::super::super::{Context, check_auth};
use crate::db::dispatched_sessions::{
    load_live_bound_session_ids_pg, load_session_rebind_context_pg,
};
use crate::services::{provider::ProviderKind, session_resume::catalog};

pub(super) async fn autocomplete_session(
    ctx: Context<'_>,
    partial: &str,
) -> CreateAutocompleteResponse {
    let choices = tokio::time::timeout(std::time::Duration::from_millis(2500), async {
        if !matches!(ctx.data().provider, ProviderKind::Claude)
            || !check_auth(
                ctx.author().id,
                &ctx.author().name,
                &ctx.data().shared,
                &ctx.data().token,
            )
            .await
        {
            return Vec::new();
        }
        let shared = &ctx.data().shared;
        let Some(pool) = shared.pg_pool.as_ref() else {
            return Vec::new();
        };
        let Some(key) = crate::services::discord::adk_session::build_adk_session_key(
            shared,
            ctx.channel_id(),
            &ctx.data().provider,
            None,
        )
        .await
        else {
            return Vec::new();
        };
        let Ok(Some(context)) = load_session_rebind_context_pg(pool, &key).await else {
            return Vec::new();
        };
        let Some(cwd) = context.cwd else {
            return Vec::new();
        };
        let Ok(live) = load_live_bound_session_ids_pg(pool).await else {
            return Vec::new();
        };
        let partial = partial.to_string();
        let home = catalog::claude_home();
        tokio::task::spawn_blocking(move || {
            catalog::choices(
                &cwd,
                context.claude_session_id.as_deref(),
                &live,
                home.as_deref(),
                &partial,
            )
        })
        .await
        .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    CreateAutocompleteResponse::new().set_choices(
        choices
            .into_iter()
            .map(|choice| AutocompleteChoice::new(choice.label, choice.session_id))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn resume_session_parameter_registers_autocomplete() {
        let command = super::super::cmd_resume();
        let parameter = command
            .parameters
            .iter()
            .find(|parameter| parameter.name == "session_id")
            .unwrap();
        assert!(parameter.autocomplete_callback.is_some());
    }
}
