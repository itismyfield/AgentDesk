use super::super::super::{Context, InterventionMode};
use super::super::autocomplete;
use crate::services::discord::queue_io::{CancelAllReport, CancelledInput};
use poise::serenity_prelude::{CreateAutocompleteResponse, MessageId, UserId};

pub(super) async fn autocomplete_cancel_queued(
    ctx: Context<'_>,
    partial: &str,
) -> CreateAutocompleteResponse {
    if !autocomplete::authorized(ctx).await {
        return CreateAutocompleteResponse::new();
    }
    autocomplete::complete(ctx, partial, async {
        let Some(mailbox) = ctx.data().shared.mailboxes.peek(ctx.channel_id()) else {
            return Vec::new();
        };
        let candidates = match mailbox
            .v2_request(|slot| Ok(slot.as_ref().map(|owner| owner.cancelable())))
            .await
        {
            Ok(Some(mut items)) => {
                // Keep newest waiting inputs ahead of retained inputs before the choice limit.
                let queued = items.partition_point(|item| !item.4);
                items[..queued].reverse();
                items
            }
            Ok(None) => match mailbox.try_snapshot().await {
                Ok(snapshot) => snapshot
                    .intervention_queue
                    .into_iter()
                    .filter(|item| item.mode == InterventionMode::Soft)
                    .map(|item| {
                        let at = chrono::Utc::now().timestamp_millis().max(0) as u64;
                        let age =
                            item.created_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                        (
                            item.author_id.get(),
                            item.message_id.get(),
                            Some(at.saturating_sub(age)),
                            item.text,
                            false,
                        )
                    })
                    .collect(),
                Err(error) => {
                    tracing::warn!(?error, "cancel autocomplete mailbox snapshot unavailable");
                    return Vec::new();
                }
            },
            Err(error) => {
                tracing::warn!(%error, "cancel autocomplete owner unavailable");
                return Vec::new();
            }
        };
        candidates
            .into_iter()
            .map(|(author, id, at, text, retained)| {
                (label(ctx, author, id, at, &text, retained), id.to_string())
            })
            .collect()
    })
    .await
}

fn label(
    ctx: Context<'_>,
    author: u64,
    id: u64,
    at: Option<u64>,
    text: &str,
    retained: bool,
) -> String {
    let author = if author <= 1 {
        "합성 입력".into()
    } else {
        ctx.serenity_context()
            .cache
            .user(UserId::new(author))
            .map(|user| user.display_name().to_owned())
            .unwrap_or_else(|| format!("user:{author}"))
    };
    let at = at
        .and_then(|at| i64::try_from(at).ok())
        .and_then(chrono::DateTime::from_timestamp_millis)
        .unwrap_or_else(|| MessageId::new(id).created_at().to_utc());
    let time = at
        .with_timezone(&chrono_tz::Asia::Seoul)
        .format("%m-%d %H:%M");
    format!(
        "{author} · {time} · {}{text}",
        if retained { "(보관) " } else { "" }
    )
}

pub(super) fn clear_reply(report: &CancelAllReport) -> String {
    if report.error.is_some() || report.count_unknown {
        return format!(
            "대기열 비우기의 저장 완료를 확인하지 못했어요. 재시작 후 입력이 복원될 수 있어요. {}",
            report
                .error
                .as_deref()
                .unwrap_or("대기 입력 수가 불확정입니다.")
        );
    }
    let mut lines = vec![format!(
        "대기 {}개와 보관 {}개, 총 {}개를 비웠어요(전달 직전 포함).",
        report.queued.len(),
        report.retained.len(),
        report.queued.len() + report.retained.len()
    )];
    for item in report.queued.iter().chain(&report.retained) {
        lines.push(excerpt(item));
    }
    if report.uncertain > 0 {
        lines.push(format!(
            "보관 {}개는 전송 여부가 불확정이어서 기록과 늦은 전달 확인을 보존했어요.",
            report.uncertain
        ));
    }
    if report.kept > 0 {
        lines.push(format!(
            "유지 {}개: 진행 중인 입력은 그대로 진행합니다(결과는 평소처럼 안내될 수 있음).",
            report.kept
        ));
    }
    lines.join("\n")
}

fn excerpt(item: &CancelledInput) -> String {
    let text: String = item
        .text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(100)
        .collect();
    format!("• user:{} · {} · {}", item.author, item.id, text)
}
