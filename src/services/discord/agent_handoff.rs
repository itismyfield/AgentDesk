use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::bot_role::UtilityBotRole;
use super::health::{self, HealthRegistry};
use super::router::{HeadlessTurnStartError, HeadlessTurnStartOutcome};
use crate::db::agents::AgentChannelBindings;
use crate::services::provider::ProviderKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentHandoffChannelKind {
    Cc,
    Cdx,
}

impl AgentHandoffChannelKind {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, AgentHandoffError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("cc") => Ok(Self::Cc),
            Some("cdx") => Ok(Self::Cdx),
            Some(_) => Err(AgentHandoffError::bad_request(
                "channel_kind must be cc or cdx",
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cc => "cc",
            Self::Cdx => "cdx",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AgentHandoffResponse {
    pub(crate) to_agent_id: String,
    pub(crate) channel_id: String,
    pub(crate) channel_kind: &'static str,
    pub(crate) message_id: String,
    pub(crate) bot: &'static str,
    pub(crate) prefixed: bool,
}

impl AgentHandoffResponse {
    pub(crate) fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHandoffError {
    status: StatusCode,
    body: Value,
}

impl AgentHandoffError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: json!({"error": message.into()}),
        }
    }

    fn agent_not_found(to_agent_id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: json!({"error": "agent not found", "to_agent_id": to_agent_id}),
        }
    }

    fn channel_kind_unset(
        to_agent_id: &str,
        channel_kind: AgentHandoffChannelKind,
        available_kinds: Vec<&'static str>,
    ) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: json!({
                "error": "channel_kind unset",
                "to_agent_id": to_agent_id,
                "channel_kind": channel_kind.as_str(),
                "available_kinds": available_kinds,
            }),
        }
    }

    fn announce_bot_not_configured() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({"error": "announce bot not configured"}),
        }
    }

    /// 409 — a turn is already active for the target mailbox. This semantic is
    /// unique to the turn-trigger handoff (#3556); the announce-only path
    /// silently coalesced into the running turn instead.
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            body: json!({"error": message.into(), "status": "conflict"}),
        }
    }

    /// 503 — the headless turn could not be reserved (runtime unavailable).
    fn turn_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: json!({"error": message.into()}),
        }
    }

    fn discord_send_failed(detail: String) -> Self {
        let discord_status = extract_discord_status(&detail);
        let mut body = json!({
            "error": "Discord send failed",
            "detail": detail,
        });
        if let Some(status) = discord_status {
            body["discord_status"] = json!(status);
        }
        Self {
            status: StatusCode::BAD_GATEWAY,
            body,
        }
    }

    fn upstream(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({"error": message.into()}),
        }
    }

    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn body(&self) -> Value {
        self.body.clone()
    }

    pub(crate) fn one_line(&self) -> String {
        let error = self
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("send-to-agent failed");
        match error {
            "agent not found" => {
                let to_agent_id = self
                    .body
                    .get("to_agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                format!("agent not found: {to_agent_id}")
            }
            "channel_kind unset" => {
                let channel_kind = self
                    .body
                    .get("channel_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let available = self
                    .body
                    .get("available_kinds")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                format!("channel_kind unset: {channel_kind}; available_kinds=[{available}]")
            }
            "Discord send failed" => self
                .body
                .get("detail")
                .and_then(Value::as_str)
                .map(|detail| format!("Discord send failed: {detail}"))
                .unwrap_or_else(|| "Discord send failed".to_string()),
            other => other.to_string(),
        }
    }
}

/// How the receiver can reach the requester when a reply is required (#4383).
///
/// Watchdogs and routines hand off under pseudo-source ids (`system`) that carry
/// no Discord channel binding. Telling their receiver to run `send-to-agent --to
/// system` yields `agent not found: system`, so the contract must route those
/// replies back to the channel the handoff landed on instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyRoute {
    /// Requester resolves to a mailbox — reply via `send-to-agent`.
    Mailbox,
    /// Requester has no mailbox — report into the handoff's own channel.
    ChannelOnly,
}

