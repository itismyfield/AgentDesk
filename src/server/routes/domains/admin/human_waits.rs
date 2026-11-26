use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    db::server_campaigns as store,
    error::{AppError, AppResult, ErrorCode},
    services::campaign_execution::{
        model::{CampaignControl, CampaignState},
        view,
    },
};

use super::AppState;

const PAGE_SIZE: i64 = 100;

#[derive(Serialize)]
struct HumanWait {
    source: &'static str,
    campaign_id: String,
    node_id: String,
    title: String,
    campaign_title: String,
    issue_url: String,
    human_status: Value,
    wait_started_at: Option<DateTime<Utc>>,
}

pub(super) async fn list_human_waits(
    State(state): State<AppState>,
) -> AppResult<impl IntoResponse> {
    let pool = state.pg_pool_ref().ok_or_else(pg_unavailable)?;
    let as_of = Utc::now();
    let mut offset = 0;
    let mut seen = HashSet::new();
    let mut waits = Vec::new();

    loop {
        let mut page = store::list(pool, PAGE_SIZE, offset)
            .await
            .map_err(read_error)?;
        let page_len = page.len() as i64;
        // An insert before the offset can repeat a row. Keep its first snapshot.
        page.retain(|campaign| seen.insert(campaign.id.clone()));
        waits.extend(project_page(pool, &page, as_of).await?);
        if page_len < PAGE_SIZE {
            break;
        }
        offset += page_len;
    }

    waits.sort_unstable_by(|left, right| {
        (&left.campaign_id, &left.node_id).cmp(&(&right.campaign_id, &right.node_id))
    });
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "waits": waits })),
    ))
}

async fn project_page(
    pool: &PgPool,
    page: &[CampaignState],
    as_of: DateTime<Utc>,
) -> AppResult<Vec<HumanWait>> {
    let views: Vec<_> = page
        .iter()
        .filter(|campaign| campaign.control == CampaignControl::Running)
        .map(|campaign| (campaign.definition.input(), view::view(campaign, as_of)))
        .collect();
    let requests: Vec<_> = views
        .iter()
        .flat_map(|(_, view)| {
            view.nodes.iter().filter_map(|node| {
                Some(store::ApprovalOpeningRequest {
                    campaign_id: view.campaign_id,
                    event_sequence: view.last_event_sequence,
                    binding: node.runtime.pending_gate.as_ref()?.binding?,
                })
            })
        })
        .collect();
    let opening_times = store::approval_opening_times(pool, &requests)
        .await
        .map_err(read_error)?;

    let mut waits = Vec::new();
    for (definition, view) in views {
        let specs: HashMap<_, _> = definition
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n))
            .collect();
        let campaign_openings = opening_times.get(view.campaign_id);
        for node in view.nodes {
            let Some(human_status) = node.runtime.human_status else {
                continue;
            };
            let Some(spec) = specs.get(node.id) else {
                continue;
            };
            waits.push(HumanWait {
                source: "server",
                campaign_id: view.campaign_id.to_owned(),
                node_id: node.id.to_owned(),
                title: spec.title.clone(),
                campaign_title: definition.title.clone(),
                issue_url: spec.issue_url.clone(),
                human_status: json!(human_status),
                wait_started_at: campaign_openings.and_then(|times| times.get(node.id).copied()),
            });
        }
    }
    Ok(waits)
}

fn pg_unavailable() -> AppError {
    AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Database,
        "canonical PostgreSQL pool unavailable",
    )
}

fn read_error(error: store::StoreError) -> AppError {
    tracing::error!(%error, "human waits database read failed");
    match error {
        store::StoreError::Sql(
            sqlx::Error::PoolClosed | sqlx::Error::PoolTimedOut | sqlx::Error::Io(_),
        ) => pg_unavailable(),
        _ => AppError::internal("human waits database read failed").with_code(ErrorCode::Database),
    }
}

#[cfg(test)]
#[path = "human_waits_tests.rs"]
mod tests;
