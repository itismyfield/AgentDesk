use super::super::prompt_builder::system_prompt_audit;
use super::super::{Context, Error, auto_restore_session, check_auth};
use crate::db::provider_session_prompts;
use crate::services::provider::ProviderKind;

mod reconstruct;

fn inspection_binding(
    active: Option<&crate::services::provider::CancelToken>,
    provider: ProviderKind,
    persisted: Option<String>,
) -> Option<(ProviderKind, String)> {
    active
        .and_then(crate::services::provider::session_prompt::observed_binding)
        .or_else(|| persisted.map(|session| (provider, session)))
}

pub(super) struct Inspection {
    prompt: String,
    anchors: usize,
    started: Option<chrono::DateTime<chrono::Utc>>,
    original: bool,
}

impl Inspection {
    fn messages(&self, session: &str) -> Vec<String> {
        let started = self
            .started
            .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string());
        system_prompt_audit::render(
            &self.prompt,
            session,
            started.as_deref(),
            self.anchors,
            self.original,
        )
    }
}

async fn load_or_reconstruct<F, Fut>(
    pool: Option<&sqlx::PgPool>,
    provider: &str,
    channel: &str,
    session: &str,
    rebuild: F,
) -> Result<Inspection, Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(String, usize), Error>>,
{
    if let Some(pool) = pool {
        match provider_session_prompts::fetch(pool, provider, channel, session).await {
            Ok(Some(saved)) => {
                return Ok(Inspection {
                    prompt: saved.system_prompt,
                    anchors: saved.anchor_count as usize,
                    started: Some(saved.started_at),
                    original: true,
                });
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "system prompt snapshot unavailable; reconstructing")
            }
        }
    }
    let (prompt, anchors) = rebuild().await?;
    Ok(Inspection {
        prompt,
        anchors,
        started: None,
        original: false,
    })
}

/// /system-prompt — 현재 세션에 주입된 system prompt 전문을 확인합니다
#[poise::command(slash_command, rename = "system-prompt")]
pub(in crate::services::discord) async fn cmd_system_prompt(ctx: Context<'_>) -> Result<(), Error> {
    ctx.defer().await?;
    let data = ctx.data();
    if !check_auth(
        ctx.author().id,
        &ctx.author().name,
        &data.shared,
        &data.token,
    )
    .await
    {
        ctx.say("이 명령을 사용할 권한이 없어요.").await?;
        return Ok(());
    }
    let channel = ctx.channel_id();
    let provider =
        super::config::effective_provider_for_channel(&data.shared, channel, &data.provider, None)
            .await;
    auto_restore_session(&data.shared, channel, ctx.serenity_context()).await;
    let mailbox = super::super::mailbox_snapshot(&data.shared, channel).await;
    let persisted = data
        .shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|s| s.session_id.clone());
    let Some((provider, session)) =
        inspection_binding(mailbox.cancel_token.as_deref(), provider, persisted)
    else {
        ctx.say("현재 provider 세션이 없어요.").await?;
        return Ok(());
    };
    if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) {
        ctx.say("이 명령은 Claude와 Codex 세션을 지원해요.").await?;
        return Ok(());
    }
    let inspection = load_or_reconstruct(
        data.shared.pg_pool.as_ref(),
        provider.as_str(),
        &channel.to_string(),
        &session,
        || reconstruct::build(ctx, &provider, &session),
    )
    .await?;
    for message in inspection.messages(&session) {
        ctx.send(
            poise::CreateReply::default()
                .content(message)
                .ephemeral(false),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