/// Whether `agent_id` is a routable reply target: registered *and* resolving to
/// at least one Discord mailbox. Mirrors the reachability predicate
/// [`send_agent_handoff`] applies to `to_agent_id`, so a contract that promises
/// `send-to-agent` only ever names an id that command can actually deliver to.
///
/// A query failure propagates rather than defaulting to a route: both callers
/// have already proven the pool healthy by loading `to_agent_id`'s bindings, so
/// an error here is a real anomaly, and guessing would reintroduce the very
/// class of bug this check exists to prevent — a contract that misdirects.
async fn resolve_reply_route(
    pg_pool: &PgPool,
    agent_id: &str,
) -> Result<ReplyRoute, AgentHandoffError> {
    let routable = crate::db::agents::load_agent_channel_bindings_pg(pg_pool, agent_id.trim())
        .await
        .map_err(|error| {
            AgentHandoffError::internal(format!("query reply-target channels: {error}"))
        })?
        .and_then(|bindings| bindings.primary_channel())
        .is_some();
    Ok(if routable {
        ReplyRoute::Mailbox
    } else {
        ReplyRoute::ChannelOnly
    })
}

/// Reply-expectation contract (#3620 follow-up) appended to the end of a handoff
/// body. `true` → 회신 필수 (the receiver must report back to the requester),
/// `false` → 회신 불필요 (no callback expected). Kept aligned with the role
/// response-contract: replies follow the request's callback info + channel rules.
fn reply_expectation_contract(
    from_agent_id: &str,
    expect_reply: bool,
    reply_route: ReplyRoute,
) -> String {
    match (expect_reply, reply_route) {
        (true, ReplyRoute::Mailbox) => format!(
            "──────────\n📨 **회신 필수 계약**: 이 핸드오프는 회신을 요구합니다. 작업/검토를 마치면 결과·결론을 요청자 `{from_agent_id}`에게 반드시 회신하세요. 회신은 `agentdesk send-to-agent --from <self> --to {from_agent_id} --message \"...\" --expect-reply false` 로 보냅니다(현재 요청의 callback 정보·채널 규칙 우선)."
        ),
        (true, ReplyRoute::ChannelOnly) => format!(
            "──────────\n📨 **회신 필수 계약**: 이 핸드오프는 회신을 요구합니다. 작업/검토를 마치면 결과·결론을 반드시 보고하세요. 단 요청자 `{from_agent_id}`는 메일박스가 없어 `send-to-agent`로 회신할 수 없습니다 — 이 핸드오프가 도착한 채널에 결과를 보고하세요."
        ),
        (false, _) => "──────────\n📭 **회신 불필요 계약**: 이 핸드오프는 회신을 요구하지 않습니다. 별도 보고 없이 처리하세요(중대한 이슈를 발견한 경우에만 자율적으로 회신).".to_string(),
    }
}

