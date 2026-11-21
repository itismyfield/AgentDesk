//! A delegation is a handoff whose origin names the requesting channel. The server posts one
//! result card there once the worker's terminal receipt commits, and answers the card's
//! `[상태]`/`[중단]` controls from input-result and turn/stop alone.

use serde_json::Value;
use sqlx::PgPool;

use super::input_result::{final_answer, start_metadata_with_origin};
use crate::db::turns::OriginReceipt;
use crate::domain::outbound::outbox::OutboxMessage;
use crate::services::provider::ProviderKind;

const ORIGIN_PREFIX: &str = "dlg:";
const NONCE_LEN: usize = 16;
/// Discord rejects a component whose custom_id is longer.
const CUSTOM_ID_MAX: usize = 100;
const RESULT_DEDUPE_PREFIX: &str = "delegation-result:";
/// Receipts older than this are left alone. It stays inside outbox retention, so a card that was
/// sent and later pruned is never posted again.
const PUBLISH_WINDOW_HOURS: i32 = 24;
const PUBLISH_BATCH: i64 = 32;
const ANSWER_PREVIEW_CHARS: usize = 1500;

/// `dlg:<requester channel>:<nonce>:<worker agent>`, minted only by a delegation handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationOrigin {
    pub requester_channel: u64,
    pub worker_agent: String,
    nonce: String,
}

impl DelegationOrigin {
    fn new(requester_channel: u64, worker_agent: &str) -> Result<Self, &'static str> {
        let nonce = uuid::Uuid::new_v4().simple().to_string()[..NONCE_LEN].to_string();
        let worker_agent = worker_agent.trim().to_string();
        let origin = Self {
            requester_channel,
            worker_agent,
            nonce,
        };
        // Every id minted must come back unchanged from a button's custom_id.
        match parse_click(&Click::Status.custom_id(&origin)) {
            Some((_, parsed)) if parsed == origin => Ok(origin),
            _ => Err("delegation_worker_id_unsupported"),
        }
    }

    pub fn id(&self) -> String {
        let (channel, nonce) = (self.requester_channel, &self.nonce);
        format!("{ORIGIN_PREFIX}{channel}:{nonce}:{}", self.worker_agent)
    }

    pub fn parse(origin: &str) -> Option<Self> {
        let mut parts = origin.strip_prefix(ORIGIN_PREFIX)?.splitn(3, ':');
        let (channel, nonce, agent) = (parts.next()?, parts.next()?, parts.next()?);
        let digits = !channel.is_empty() && channel.bytes().all(|b| b.is_ascii_digit());
        let requester_channel = channel.parse::<u64>().ok().filter(|id| digits && *id > 0)?;
        let nonce_ok = nonce.len() == NONCE_LEN
            && nonce
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let agent_ok = !agent.is_empty()
            && agent
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
        (nonce_ok && agent_ok).then(|| Self {
            requester_channel,
            worker_agent: agent.to_string(),
            nonce: nonce.to_string(),
        })
    }
}

/// A delegation handoff: its origin and the provider whose bot runs the requesting channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    pub origin: DelegationOrigin,
    pub requester_provider: ProviderKind,
}

