//! Read-only host checks for a policy page, run on each row's current owner.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::db::dispatched_sessions::hosted_execution::{CleanupRow, load_idle_cleanup_rows_pg};
use crate::services::session_forwarding::{
    ForwardCallerContext, ForwardResolution, enforce_receiver_fence, is_forwarded_request,
    resolve_forward_target, trusted_request,
};
use crate::services::tmux_turn_liveness::cleanup_host::idle_cleanup_support;

const PAGE_KEYS: usize = 64;
const ENDPOINT: &str = "/api/sessions/idle-cleanup-candidates";
type DecisionMap = BTreeMap<String, Decision>;
type PageRows = Vec<(CleanupRow, Option<String>)>;
type Error = (StatusCode, Json<serde_json::Value>);

#[derive(Serialize, Deserialize)]
pub(crate) struct CandidatePage {
    keys: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct CandidateReply {
    decisions: DecisionMap,
}

#[derive(Serialize, Deserialize)]
struct Decision {
    supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl Decision {
    fn refused(reason: &str) -> Self {
        Self {
            supported: false,
            reason: Some(reason.to_owned()),
        }
    }

    fn for_row(row: &CleanupRow) -> Self {
        match idle_cleanup_support(row) {
            Ok(()) => Self {
                supported: true,
                reason: None,
            },
            Err(refusal) => Self::refused(refusal.candidate_reason()),
        }
    }
}

pub(crate) async fn idle_cleanup_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(page): Json<CandidatePage>,
) -> Result<Json<CandidateReply>, Error> {
    if page.keys.len() > PAGE_KEYS || page.keys.iter().any(|key| key.trim().is_empty()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid candidate page"})),
        ));
    }
    let Some(pool) = state.pg_pool_ref() else {
        return Err(unavailable("postgres pool unavailable"));
    };
    let rows = load_idle_cleanup_rows_pg(pool, &page.keys)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "idle cleanup candidate rows unavailable");
            unavailable("candidate rows unavailable")
        })?;
    let context = ForwardCallerContext::from(&state);
    let mut decisions: DecisionMap = page
        .keys
        .into_iter()
        .map(|key| (key, Decision::refused("host_row_missing_or_conflict")))
        .collect();
    if is_forwarded_request(&headers) {
        decisions.extend(local_decisions(
            rows,
            &headers,
            context.cluster_instance_id.as_deref(),
        ));
        return Ok(Json(CandidateReply { decisions }));
    }

    let mut groups = BTreeMap::<Option<String>, PageRows>::new();
    for (row, owner) in rows {
        if let Some(key) = &row.session_key {
            decisions.insert(key.clone(), Decision::refused("host_owner_unavailable"));
        }
        let group = owner
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        groups
            .entry(group.map(str::to_owned))
            .or_default()
            .push((row, owner));
    }
    let local_id = context.cluster_instance_id.as_deref();
    let mut remote = BTreeMap::new();
    for (owner, rows) in groups {
        if owner.as_deref().is_none()
            || local_id.is_none()
            || owner.as_deref() == local_id.map(str::trim)
        {
            decisions.extend(local_decisions(rows, &headers, local_id));
        } else {
            remote.insert(owner, rows);
        }
    }
    // A slow owner must leave time for the eligible rows on healthy owners.
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    let pending = stream::iter(remote)
        .map(|(owner, rows)| remote_decisions(&context, pool, owner, rows))
        .buffer_unordered(8);
    tokio::pin!(pending);
    loop {
        tokio::select! {
            biased;
            answer = pending.next() => match answer {
                Some(Some(answer)) => decisions.extend(answer),
                Some(None) => {},
                None => break,
            },
            () = &mut deadline => break,
        }
    }
    Ok(Json(CandidateReply { decisions }))
}

fn local_decisions(rows: PageRows, headers: &HeaderMap, local_id: Option<&str>) -> DecisionMap {
    rows.into_iter()
        .filter_map(|(row, owner)| {
            let decision = if enforce_receiver_fence(headers, owner.as_deref(), local_id).is_ok() {
                Decision::for_row(&row)
            } else {
                Decision::refused("session_forward_owner_conflict")
            };
            row.session_key.map(|key| (key, decision))
        })
        .collect()
}

async fn remote_decisions(
    context: &ForwardCallerContext,
    pool: &sqlx::PgPool,
    owner: Option<String>,
    rows: PageRows,
) -> Option<DecisionMap> {
    let ForwardResolution::Forward(target) =
        resolve_forward_target(context, owner.as_deref(), pool).await
    else {
        return None;
    };
    let page = CandidatePage {
        keys: rows
            .into_iter()
            .filter_map(|(row, _)| row.session_key)
            .collect(),
    };
    let request = trusted_request(context, &target, reqwest::Method::POST, ENDPOINT).ok()?;
    let mut reply = request
        .json(&page)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<CandidateReply>()
        .await
        .ok()?;
    Some(
        page.keys
            .into_iter()
            .filter_map(|key| reply.decisions.remove(&key).map(|decision| (key, decision)))
            .collect(),
    )
}

fn unavailable(message: &str) -> Error {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": message})),
    )
}