/// Build the handoff body. When `expect_reply` is `Some`, a reply-expectation
/// contract is appended to the end of the message; `None` keeps the legacy
/// behavior (no contract) for backward compatibility. `reply_route` decides how
/// a required reply is addressed and is ignored otherwise.
pub(crate) fn build_agent_handoff_content(
    from_agent_id: &str,
    to_agent_id: &str,
    message: &str,
    prefix: bool,
    expect_reply: Option<bool>,
    reply_route: ReplyRoute,
) -> String {
    let mut content = if prefix {
        format!("[{from_agent_id} → {to_agent_id} 핸드오프]\n\n{message}")
    } else {
        message.to_string()
    };
    if let Some(expect_reply) = expect_reply {
        content.push_str("\n\n");
        content.push_str(&reply_expectation_contract(
            from_agent_id,
            expect_reply,
            reply_route,
        ));
    }
    content
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_agent_handoff(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    from_agent_id: &str,
    to_agent_id: &str,
    message: &str,
    channel_kind: AgentHandoffChannelKind,
    prefix: bool,
    expect_reply: Option<bool>,
) -> Result<AgentHandoffResponse, AgentHandoffError> {
    let from_agent_id = from_agent_id.trim();
    if from_agent_id.is_empty() {
        return Err(AgentHandoffError::bad_request("from_agent_id is required"));
    }
    let to_agent_id = to_agent_id.trim();
    if to_agent_id.is_empty() {
        return Err(AgentHandoffError::bad_request("to_agent_id is required"));
    }
    if message.is_empty() {
        return Err(AgentHandoffError::bad_request("message is required"));
    }

    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pg_pool, to_agent_id)
        .await
        .map_err(|error| AgentHandoffError::internal(format!("query agent channels: {error}")))?
        .ok_or_else(|| AgentHandoffError::agent_not_found(to_agent_id))?;

    // Honor an explicit cc/cdx binding when present, but fall back to the
    // agent's primary channel so single-provider mailboxes that are not a
    // cc/cdx slot (opencode/gemini/qwen — e.g. monitoring) and cross-provider
    // sends (claude↔codex) remain reachable instead of erroring "channel_kind
    // unset". The receiving channel's intake resolves the agent's real provider.
    let Some(channel_id) =
        channel_for_kind(&bindings, channel_kind).or_else(|| bindings.primary_channel())
    else {
        return Err(AgentHandoffError::channel_kind_unset(
            to_agent_id,
            channel_kind,
            available_channel_kinds(&bindings),
        ));
    };

    let reply_route = if expect_reply == Some(true) {
        resolve_reply_route(pg_pool, from_agent_id).await?
    } else {
        ReplyRoute::Mailbox
    };
    let content = build_agent_handoff_content(
        from_agent_id,
        to_agent_id,
        message,
        prefix,
        expect_reply,
        reply_route,
    );
    let target = format!("channel:{channel_id}");
    let (status, response_body) = health::send_message_with_backends(
        registry,
        Some(pg_pool),
        &target,
        &content,
        from_agent_id,
        UtilityBotRole::Announce.alias(),
        None,
    )
    .await;
    let status = parse_health_status_code(status);
    let response_json: Value = serde_json::from_str(&response_body)
        .unwrap_or_else(|_| json!({"error": response_body.clone()}));

    if !status.is_success() {
        return Err(map_send_failure(status, response_json));
    }

    Ok(AgentHandoffResponse {
        to_agent_id: to_agent_id.to_string(),
        channel_id,
        channel_kind: channel_kind.as_str(),
        message_id: response_json
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        bot: UtilityBotRole::Announce.alias(),
        prefixed: prefix,
    })
}

/// Successful outcome of a turn-trigger handoff (#3556). Unlike
/// [`AgentHandoffResponse`] (announce message post), this carries the reserved
/// `turn_id` and lifecycle `status` so the caller learns the turn was actually
/// scheduled — not merely that a message was best-effort delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AgentHandoffTurnResponse {
    pub(crate) to_agent_id: String,
    pub(crate) channel_id: String,
    pub(crate) channel_kind: &'static str,
    pub(crate) turn_id: String,
    pub(crate) status: &'static str,
}

impl AgentHandoffTurnResponse {
    pub(crate) fn to_value(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        if let Value::Object(map) = &mut value {
            map.insert("ok".to_string(), Value::Bool(true));
        }
        value
    }
}

/// Provider that owns a given handoff channel binding. The cc binding is the
/// Claude mailbox; cdx is the Codex mailbox. Mapping the turn to the binding's
/// owning provider keeps the handoff envelope and the reserved turn on the same
/// channel so the announce-bot double-trigger (#3576) cannot fire.
fn provider_for_channel_kind(channel_kind: AgentHandoffChannelKind) -> ProviderKind {
    match channel_kind {
        AgentHandoffChannelKind::Cc => ProviderKind::Claude,
        AgentHandoffChannelKind::Cdx => ProviderKind::Codex,
    }
}

/// Registry-independent resolution for a turn-trigger handoff: validates the
/// agents, resolves the cc/cdx binding to a numeric Discord channel id, derives
/// the owning provider, and builds the handoff envelope content. Split out from
/// [`start_agent_handoff_turn`] so the resolution contract is unit-testable
/// without a live [`HealthRegistry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHandoffTurnTarget {
    pub(crate) to_agent_id: String,
    pub(crate) channel_id: String,
    pub(crate) channel_id_num: u64,
    pub(crate) channel_kind: AgentHandoffChannelKind,
    pub(crate) provider: ProviderKind,
    pub(crate) content: String,
}

