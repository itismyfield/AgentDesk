//! A delegated channel's user stop goes from the gateway to the holder its home row names over
//! the trusted forward; only a matching typed answer counts, and nothing is retried or run here.

use std::future::Future;

use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{ForwardCallerContext, ForwardResolution};
use crate::db::o_channel_homes::{self, HomeState};
use crate::services::cluster::channel_home::{self, HomeIntake, HomeOwnership};
use crate::services::cluster::home_availability::{self, Availability};
use crate::services::provider::ProviderKind;

pub(crate) const CAPABILITY: &str = "home_stop_forwarding_v1";
const ENDPOINT: &str = "/api/internal/home-stop/v1";
const UNOBSERVED: &str = "home_unobserved";
/// Only a home-stop request carries these; a direct cancel body naming one is refused.
const ENVELOPE_FIELDS: [&str; 6] = [
    "v",
    "request_id",
    "expected_holder",
    "home_epoch",
    "intent",
    "surface",
];
/// The typed results a holder answers with; anything else is unconfirmed.
const OUTCOMES: [&str; 6] = [
    "herdr",
    "stopping",
    "already_stopping",
    "host_refused",
    "no_active_turn",
    "refused",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Surface {
    SlashStop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Intent {
    UserStop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HomeStopRequest {
    v: u8,
    request_id: String,
    channel_id: String,
    provider: String,
    expected_holder: String,
    home_epoch: i64,
    surface: Surface,
    intent: Intent,
    force: bool,
}

impl HomeStopRequest {
    /// The request's identity with `outcome`; never a terminal confirmation.
    fn answer(&self, outcome: Value) -> Value {
        let mut answer = json!({
            "v": 1,
            "request_id": self.request_id,
            "channel_id": self.channel_id,
            "provider": self.provider,
            "holder": self.expected_holder,
            "home_epoch": self.home_epoch,
            "terminal_confirmed": false,
        });
        if let (Some(answer), Value::Object(outcome)) = (answer.as_object_mut(), outcome) {
            for (key, value) in outcome {
                answer.entry(key).or_insert(value);
            }
        }
        answer
    }

    fn matches(&self, answer: &Value) -> bool {
        let text = |key: &str| answer.get(key).and_then(Value::as_str);
        answer.get("v").and_then(Value::as_u64) == Some(1)
            && text("request_id") == Some(&self.request_id)
            && text("channel_id") == Some(&self.channel_id)
            && text("provider") == Some(&self.provider)
            && text("holder") == Some(&self.expected_holder)
            && answer.get("home_epoch").and_then(Value::as_i64) == Some(self.home_epoch)
            && answer.get("terminal_confirmed").and_then(Value::as_bool) == Some(false)
            && text("outcome").is_some_and(|outcome| OUTCOMES.contains(&outcome))
            && (mutant("home_answer_payload_unchecked")
                || match text("outcome") {
                    Some("herdr") => {
                        text("delivery").is_some_and(|value| {
                            ["sent", "indeterminate", "not_sent"].contains(&value)
                        }) && text("intent").is_some_and(|value| {
                            ["recorded", "already_recorded", "refused"].contains(&value)
                        }) && answer
                            .get("effect_started")
                            .and_then(Value::as_bool)
                            .is_some()
                    }
                    Some("refused") => {
                        text("reason").is_some()
                            && answer.get("effect_started").and_then(Value::as_bool) == Some(false)
                    }
                    Some("stopping") => {
                        answer.get("effect_started").and_then(Value::as_bool) == Some(true)
                    }
                    _ => answer.get("effect_started").and_then(Value::as_bool) == Some(false),
                })
    }
}

/// What a gateway's `/stop` does for a channel.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GatewayStop {
    /// Not delegated away from this node: the existing stop path runs.
    Legacy,
    /// Refused before anything was sent.
    Refused(&'static str),
    /// The holder's matching typed answer.
    Confirmed(Value),
    /// Sent without a matching answer; nothing is resent, redirected or stopped here.
    Unconfirmed(&'static str),
}

fn mutant(name: &str) -> bool {
    #[cfg(test)]
    return channel_home::command_mutant(name);
    #[cfg(not(test))]
    {
        let _ = name;
        false
    }
}

fn local_id(ctx: &ForwardCallerContext) -> Option<&str> {
    let local = ctx.cluster_instance_id.as_deref().map(str::trim);
    local.filter(|id| !id.is_empty())
}

/// Whether a direct cancel body carries the home-stop envelope.
pub(crate) fn names_envelope(body: &[u8]) -> bool {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let object = parsed.as_ref().and_then(Value::as_object);
    object.is_some_and(|object| ENVELOPE_FIELDS.iter().any(|key| object.contains_key(*key)))
}

/// The gateway's authority for a `/stop`: a gate here or delegation off keeps the existing path
/// unread; otherwise the row decides, and a remote holder gets the stop forwarded.
pub(crate) async fn gateway_stop(
    ctx: &ForwardCallerContext,
    channel: u64,
    provider: &str,
) -> GatewayStop {
    let local_only = channel_home::registered_channel(channel).is_some()
        || home_availability::state(provider) == Availability::Off;
    if local_only && !mutant("pg_lookup_for_all_nodes") {
        return GatewayStop::Legacy;
    }
    if let Some(reason) = home_availability::refusal(channel) {
        return GatewayStop::Refused(reason.as_str());
    }
    if mutant("gateway_missing_home_as_legacy") {
        return GatewayStop::Legacy;
    }
    let (Some(pool), Some(local)) = (ctx.pg_pool_ref(), local_id(ctx)) else {
        return GatewayStop::Refused(UNOBSERVED);
    };
    #[cfg(test)]
    TEST_COUNTS.with(|counts| counts.borrow_mut().0 += 1);
    let home = match o_channel_homes::read_home(pool, &channel.to_string()).await {
        Ok(Some(home)) => home,
        Ok(None) => return GatewayStop::Legacy,
        Err(error) => {
            tracing::warn!(channel, %error, "gateway stop refused: home row unreadable");
            return GatewayStop::Refused(UNOBSERVED);
        }
    };
    if home.provider != provider {
        return GatewayStop::Refused("home_provider_mismatch");
    }
    let (HomeState::Worker, Some(holder)) = (home.state, home.holder.clone()) else {
        return GatewayStop::Refused("home_in_transition");
    };
    if holder == local {
        return GatewayStop::Refused(UNOBSERVED);
    }
    let mut target_id = holder.clone();
    if mutant("holder_uses_session_owner") {
        let owner = super::load_cancel_owner(pool, &channel.to_string()).await;
        target_id = owner.ok().flatten().unwrap_or(target_id);
    }
    let target = match resolve_holder(ctx, pool, &target_id).await {
        ForwardResolution::Forward(target) => target,
        _ => return GatewayStop::Refused("holder_unreachable"),
    };
    let request = HomeStopRequest {
        v: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        channel_id: channel.to_string(),
        provider: provider.to_owned(),
        expected_holder: holder,
        home_epoch: home.epoch,
        surface: Surface::SlashStop,
        intent: Intent::UserStop,
        force: false,
    };
    let sent = send(ctx, &target, &request).await;
    tracing::info!(channel, holder = %request.expected_holder, epoch = request.home_epoch,
        request_id = %request.request_id, result = ?sent, "gateway stop forwarded to holder");
    if mutant("origin_local_fallback") && matches!(sent, GatewayStop::Unconfirmed(_)) {
        return GatewayStop::Legacy;
    }
    sent
}

#[cfg(test)]
type RowBarrier = (
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Notify>,
);

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_COUNTS: std::cell::RefCell<(usize, usize)> = const { std::cell::RefCell::new((0, 0)) };
    pub(crate) static AFTER_ROW: std::cell::RefCell<Option<RowBarrier>> = const { std::cell::RefCell::new(None) };
    /// Test holders' origins, reached without the trusted target's address checks.
    pub(crate) static TEST_ORIGINS: std::cell::RefCell<std::collections::BTreeMap<String, String>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

async fn resolve_holder(
    ctx: &ForwardCallerContext,
    pool: &sqlx::PgPool,
    holder: &str,
) -> ForwardResolution {
    #[cfg(test)]
    if let Some(origin) = TEST_ORIGINS.with(|origins| origins.borrow().get(holder).cloned()) {
        let target = super::TrustedForwardTarget::for_test(holder, &origin);
        return target.map_or(ForwardResolution::Local, ForwardResolution::Forward);
    }
    super::resolve_forward_target_with_capability(ctx, Some(holder), pool, CAPABILITY).await
}

async fn send(
    ctx: &ForwardCallerContext,
    target: &super::TrustedForwardTarget,
    request: &HomeStopRequest,
) -> GatewayStop {
    let Ok(builder) = super::trusted_request(ctx, target, reqwest::Method::POST, ENDPOINT) else {
        return GatewayStop::Refused("holder_unreachable");
    };
    #[cfg(test)]
    TEST_COUNTS.with(|counts| counts.borrow_mut().1 += 1);
    let Ok(response) = builder.json(request).send().await else {
        return GatewayStop::Unconfirmed("no_response");
    };
    let status = response.status().as_u16();
    let answer = response.json::<Value>().await.ok();
    #[cfg(test)]
    if status == 409 && mutant("home_409_owner_retry") {
        if let Some(pool) = ctx.pg_pool_ref() {
            if let Ok(Some(owner)) = super::load_cancel_owner(pool, &request.channel_id).await {
                if let ForwardResolution::Forward(target) = resolve_holder(ctx, pool, &owner).await
                {
                    if let Ok(builder) =
                        super::trusted_request(ctx, &target, reqwest::Method::POST, ENDPOINT)
                    {
                        let _ = builder.json(request).send().await;
                    }
                }
            }
        }
    }
    if status == 404 && mutant("unknown_404_as_absent") {
        return GatewayStop::Confirmed(request.answer(json!({"outcome": "no_active_turn"})));
    }
    match (status, answer) {
        (200, Some(answer)) if mutant("old_200_as_sent") || request.matches(&answer) => {
            GatewayStop::Confirmed(answer)
        }
        (200, _) => GatewayStop::Unconfirmed("answer_mismatch"),
        (404, _) => GatewayStop::Unconfirmed("holder_route_missing"),
        (409, _) => GatewayStop::Unconfirmed("holder_conflict"),
        _ => GatewayStop::Unconfirmed("holder_rejected"),
    }
}

/// The holder's side: only a trusted forward naming this node's fresh holder, provider and epoch
/// runs, inside an ordinary command admission, through `run`, this node's own `/stop`.
pub(crate) async fn receive<F, Fut>(
    ctx: &ForwardCallerContext,
    headers: &HeaderMap,
    body: &[u8],
    run: F,
) -> (StatusCode, Value)
where
    F: FnOnce(ProviderKind, u64) -> Fut,
    Fut: Future<Output = Option<Value>>,
{
    if !super::is_forwarded_request(headers) && !mutant("internal_route_skips_trusted_forward") {
        let refusal =
            json!({"error": "home stop needs a trusted forward", "code": "home_stop_untrusted"});
        return (StatusCode::FORBIDDEN, refusal);
    }
    let request = serde_json::from_slice::<HomeStopRequest>(body).ok();
    let request = request.filter(|request| request.v == 1 && !request.force);
    let parsed = request.as_ref().and_then(|request| {
        let channel = request.channel_id.parse::<u64>().ok()?;
        Some((ProviderKind::from_str(&request.provider)?, channel))
    });
    let (Some(request), Some((provider, channel))) = (request, parsed) else {
        let invalid = json!({"error": "invalid home stop request", "code": "home_stop_invalid"});
        return (StatusCode::BAD_REQUEST, invalid);
    };
    let refused = |reason: &str| {
        tracing::warn!(channel, reason, request_id = %request.request_id, "home stop refused");
        let outcome = json!({"outcome": "refused", "reason": reason, "effect_started": false});
        (StatusCode::OK, request.answer(outcome))
    };
    if home_availability::state(&request.provider) == Availability::Off {
        return refused("delegation_off");
    }
    if let Some(reason) = home_availability::refusal(channel) {
        return refused(reason.as_str());
    }
    let (Some(pool), Some(local)) = (ctx.pg_pool_ref(), local_id(ctx)) else {
        return refused(UNOBSERVED);
    };
    let home = match o_channel_homes::read_home(pool, &request.channel_id).await {
        Ok(Some(home)) => home,
        Ok(None) => return refused("home_absent"),
        Err(_) => return refused(UNOBSERVED),
    };
    #[cfg(test)]
    if let Some((arrived, resume)) = AFTER_ROW.with(|barrier| barrier.borrow_mut().take()) {
        arrived.notify_one();
        resume.notified().await;
    }
    let epoch_checked = !mutant("receiver_epoch_ignored");
    if home.provider != request.provider {
        return refused("home_provider_mismatch");
    }
    if home.state != HomeState::Worker {
        return refused("home_in_transition");
    }
    if home.holder.as_deref() != Some(local) || request.expected_holder != local {
        return refused("holder_mismatch");
    }
    if epoch_checked && home.epoch != request.home_epoch {
        return refused("epoch_mismatch");
    }
    if let Err((status, axum::Json(fence))) =
        super::enforce_receiver_fence(headers, home.holder.as_deref(), Some(local))
    {
        return (status, fence);
    }
    match super::load_cancel_owner(pool, &request.channel_id).await {
        Ok(Some(owner)) if owner != local => return refused("stale_session_owner"),
        Err(_) => return refused("session_owner_unobserved"),
        Ok(_) => {}
    }
    let Some(gate) = channel_home::registered(&request.channel_id) else {
        return refused("home_not_registered");
    };
    let Some(permit) = gate.admit_command(provider.as_str()) else {
        let reason = gate.refusal().map(|refusal| refusal.to_string());
        return refused(reason.as_deref().unwrap_or("home_not_held"));
    };
    let held_at_epoch = matches!(gate.ownership(), HomeOwnership::Owned {
        home_epoch, intake: HomeIntake::Open, ..
    } if home_epoch == request.home_epoch);
    if epoch_checked && !mutant("receiver_post_admission_epoch_ignored") && !held_at_epoch {
        drop(permit);
        return refused("epoch_mismatch");
    }
    match channel_home::command_scope(Some(permit), run(provider, channel)).await {
        Some(outcome) => (StatusCode::OK, request.answer(outcome)),
        None => refused("runtime_missing"),
    }
}

#[cfg(test)]
#[path = "home_stop_tests.rs"]
pub(crate) mod tests;
