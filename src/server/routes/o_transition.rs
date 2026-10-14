//! `POST /api/o/transition`: an operator's Inspect of the transition cleanup the channel's writer
//! actor would do, or its Apply under the scope digest that Inspect gave. This handler is the
//! cleanup's only production sender.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header::ORIGIN};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::services::discord::health::o_transition::{Target, resolve_context};
use crate::services::tui_o::writer::host::{Readiness, process_readiness};
use crate::services::tui_o::writer::transition_port::{
    Context, Operation, Request, ScopeDigest, Termination,
};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Asked {
    Inspect,
    Apply,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Body {
    operation: Asked,
    channel: u64,
    session_key: String,
    execution_nonce: String,
    cutoff: u64,
    job_id: String,
    operator: String,
    reason: String,
    termination: Option<Termination>,
    /// The digest the confirming Inspect gave, required by an Apply and refused with an Inspect.
    scope_digest: Option<String>,
}

pub(crate) type Answer = (StatusCode, Json<Value>);

fn refused(status: StatusCode, result: &str, detail: &str) -> Answer {
    (
        status,
        Json(json!({"ok": false, "result": result, "detail": detail})),
    )
}

/// A loopback peer sending exactly one Origin, a loopback URL on this server's port; a configured
/// token neither replaces nor relaxes it.
fn local_operator(peer: &SocketAddr, headers: &HeaderMap, port: u16) -> bool {
    let mut origins = headers.get_all(ORIGIN).iter();
    let origin = origins.next().and_then(|origin| origin.to_str().ok());
    peer.ip().is_loopback()
        && origins.next().is_none()
        && origin
            .is_some_and(|origin| crate::utils::loopback_url::is_loopback_url(origin, Some(port)))
}

/// Peer and Origin, body, operation and digest, dependencies, then the context: nothing before the
/// Origin check reads PG or the runtime, and an Apply without a well-formed digest reaches neither.
pub(super) async fn transition(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Body>, JsonRejection>,
) -> Answer {
    if !local_operator(&peer, &headers, state.config.server.port) {
        return refused(StatusCode::FORBIDDEN, "forbidden", "local operator only");
    }
    let Ok(Json(body)) = body else {
        return refused(StatusCode::BAD_REQUEST, "bad_request", "invalid body");
    };
    let fields = [
        &body.session_key,
        &body.execution_nonce,
        &body.job_id,
        &body.operator,
    ];
    if body.channel == 0
        || fields
            .iter()
            .chain([&&body.reason])
            .any(|f| f.trim().is_empty())
    {
        return refused(StatusCode::BAD_REQUEST, "bad_request", "empty field");
    }
    let digest = match (&body.operation, body.scope_digest.as_deref()) {
        (Asked::Inspect, None) => None,
        (Asked::Apply, Some(shown)) => match ScopeDigest::parse(shown) {
            Ok(digest) => Some(digest),
            Err(detail) => return refused(StatusCode::BAD_REQUEST, "bad_request", &detail),
        },
        (Asked::Inspect, Some(_)) => {
            let detail = "an inspect takes no scope digest";
            return refused(StatusCode::BAD_REQUEST, "bad_request", detail);
        }
        (Asked::Apply, None) => {
            let detail = "an apply needs the scope digest of its inspect";
            return refused(StatusCode::BAD_REQUEST, "bad_request", detail);
        }
    };
    let (Some(registry), Some(pool)) = (state.health_registry.as_ref(), state.pg_pool.as_ref())
    else {
        let detail = "runtime unavailable";
        return refused(StatusCode::SERVICE_UNAVAILABLE, "unavailable", detail);
    };
    let target = Target {
        channel: body.channel,
        session_key: body.session_key,
        nonce: body.execution_nonce,
    };
    let context = match resolve_context(registry, pool, &target).await {
        Ok(context) => context,
        Err(detail) => return refused(StatusCode::CONFLICT, "refused", &detail),
    };
    let request = Request {
        channel: body.channel,
        cutoff: body.cutoff,
        job_id: body.job_id,
        operator: body.operator,
        at: chrono::Utc::now(),
        reason: body.reason,
        termination: body.termination,
    };
    let operation = match digest {
        None => Operation::Inspect(request),
        Some(digest) => Operation::Apply(request, digest),
    };
    send(&process_readiness(), body.channel, context, operation).await
}

/// Sends the operation to the channel's running actor and answers with its reply only while the
/// run it went to is still the channel's current one.
pub(crate) async fn send(
    readiness: &Readiness,
    channel: u64,
    context: Context,
    operation: Operation,
) -> Answer {
    let done = match operation {
        Operation::Inspect(_) => "inspected",
        Operation::Apply(..) => "applied",
    };
    let Some(port) = readiness.transition(channel) else {
        let detail = "the channel's writer takes no request now";
        return refused(StatusCode::CONFLICT, "actor_unavailable", detail);
    };
    #[cfg(test)]
    test_hooks::SENDS.with(|count| count.set(count.get() + 1));
    let reply = match port.request(operation, context).await {
        Ok(reply) => reply,
        Err(detail) => return refused(StatusCode::BAD_GATEWAY, "reply_lost", &detail),
    };
    #[cfg(test)]
    test_hooks::replied();
    let consumed = readiness.with_transition_reply(channel, &port, reply, |reply| {
        let ok = reply.refused.is_none();
        let result = if ok { done } else { "actor_refused" };
        let status = if ok {
            StatusCode::OK
        } else {
            StatusCode::CONFLICT
        };
        (status, json!({"ok": ok, "result": result, "reply": reply}))
    });
    match consumed {
        Ok((status, answer)) => (status, Json(answer)),
        Err(result) => refused(
            StatusCode::CONFLICT,
            result,
            "the run that replied is not current",
        ),
    }
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// Requests this thread's handlers sent to an actor.
        pub(crate) static SENDS: Cell<usize> = const { Cell::new(0) };
        static REPLIED: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    }

    /// Runs `probe` once, after the next reply arrives and before it is consumed.
    pub(crate) fn on_reply(probe: impl FnOnce() + 'static) {
        REPLIED.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
    }

    pub(super) fn replied() {
        if let Some(probe) = REPLIED.with(|slot| slot.borrow_mut().take()) {
            probe();
        }
    }
}

#[cfg(test)]
#[path = "o_transition_tests.rs"]
mod tests;