#[allow(clippy::too_many_arguments)]
fn resolve_agent_handoff_turn_target(
    bindings: &AgentChannelBindings,
    from_agent_id: &str,
    to_agent_id: &str,
    prompt: &str,
    channel_kind: AgentHandoffChannelKind,
    prefix: bool,
    expect_reply: Option<bool>,
    reply_route: ReplyRoute,
) -> Result<AgentHandoffTurnTarget, AgentHandoffError> {
    let from_agent_id = from_agent_id.trim();
    if from_agent_id.is_empty() {
        return Err(AgentHandoffError::bad_request("from_agent_id is required"));
    }
    let to_agent_id = to_agent_id.trim();
    if to_agent_id.is_empty() {
        return Err(AgentHandoffError::bad_request("to_agent_id is required"));
    }
    if prompt.trim().is_empty() {
        return Err(AgentHandoffError::bad_request("prompt is required"));
    }

    // Prefer the explicit cc/cdx mailbox; otherwise fall back to the agent's
    // primary channel and its real provider so opencode/gemini/qwen mailboxes
    // (and cross-provider sends) dispatch the turn on the correct CLI rather
    // than rejecting with "channel_kind unset".
    let (channel_id, provider) = match channel_for_kind(bindings, channel_kind) {
        Some(channel_id) => (channel_id, provider_for_channel_kind(channel_kind)),
        None => {
            let Some(channel_id) = bindings.primary_channel() else {
                return Err(AgentHandoffError::channel_kind_unset(
                    to_agent_id,
                    channel_kind,
                    available_channel_kinds(bindings),
                ));
            };
            let provider = bindings
                .resolved_primary_provider_kind()
                .unwrap_or_else(|| provider_for_channel_kind(channel_kind));
            (channel_id, provider)
        }
    };

    let Some(channel_id_num) =
        crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&channel_id)
            .or_else(|| channel_id.parse::<u64>().ok())
            .filter(|id| *id > 0)
    else {
        return Err(AgentHandoffError::internal(format!(
            "agent {to_agent_id} {} channel is invalid: {channel_id}",
            channel_kind.as_str()
        )));
    };

    let content = build_agent_handoff_content(
        from_agent_id,
        to_agent_id,
        prompt,
        prefix,
        expect_reply,
        reply_route,
    );

    Ok(AgentHandoffTurnTarget {
        to_agent_id: to_agent_id.to_string(),
        channel_id,
        channel_id_num,
        channel_kind,
        provider,
        content,
    })
}

/// Map a headless turn-start result to the handoff API contract (#3556):
/// `started`/`consumed` → 200 with `turn_id`, `Conflict` → 409 (mailbox busy —
/// a semantic the announce-only path never had), `Internal` → 503.
fn map_turn_start_result(
    target: &AgentHandoffTurnTarget,
    result: Result<HeadlessTurnStartOutcome, HeadlessTurnStartError>,
) -> Result<AgentHandoffTurnResponse, AgentHandoffError> {
    match result {
        Ok(outcome) => Ok(AgentHandoffTurnResponse {
            to_agent_id: target.to_agent_id.clone(),
            channel_id: target.channel_id.clone(),
            channel_kind: target.channel_kind.as_str(),
            turn_id: outcome.turn_id,
            status: outcome.status.as_str(),
        }),
        Err(HeadlessTurnStartError::Conflict(error)) => Err(AgentHandoffError::conflict(error)),
        Err(HeadlessTurnStartError::Internal(error)) => {
            Err(AgentHandoffError::turn_unavailable(error))
        }
    }
}

