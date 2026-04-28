use super::super::meeting;
use super::super::{Context, Error, check_auth};
use crate::services::provider::ProviderKind;

#[poise::command(slash_command, rename = "meeting")]
pub(in crate::services::discord) async fn cmd_meeting(
    ctx: Context<'_>,
    #[description = "Action: start / stop / status"] action: String,
    #[description = "Agenda (required for start)"] agenda: Option<String>,
    #[description = "Primary provider (optional: claude / codex / gemini / opencode / qwen)"]
    primary_provider: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    let channel_id = ctx.channel_id();
    let agenda_str = agenda.as_deref().unwrap_or("");
    println!("  [{ts}] ◀ [{user_name}] /meeting {action} {agenda_str}");

    ctx.defer().await?;

    let http = ctx.serenity_context().http.clone();
    let shared = ctx.data().shared.clone();

    match action.as_str() {
        "start" => {
            let agenda_text = agenda_str.trim();
            if agenda_text.is_empty() {
                ctx.say(
                    "사용법: `/meeting start <안건>` + optional `primary_provider=claude|codex|gemini|opencode|qwen`",
                )
                .await?;
                return Ok(());
            }
            let selected_provider = match primary_provider.as_deref().map(ProviderKind::from_str) {
                Some(Some(provider)) => provider,
                Some(None) => {
                    ctx.say(
                        "primary_provider는 `claude`, `codex`, `gemini`, `opencode`, `qwen` 중 하나여야 해.",
                    )
                    .await?;
                    return Ok(());
                }
                None => ctx.data().provider.clone(),
            };
            let agenda_owned = agenda_text.to_string();
            // Spawn as background task
            let spawn_provider = selected_provider.clone();
            let spawn_reviewer = selected_provider.counterpart();
            tokio::spawn(async move {
                match meeting::start_meeting(
                    &*http,
                    channel_id,
                    &agenda_owned,
                    spawn_provider,
                    spawn_reviewer,
                    &shared,
                )
                .await
                {
                    Ok(Some(id)) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ✅ Meeting completed: {id}");
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ❌ Meeting error: {e}");
                        let _ = meeting::send_meeting_message(
                            &http,
                            channel_id,
                            &shared,
                            format!("❌ 회의 오류: {}", e),
                        )
                        .await;
                    }
                }
            });
            ctx.say(format!(
                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                selected_provider.display_name(),
                selected_provider.counterpart().display_name()
            ))
            .await?;
        }
        "stop" => {
            meeting::cancel_meeting(&*http, channel_id, &shared).await?;
        }
        "status" => {
            meeting::meeting_status(&*http, channel_id, &shared).await?;
        }
        _ => {
            ctx.say("사용법: `/meeting start|stop|status`").await?;
        }
    }

    Ok(())
}