/// The start metadata of a handoff and, for a delegation, the origin minted for it. A
/// delegation's origin is always the server's; a plain handoff's origin passes through as on
/// turn/start. Errors carry the HTTP status and code.
pub async fn handoff_origin(
    pool: &PgPool,
    from_agent_id: &str,
    to_agent_id: &str,
    delegation: bool,
    origin_id: Option<&str>,
    metadata: Option<Value>,
) -> Result<(Option<Value>, Option<Delegation>), (u16, &'static str)> {
    if !delegation {
        let metadata = start_metadata_with_origin(metadata, origin_id).map_err(|e| (400, e))?;
        return Ok((metadata, None));
    }
    if origin_id.is_some_and(|origin| !origin.trim().is_empty()) {
        return Err((400, "origin_id_conflict"));
    }
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pool, from_agent_id.trim())
        .await
        .map_err(|_| (503, "delegation_requester_unavailable"))?
        .ok_or((404, "delegation_requester_not_found"))?;
    let channel = bindings.primary_channel().and_then(|channel| {
        crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&channel)
            .or_else(|| channel.parse::<u64>().ok())
            .filter(|id| *id > 0)
    });
    let (Some(channel), Some(requester_provider)) =
        (channel, bindings.resolved_primary_provider_kind())
    else {
        return Err((422, "delegation_requester_has_no_channel"));
    };
    let origin = DelegationOrigin::new(channel, to_agent_id).map_err(|e| (400, e))?;
    let metadata =
        start_metadata_with_origin(metadata, Some(&origin.id())).map_err(|e| (400, e))?;
    let delegation = Delegation {
        origin,
        requester_provider,
    };
    Ok((metadata, Some(delegation)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Status,
    Stop,
}

impl Click {
    pub fn custom_id(self, origin: &DelegationOrigin) -> String {
        let tag = match self {
            Self::Status => "dlg-status",
            Self::Stop => "dlg-stop",
        };
        format!("{tag}|{}", origin.id())
    }
}

/// The one check every card click passes before any lookup.
pub fn parse_click(custom_id: &str) -> Option<(Click, DelegationOrigin)> {
    if custom_id.len() > CUSTOM_ID_MAX {
        return None;
    }
    let (tag, origin) = custom_id.split_once('|')?;
    let click = match tag {
        "dlg-status" => Click::Status,
        "dlg-stop" => Click::Stop,
        _ => return None,
    };
    Some((click, DelegationOrigin::parse(origin)?))
}

/// Who pressed a card control and where.
pub struct Actor {
    pub owner: Option<u64>,
    pub user: u64,
    pub channel: u64,
}

/// The existing APIs a card control calls: input-result to read, turn/stop to stop.
#[async_trait::async_trait]
pub trait DelegationControl {
    async fn input_result(&self, agent_id: &str, origin_id: &str) -> Result<Value, String>;
    async fn stop(&self, agent_id: &str) -> Result<Value, String>;
}

/// Only the requesting channel's owner, pressing in that channel, may read or stop. A stop goes
/// out only while input-result names a running execution, so a repeated press or one after the
/// end never reaches another turn.
pub async fn click_reply(
    control: &(dyn DelegationControl + Sync),
    click: Click,
    origin: &DelegationOrigin,
    actor: &Actor,
) -> String {
    if actor.owner != Some(actor.user) || actor.channel != origin.requester_channel {
        return "이 위임 카드는 비서 채널 소유자만 이 채널에서 조작할 수 있습니다.".to_string();
    }
    let state = match control
        .input_result(&origin.worker_agent, &origin.id())
        .await
    {
        Ok(state) => state,
        Err(error) if click == Click::Stop => {
            return format!("상태를 읽지 못해 중단하지 않았습니다: {error}");
        }
        Err(error) => return format!("상태를 읽지 못했습니다: {error}"),
    };
    let line = status_line(&state);
    let running = state["consumption"] == "consumed" && state["terminal"].is_null();
    match click {
        Click::Status => line,
        Click::Stop if running => match control.stop(&origin.worker_agent).await {
            Ok(_) => format!("⏹ 중단을 요청했습니다.\n{line}"),
            Err(error) => format!("중단 요청이 받아들여지지 않았습니다: {error}\n{line}"),
        },
        Click::Stop => format!("중단할 실행이 없습니다.\n{line}"),
    }
}

fn outcome_label(outcome: &str) -> &str {
    match outcome {
        "completed" => "✅ 완료",
        "cancelled" => "⏹ 중단됨",
        "failed" => "❌ 실패",
        "empty" => "∅ 빈 응답",
        other => other,
    }
}

fn status_line(state: &Value) -> String {
    let channel = state["channel_id"].as_str().unwrap_or("?");
    match (state["consumption"].as_str(), &state["terminal"]) {
        (Some("accepted"), _) => format!("⏳ 대기 중 — 아직 시작 전입니다 · <#{channel}>"),
        (Some("consumed"), Value::Null) => format!("▶️ 실행 중 · <#{channel}>"),
        (Some("consumed"), terminal) => {
            let outcome = terminal["outcome"].as_str().unwrap_or("?");
            let at = terminal["confirmed_at"].as_str().unwrap_or("?");
            format!("{} · {at} · <#{channel}>", outcome_label(outcome))
        }
        _ => "상태를 알 수 없습니다.".to_string(),
    }
}

/// The card for one ended delegation: its outcome and the verified final answer, else a pointer
/// to the worker channel.
fn result_card(origin: &DelegationOrigin, receipt: &OriginReceipt, answer: Option<&str>) -> String {
    let outcome = outcome_label(&receipt.terminal_outcome);
    let body = match answer {
        Some(text) if text.chars().count() > ANSWER_PREVIEW_CHARS => {
            let cut: String = text.chars().take(ANSWER_PREVIEW_CHARS).collect();
            format!(
                "{cut}…\n전체 답은 워커 채널 <#{}>에 있습니다.",
                receipt.channel_id
            )
        }
        Some(text) => text.to_string(),
        None => format!("결과는 워커 채널 <#{}>에서 확인하세요.", receipt.channel_id),
    };
    let (worker, id) = (&origin.worker_agent, origin.id());
    format!("📬 **위임 결과** · `{worker}` · {outcome}\n{body}\n-# origin `{id}`")
}

/// Stages the result card of every recently ended delegation that has none. It reads committed
/// receipts only, so whichever writer ended the execution, before or across a restart, the card
/// follows; the origin-keyed outbox dedupe keeps it to one per delegation.
pub async fn publish_result_cards(pool: &PgPool) -> Result<usize, String> {
    let due: Vec<(
        String,
        String,
        String,
        chrono::DateTime<chrono::Utc>,
        String,
    )> = sqlx::query_as(
        "SELECT t.turn_id, t.channel_id, t.terminal_outcome, t.terminal_confirmed_at, o.origin
               FROM turns t CROSS JOIN LATERAL unnest(t.consumed_origin_ids) AS o(origin)
              WHERE t.finished_at >= NOW() - make_interval(hours => $1)
                AND t.terminal_outcome IS NOT NULL AND t.terminal_confirmed_at IS NOT NULL
                AND o.origin LIKE 'dlg:%'
                AND NOT EXISTS (SELECT 1 FROM message_outbox m WHERE m.dedupe_key = $2 || o.origin)
              ORDER BY t.finished_at, t.turn_id LIMIT $3",
    )
    .bind(PUBLISH_WINDOW_HOURS)
    .bind(RESULT_DEDUPE_PREFIX)
    .bind(PUBLISH_BATCH)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("read ended delegations failed: {error}"))?;
    let mut staged = 0;
    for (turn_id, channel_id, terminal_outcome, terminal_confirmed_at, origin_id) in due {
        let Some(origin) = DelegationOrigin::parse(&origin_id) else {
            continue;
        };
        let receipt = OriginReceipt {
            turn_id,
            channel_id,
            terminal_outcome,
            terminal_confirmed_at,
        };
        let answer = final_answer(pool, &receipt).await?;
        let content = result_card(&origin, &receipt, answer.as_ref().map(|a| a.text.as_str()));
        let target = format!("channel:{}", origin.requester_channel);
        let message = OutboxMessage {
            target: &target,
            content: &content,
            bot: "notify",
            source: crate::services::message_outbox::LIFECYCLE_NOTIFIER_SOURCE,
            reason_code: Some("delegation.result"),
            session_key: Some(&origin_id),
        };
        let dedupe = format!("{RESULT_DEDUPE_PREFIX}{origin_id}");
        crate::services::message_outbox::enqueue_outbox_pg_with_exact_origin(
            pool, message, &dedupe, None, None, None, None,
        )
        .await
        .map_err(|error| format!("stage delegation result failed: {error:?}"))?;
        staged += 1;
    }
    Ok(staged)
}

/// Leader loop: stages due result cards, at once on boot and then every few seconds.
pub async fn result_card_loop(pool: PgPool) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(error) = publish_result_cards(&pool).await {
            tracing::warn!(%error, "delegation result cards not staged");
        }
    }
}

#[cfg(test)]
#[path = "delegation_tests.rs"]
mod tests;