/// Turn-trigger handoff (#3556). Resolves the cc/cdx binding and reserves a
/// headless turn directly on that mailbox — never posts an announce message —
/// so the handoff is an authoritative, synchronous turn reservation rather than
/// "post and hope". Because no announce message lands on the cc channel, the
/// #3576 announce-trigger branch cannot start an unintended second turn.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_agent_handoff_turn(
    registry: &HealthRegistry,
    pg_pool: &PgPool,
    from_agent_id: &str,
    to_agent_id: &str,
    prompt: &str,
    channel_kind: AgentHandoffChannelKind,
    prefix: bool,
    expect_reply: Option<bool>,
    source: Option<String>,
    metadata: Option<Value>,
) -> Result<AgentHandoffTurnResponse, AgentHandoffError> {
    let to_agent_id_trimmed = to_agent_id.trim();
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pg_pool, to_agent_id_trimmed)
        .await
        .map_err(|error| AgentHandoffError::internal(format!("query agent channels: {error}")))?
        .ok_or_else(|| AgentHandoffError::agent_not_found(to_agent_id_trimmed))?;

    let reply_route = if expect_reply == Some(true) {
        resolve_reply_route(pg_pool, from_agent_id).await?
    } else {
        ReplyRoute::Mailbox
    };

    let target = resolve_agent_handoff_turn_target(
        &bindings,
        from_agent_id,
        to_agent_id,
        prompt,
        channel_kind,
        prefix,
        expect_reply,
        reply_route,
    )?;

    let result = health::start_headless_agent_turn(
        registry,
        poise::serenity_prelude::ChannelId::new(target.channel_id_num),
        target.provider.clone(),
        target.content.clone(),
        source,
        metadata,
        None,
    )
    .await;

    map_turn_start_result(&target, result)
}

fn channel_for_kind(
    bindings: &AgentChannelBindings,
    channel_kind: AgentHandoffChannelKind,
) -> Option<String> {
    match channel_kind {
        AgentHandoffChannelKind::Cc => normalized_channel(bindings.discord_channel_cc.as_deref()),
        AgentHandoffChannelKind::Cdx => normalized_channel(bindings.discord_channel_cdx.as_deref()),
    }
}

fn available_channel_kinds(bindings: &AgentChannelBindings) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if normalized_channel(bindings.discord_channel_cc.as_deref()).is_some() {
        kinds.push("cc");
    }
    if normalized_channel(bindings.discord_channel_cdx.as_deref()).is_some() {
        kinds.push("cdx");
    }
    kinds
}

fn normalized_channel(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn map_send_failure(status: StatusCode, body: Value) -> AgentHandoffError {
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let lowered = error.to_ascii_lowercase();

    if lowered.contains("announce bot not configured") {
        return AgentHandoffError::announce_bot_not_configured();
    }
    if lowered.contains("discord send failed") {
        return AgentHandoffError::discord_send_failed(
            error
                .strip_prefix("Discord send failed:")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(&error)
                .to_string(),
        );
    }

    AgentHandoffError::upstream(status, body)
}

fn parse_health_status_code(status: &str) -> StatusCode {
    match status {
        "200 OK" => StatusCode::OK,
        "400 Bad Request" => StatusCode::BAD_REQUEST,
        "403 Forbidden" => StatusCode::FORBIDDEN,
        "404 Not Found" => StatusCode::NOT_FOUND,
        "422 Unprocessable Entity" => StatusCode::UNPROCESSABLE_ENTITY,
        "500 Internal Server Error" => StatusCode::INTERNAL_SERVER_ERROR,
        "502 Bad Gateway" => StatusCode::BAD_GATEWAY,
        "503 Service Unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn extract_discord_status(detail: &str) -> Option<u16> {
    for marker in ["HTTP ", "status "] {
        if let Some((_, rest)) = detail.split_once(marker) {
            let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            if let Ok(status) = digits.parse::<u16>() {
                return Some(status);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(cc: Option<&str>, cdx: Option<&str>) -> AgentChannelBindings {
        AgentChannelBindings {
            discord_channel_cc: cc.map(str::to_string),
            discord_channel_cdx: cdx.map(str::to_string),
            ..AgentChannelBindings::default()
        }
    }

    #[test]
    fn channel_kind_defaults_to_cc() {
        assert_eq!(
            AgentHandoffChannelKind::parse(None).unwrap(),
            AgentHandoffChannelKind::Cc
        );
    }

    #[test]
    fn channel_kind_rejects_unknown_value() {
        let error = AgentHandoffChannelKind::parse(Some("notify")).unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.body()["error"], "channel_kind must be cc or cdx");
    }

    #[test]
    fn handoff_prefix_can_be_enabled_or_disabled() {
        assert_eq!(
            build_agent_handoff_content(
                "project-agentdesk",
                "adk-dashboard",
                "hello",
                true,
                None,
                ReplyRoute::Mailbox
            ),
            "[project-agentdesk → adk-dashboard 핸드오프]\n\nhello"
        );
        assert_eq!(
            build_agent_handoff_content(
                "project-agentdesk",
                "adk-dashboard",
                "hello",
                false,
                None,
                ReplyRoute::Mailbox
            ),
            "hello"
        );
    }

    #[test]
    fn handoff_reply_expectation_appends_contract() {
        // None → no contract (backward compatible).
        assert_eq!(
            build_agent_handoff_content("a", "b", "hi", false, None, ReplyRoute::Mailbox),
            "hi"
        );
        // Some(true) → 회신 필수 contract referencing the requester.
        let required =
            build_agent_handoff_content("a", "b", "hi", false, Some(true), ReplyRoute::Mailbox);
        assert!(required.starts_with("hi\n\n"));
        assert!(required.contains("회신 필수 계약"));
        assert!(required.contains("`a`"));
        // Some(false) → 회신 불필요 contract.
        let not_required =
            build_agent_handoff_content("a", "b", "hi", false, Some(false), ReplyRoute::Mailbox);
        assert!(not_required.starts_with("hi\n\n"));
        assert!(not_required.contains("회신 불필요 계약"));
        // Contract appends after the prefix envelope too.
        let prefixed =
            build_agent_handoff_content("a", "b", "hi", true, Some(true), ReplyRoute::Mailbox);
        assert!(prefixed.starts_with("[a → b 핸드오프]\n\nhi\n\n"));
        assert!(prefixed.contains("회신 필수 계약"));
    }

    /// #4383: a pseudo-source requester (`system`) has no mailbox, so the
    /// required-reply contract must never emit `send-to-agent --to system` —
    /// running it yields `agent not found: system`.
    #[test]
    fn required_reply_to_unroutable_requester_avoids_send_to_agent() {
        let contract = build_agent_handoff_content(
            "system",
            "project-agentdesk",
            "check the relay",
            false,
            Some(true),
            ReplyRoute::ChannelOnly,
        );
        assert!(contract.contains("회신 필수 계약"));
        assert!(contract.contains("`system`"));
        // The contract may *name* send-to-agent to explain why it is unusable,
        // but must never hand the receiver a runnable `--to system` command.
        assert!(!contract.contains("--to system"));
        assert!(!contract.contains("--expect-reply"));
        assert!(contract.contains("이 핸드오프가 도착한 채널"));
    }

    /// A routable requester keeps the executable `send-to-agent` instruction.
    #[test]
    fn required_reply_to_routable_requester_keeps_send_to_agent() {
        let contract = build_agent_handoff_content(
            "project-agentdesk",
            "adk-dashboard",
            "review this",
            false,
            Some(true),
            ReplyRoute::Mailbox,
        );
        assert!(contract.contains("--to project-agentdesk"));
        assert!(contract.contains("--expect-reply false"));
    }

    /// `expect_reply=false` renders the same 회신 불필요 contract regardless of
    /// how the requester would have been reached.
    #[test]
    fn reply_route_is_ignored_when_reply_not_expected() {
        let mailbox = reply_expectation_contract("system", false, ReplyRoute::Mailbox);
        let channel_only = reply_expectation_contract("system", false, ReplyRoute::ChannelOnly);
        assert_eq!(mailbox, channel_only);
        assert!(mailbox.contains("회신 불필요 계약"));
    }

    #[test]
    fn channel_selection_uses_explicit_kind_only() {
        let bindings = bindings(Some(" 111 "), Some("222"));
        assert_eq!(
            channel_for_kind(&bindings, AgentHandoffChannelKind::Cc).as_deref(),
            Some("111")
        );
        assert_eq!(
            channel_for_kind(&bindings, AgentHandoffChannelKind::Cdx).as_deref(),
            Some("222")
        );
    }

    #[test]
    fn channel_kind_unset_reports_available_kinds() {
        let error = AgentHandoffError::channel_kind_unset(
            "agent-a",
            AgentHandoffChannelKind::Cc,
            available_channel_kinds(&bindings(None, Some("222"))),
        );
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.body()["available_kinds"], json!(["cdx"]));
        assert_eq!(
            error.one_line(),
            "channel_kind unset: cc; available_kinds=[cdx]"
        );
    }

    #[test]
    fn discord_failure_maps_to_bad_gateway_with_status_when_available() {
        let error = map_send_failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "Discord send failed: HTTP 403 {\"message\":\"Missing Permissions\"}"}),
        );
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.body()["discord_status"], 403);
    }

    // ── #3556 turn-trigger handoff ────────────────────────────────────────

    #[test]
    fn handoff_turn_maps_channel_kind_to_owning_provider() {
        assert_eq!(
            provider_for_channel_kind(AgentHandoffChannelKind::Cc),
            ProviderKind::Claude
        );
        assert_eq!(
            provider_for_channel_kind(AgentHandoffChannelKind::Cdx),
            ProviderKind::Codex
        );
    }

    #[test]
    fn handoff_turn_target_resolves_channel_provider_and_envelope() {
        let target = resolve_agent_handoff_turn_target(
            &bindings(Some("111"), Some("222")),
            "project-agentdesk",
            "adk-dashboard",
            "리뷰 반영해줘",
            AgentHandoffChannelKind::Cdx,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .expect("cdx binding resolves");
        assert_eq!(target.to_agent_id, "adk-dashboard");
        assert_eq!(target.channel_id, "222");
        assert_eq!(target.channel_id_num, 222);
        assert_eq!(target.channel_kind, AgentHandoffChannelKind::Cdx);
        assert_eq!(target.provider, ProviderKind::Codex);
        assert_eq!(
            target.content,
            "[project-agentdesk → adk-dashboard 핸드오프]\n\n리뷰 반영해줘"
        );
    }

    #[test]
    fn handoff_turn_target_honors_no_prefix() {
        let target = resolve_agent_handoff_turn_target(
            &bindings(Some("111"), None),
            "from",
            "to",
            "raw prompt",
            AgentHandoffChannelKind::Cc,
            false,
            None,
            ReplyRoute::Mailbox,
        )
        .expect("cc binding resolves");
        assert_eq!(target.content, "raw prompt");
    }

    #[test]
    fn handoff_turn_target_falls_back_to_primary_for_single_provider() {
        // Codex agent (only cdx set): a cc request must fall back to the cdx
        // mailbox and the codex provider instead of rejecting, so cross-provider
        // handoffs (e.g. a now-claude sender → codex peer) still reach it.
        let target = resolve_agent_handoff_turn_target(
            &bindings(None, Some("222")),
            "from",
            "to",
            "prompt",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .expect("falls back to the agent's primary channel");
        assert_eq!(target.channel_id, "222");
        assert_eq!(target.provider, ProviderKind::Codex);
    }

    #[test]
    fn handoff_turn_target_keeps_codex_for_primary_only_binding() {
        // Regression (#3556 P1): a Codex agent bound ONLY via discord_channel_id
        // (no cdx/alt mailbox) must still resolve to the Codex provider. The
        // earlier fallback derived Claude — codex_channel() found nothing, so
        // resolution slid to the Claude counterpart aliasing the same primary
        // channel — reserving a Claude turn on a Codex mailbox.
        let codex = AgentChannelBindings {
            provider: Some("codex".to_string()),
            discord_channel_id: Some("1495040912361914399".to_string()),
            ..AgentChannelBindings::default()
        };
        let target = resolve_agent_handoff_turn_target(
            &codex,
            "from",
            "to",
            "prompt",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .expect("codex primary-only binding resolves");
        assert_eq!(target.channel_id, "1495040912361914399");
        assert_eq!(target.provider, ProviderKind::Codex);
    }

    #[test]
    fn handoff_turn_target_keeps_codex_cc_only_binding_on_claude() {
        // A Codex-configured row with only an explicit cc binding is Claude-owned;
        // a missing cdx request must not reserve a Codex turn on that mailbox.
        let codex_cc_only = AgentChannelBindings {
            provider: Some("codex".to_string()),
            discord_channel_id: Some("1495040912361914400".to_string()),
            discord_channel_cc: Some("1495040912361914400".to_string()),
            ..AgentChannelBindings::default()
        };
        let target = resolve_agent_handoff_turn_target(
            &codex_cc_only,
            "from",
            "to",
            "prompt",
            AgentHandoffChannelKind::Cdx,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .expect("explicit cc-only fallback resolves via Claude");
        assert_eq!(target.channel_id, "1495040912361914400");
        assert_eq!(target.provider, ProviderKind::Claude);
    }

    #[test]
    fn handoff_turn_target_reaches_opencode_mailbox() {
        // opencode/gemini/qwen agents expose neither cc nor cdx; their mailbox
        // is discord_channel_id. Any requested kind must resolve there with the
        // agent's real provider so monitoring (opencode) stays reachable.
        let opencode = AgentChannelBindings {
            provider: Some("opencode".to_string()),
            discord_channel_id: Some("1495040912361914398".to_string()),
            ..AgentChannelBindings::default()
        };
        let target = resolve_agent_handoff_turn_target(
            &opencode,
            "from",
            "monitoring",
            "prompt",
            AgentHandoffChannelKind::Cdx,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .expect("opencode mailbox resolves via primary channel");
        assert_eq!(target.channel_id, "1495040912361914398");
        assert_eq!(target.provider, ProviderKind::OpenCode);
    }

    #[test]
    fn handoff_turn_target_rejects_when_no_channel_at_all() {
        // A genuinely unbound agent (no cc/cdx/primary channel) still errors.
        let error = resolve_agent_handoff_turn_target(
            &AgentChannelBindings::default(),
            "from",
            "to",
            "prompt",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn handoff_turn_target_requires_prompt() {
        let error = resolve_agent_handoff_turn_target(
            &bindings(Some("111"), None),
            "from",
            "to",
            "   ",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.body()["error"], "prompt is required");
    }

    #[test]
    fn handoff_turn_target_rejects_non_numeric_channel() {
        let error = resolve_agent_handoff_turn_target(
            &bindings(Some("not-a-channel"), None),
            "from",
            "to",
            "prompt",
            AgentHandoffChannelKind::Cc,
            true,
            None,
            ReplyRoute::Mailbox,
        )
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn sample_target() -> AgentHandoffTurnTarget {
        AgentHandoffTurnTarget {
            to_agent_id: "adk-dashboard".to_string(),
            channel_id: "111".to_string(),
            channel_id_num: 111,
            channel_kind: AgentHandoffChannelKind::Cc,
            provider: ProviderKind::Claude,
            content: "envelope".to_string(),
        }
    }

    #[test]
    fn handoff_turn_success_maps_to_started_response() {
        let target = sample_target();
        let response = map_turn_start_result(
            &target,
            Ok(HeadlessTurnStartOutcome {
                turn_id: "discord:111:222".to_string(),
                status: super::super::router::HeadlessTurnStartStatus::Started,
            }),
        )
        .expect("started outcome maps to response");
        let value = response.to_value();
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["to_agent_id"], "adk-dashboard");
        assert_eq!(value["channel_id"], "111");
        assert_eq!(value["channel_kind"], "cc");
        assert_eq!(value["turn_id"], "discord:111:222");
        assert_eq!(value["status"], "started");
    }

    #[test]
    fn handoff_turn_conflict_maps_to_409() {
        let target = sample_target();
        let error = map_turn_start_result(
            &target,
            Err(HeadlessTurnStartError::Conflict(
                "turn already active for this agent mailbox".to_string(),
            )),
        )
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::CONFLICT);
        assert_eq!(error.body()["status"], "conflict");
        assert_eq!(
            error.body()["error"],
            "turn already active for this agent mailbox"
        );
    }

    #[test]
    fn handoff_turn_internal_maps_to_503() {
        let target = sample_target();
        let error = map_turn_start_result(
            &target,
            Err(HeadlessTurnStartError::Internal(
                "runtime unavailable".to_string(),
            )),
        )
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.body()["error"], "runtime unavailable");
    }
}
