//! The campaign DAG decides which nodes may start; auto-queue only runs them.
//! Reads the ledger and never writes it: a queued node's card carries its progress.
use super::*;
use std::collections::BTreeMap;

use crate::db::campaigns::{self, Campaign, NodeStatus};
use crate::engine::PolicyEngine;

/// Lanes a campaign-created run may work at once, the explicit-group default of generate.
const CAMPAIGN_RUN_MAX_CONCURRENT: i64 = 4;
const CAMPAIGN_AI_MODEL: &str = "campaign";

#[derive(Debug, Default, Serialize)]
pub(crate) struct HandoffReport {
    pub queued: Vec<QueuedNode>,
    /// Nodes whose prerequisites are done but that something else holds back.
    pub waiting: Vec<WaitingNode>,
}

#[derive(Debug, Serialize)]
pub(crate) struct QueuedNode {
    pub node_id: String,
    pub card_id: String,
    pub run_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct WaitingNode {
    pub node_id: String,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(sqlx::FromRow)]
struct NodeCard {
    key_repo: String,
    issue_number: i64,
    card_id: String,
    repo_id: Option<String>,
    status: String,
    assigned_agent_id: Option<String>,
    busy: bool,
    latest_entry_status: Option<String>,
}

struct Candidate<'a> {
    node_id: &'a str,
    card: &'a NodeCard,
    agent: String,
}

impl HandoffReport {
    fn wait(&mut self, node_id: &str, reason: &'static str, detail: Option<String>) {
        self.waiting.push(WaitingNode {
            node_id: node_id.to_owned(),
            reason,
            detail,
        });
    }
}

async fn load_node_cards(
    pool: &sqlx::PgPool,
    campaign: &Campaign,
) -> Result<HashMap<(String, i64), NodeCard>, String> {
    let keys: HashSet<(String, i64)> = campaign
        .nodes
        .iter()
        .filter_map(|node| {
            node.input
                .issue_url
                .as_deref()
                .and_then(campaigns::issue_ref)
        })
        .map(|(repo, number)| (repo.to_lowercase(), number))
        .collect();
    let (repos, numbers): (Vec<String>, Vec<i64>) = keys.into_iter().unzip();
    let rows: Vec<NodeCard> = sqlx::query_as(
        "SELECT DISTINCT ON (k.repo_id, k.issue_number)
                k.repo_id AS key_repo, k.issue_number, c.id AS card_id, c.repo_id,
                COALESCE(c.status, 'backlog') AS status, c.assigned_agent_id,
                EXISTS (SELECT 1 FROM task_dispatches d WHERE d.kanban_card_id = c.id
                          AND d.status IN ('pending', 'dispatched'))
                OR EXISTS (SELECT 1 FROM auto_queue_entries e WHERE e.kanban_card_id = c.id
                          AND e.status IN ('pending', 'dispatched')) AS busy,
                (SELECT e.status FROM auto_queue_entries e WHERE e.kanban_card_id = c.id
                 ORDER BY e.created_at DESC, e.id DESC LIMIT 1) AS latest_entry_status
         FROM UNNEST($1::TEXT[], $2::BIGINT[]) AS k(repo_id, issue_number)
         JOIN kanban_cards c
           ON LOWER(c.repo_id) = k.repo_id AND c.github_issue_number = k.issue_number
         ORDER BY k.repo_id, k.issue_number, c.updated_at DESC NULLS LAST, c.id",
    )
    .bind(&repos)
    .bind(&numbers)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load campaign node cards: {error}"))?;
    Ok(rows
        .into_iter()
        .map(|row| ((row.key_repo.clone(), row.issue_number), row))
        .collect())
}

/// Card id -> its effective pipeline, resolved once per (repo, agent) pair.
async fn card_pipelines<'a>(
    pool: &sqlx::PgPool,
    cards: &'a HashMap<(String, i64), NodeCard>,
) -> HashMap<&'a str, std::sync::Arc<crate::pipeline::PipelineConfig>> {
    let mut by_owner = HashMap::new();
    let mut by_card = HashMap::new();
    for card in cards.values() {
        let owner = (card.repo_id.as_deref(), card.assigned_agent_id.as_deref());
        if !by_owner.contains_key(&owner) {
            let pipeline = crate::pipeline::resolve_for_card_pg(pool, owner.0, owner.1).await;
            by_owner.insert(owner, std::sync::Arc::new(pipeline));
        }
        by_card.insert(card.card_id.as_str(), by_owner[&owner].clone());
    }
    by_card
}

/// Queues every pending node whose dependencies are done, by ledger or by a terminal card.
pub(crate) async fn hand_off_ready_nodes_pg(
    pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    campaign: &Campaign,
) -> Result<HandoffReport, String> {
    if !campaign
        .nodes
        .iter()
        .any(|node| node.input.status == NodeStatus::Pending)
    {
        return Ok(HandoffReport::default());
    }
    crate::pipeline::ensure_loaded();
    let cards = load_node_cards(pool, campaign).await?;
    let pipelines = card_pipelines(pool, &cards).await;
    let terminal = |card: &NodeCard| pipelines[card.card_id.as_str()].is_terminal(&card.status);
    let card_of = |node: &campaigns::Node| {
        let (repo, number) = node
            .input
            .issue_url
            .as_deref()
            .and_then(campaigns::issue_ref)?;
        cards.get(&(repo.to_lowercase(), number))
    };
    let by_id: HashMap<&str, &campaigns::Node> = campaign
        .nodes
        .iter()
        .map(|node| (node.input.id.as_str(), node))
        .collect();
    // A node a person saved as blocked or failed holds its dependents even if its card finished.
    let finished = |node: &campaigns::Node| match node.input.status {
        NodeStatus::Completed | NodeStatus::Skipped => true,
        NodeStatus::Pending | NodeStatus::Running => card_of(node).is_some_and(terminal),
        NodeStatus::Blocked | NodeStatus::Failed => false,
    };

    let mut report = HandoffReport::default();
    let mut candidates = Vec::new();
    for node in &campaign.nodes {
        let deps_done = node
            .input
            .dependencies
            .iter()
            .all(|dep| by_id.get(dep.as_str()).is_some_and(|dep| finished(dep)));
        if node.input.status != NodeStatus::Pending || !deps_done {
            continue;
        }
        let node_id = node.input.id.as_str();
        let Some(card) = card_of(node) else {
            report.wait(node_id, "no_issue_card", node.input.issue_url.clone());
            continue;
        };
        if terminal(card) || card.busy {
            continue;
        }
        if let Some(status) = card.latest_entry_status.as_deref().filter(|status| {
            matches!(
                *status,
                "failed" | "skipped" | "user_cancelled" | "cancelled"
            )
        }) {
            report.wait(node_id, "previous_attempt_stopped", Some(status.to_owned()));
            continue;
        }
        let Some(agent) = card
            .assigned_agent_id
            .clone()
            .filter(|a| !a.trim().is_empty())
        else {
            report.wait(node_id, "no_assigned_agent", None);
            continue;
        };
        if card.status == "backlog" {
            let input = crate::services::auto_queue::PrepareGenerateInput {
                repo: card.repo_id.clone(),
                agent_id: Some(agent.clone()),
                issue_numbers: Some(vec![card.issue_number]),
            };
            if let Err(error) = crate::services::auto_queue::AutoQueueService::new(engine.clone())
                .prepare_generate_cards_with_pg(pool, &input)
                .await
            {
                report.wait(node_id, "not_enqueueable", Some(error.message().to_owned()));
                continue;
            }
        } else if !crate::services::auto_queue::enqueueable_states_for(
            &pipelines[card.card_id.as_str()],
        )
        .contains(&card.status)
        {
            report.wait(node_id, "card_not_ready", Some(card.status.clone()));
            continue;
        }
        candidates.push(Candidate {
            node_id,
            card,
            agent,
        });
    }
    if !candidates.is_empty() {
        enqueue_candidates(pool, campaign, &candidates, &mut report).await?;
    }
    Ok(report)
}

/// One transaction per (repo, agent) group, so a handoff never holds two run tokens at once.
async fn enqueue_candidates(
    pool: &sqlx::PgPool,
    campaign: &Campaign,
    candidates: &[Candidate<'_>],
    report: &mut HandoffReport,
) -> Result<(), String> {
    let mut groups: BTreeMap<(String, String), Vec<&Candidate>> = BTreeMap::new();
    for candidate in candidates {
        let repo = candidate.card.repo_id.clone().unwrap_or_default();
        groups
            .entry((repo, candidate.agent.clone()))
            .or_default()
            .push(candidate);
    }
    for ((repo, agent), members) in groups {
        enqueue_group(pool, campaign, &repo, &agent, &members, report).await?;
    }
    Ok(())
}

enum PickedRun {
    Active(String),
    Paused(String),
    /// Finished while this waited for its token; retry in a fresh transaction.
    Gone,
}

/// The agent's live run like generate would find it, or a new one.
async fn pick_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    campaign: &Campaign,
    repo: &str,
    agent: &str,
) -> Result<PickedRun, String> {
    let db = |error: sqlx::Error| format!("campaign handoff: {error}");
    let live: Option<String> = sqlx::query_scalar(
        "SELECT id FROM auto_queue_runs
         WHERE status IN ('active', 'paused')
           AND (repo = $1 OR repo IS NULL OR repo = '')
           AND (agent_id = $2 OR agent_id IS NULL OR agent_id = '')
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(repo)
    .bind(agent)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let Some(run_id) = live else {
        let run_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, review_mode, status,
                 ai_model, ai_rationale, unified_thread, max_concurrent_threads)
             VALUES ($1, NULLIF($2, ''), $3, $4, 'active', $5, $6, FALSE, 1)",
        )
        .bind(&run_id)
        .bind(repo)
        .bind(agent)
        .bind(AUTO_QUEUE_REVIEW_MODE_DISABLED)
        .bind(CAMPAIGN_AI_MODEL)
        .bind(format!("campaign {}: {}", campaign.id, campaign.title))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        return Ok(PickedRun::Active(run_id));
    };
    // Completion and cancel read the run's entries under this token.
    crate::db::auto_queue::acquire_run_advisory_xact_locks_on_pg_tx(
        tx,
        std::slice::from_ref(&run_id),
    )
    .await?;
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind(&run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?
            .flatten();
    Ok(match status.as_deref() {
        Some("active") => PickedRun::Active(run_id),
        Some("paused") => PickedRun::Paused(run_id),
        _ => PickedRun::Gone,
    })
}

async fn enqueue_group(
    pool: &sqlx::PgPool,
    campaign: &Campaign,
    repo: &str,
    agent: &str,
    members: &[&Candidate<'_>],
    report: &mut HandoffReport,
) -> Result<(), String> {
    let db = |error: sqlx::Error| format!("campaign handoff: {error}");
    // One run token per transaction: a finished pick is retried after rollback, never stacked.
    let (mut tx, run_id) = loop {
        let mut tx = pool.begin().await.map_err(db)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('campaign-handoff'))")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        // A save that committed first wins; a later save waits until these entries commit.
        let revision: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM campaigns WHERE id = $1 FOR SHARE")
                .bind(&campaign.id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        if revision != Some(campaign.revision) {
            for member in members {
                report.wait(member.node_id, "campaign_changed", None);
            }
            return Ok(());
        }
        match pick_run(&mut tx, campaign, repo, agent).await? {
            PickedRun::Active(run_id) => break (tx, run_id),
            PickedRun::Paused(run_id) => {
                for member in members {
                    report.wait(member.node_id, "run_paused", Some(run_id.clone()));
                }
                return Ok(());
            }
            PickedRun::Gone => tx.rollback().await.map_err(db)?,
        }
    };
    let (mut next_group, phase): (i64, i64) = sqlx::query_as(
        "SELECT (COALESCE(MAX(COALESCE(thread_group, 0)), -1) + 1)::BIGINT,
                COALESCE(MIN(batch_phase) FILTER (WHERE status IN ('pending', 'dispatched')),
                         MAX(batch_phase), 0)::BIGINT
         FROM auto_queue_entries WHERE run_id = $1",
    )
    .bind(&run_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    for member in members {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM auto_queue_entries WHERE kanban_card_id = $1
                              AND status IN ('pending', 'dispatched'))
                 OR EXISTS (SELECT 1 FROM task_dispatches WHERE kanban_card_id = $1
                              AND status IN ('pending', 'dispatched'))",
        )
        .bind(&member.card.card_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if busy {
            continue;
        }
        let inserted = sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id,
                 priority_rank, thread_group, batch_phase, reason)
             VALUES ($1, $2, $3, $4, 0, $5, $6, $7)
             ON CONFLICT (run_id, kanban_card_id) WHERE status NOT IN ('skipped', 'cancelled')
             DO NOTHING",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&run_id)
        .bind(&member.card.card_id)
        .bind(agent)
        .bind(next_group)
        .bind(phase)
        .bind(format!("campaign {} node {}", campaign.id, member.node_id))
        .execute(&mut *tx)
        .await
        .map_err(db)?
        .rows_affected();
        if inserted == 0 {
            report.wait(member.node_id, "already_in_run", Some(run_id.clone()));
            continue;
        }
        next_group += 1;
        report.queued.push(QueuedNode {
            node_id: member.node_id.to_owned(),
            card_id: member.card.card_id.clone(),
            run_id: run_id.clone(),
        });
    }
    sqlx::query(
        "UPDATE auto_queue_runs SET
             thread_group_count = (SELECT GREATEST(COUNT(DISTINCT COALESCE(thread_group, 0)), 1)
                                   FROM auto_queue_entries WHERE run_id = $1),
             max_concurrent_threads = CASE WHEN ai_model = $2 THEN GREATEST(
                 COALESCE(max_concurrent_threads, 1),
                 LEAST((SELECT COUNT(DISTINCT COALESCE(thread_group, 0))
                        FROM auto_queue_entries
                        WHERE run_id = $1 AND status IN ('pending', 'dispatched')), $3))
             ELSE max_concurrent_threads END
         WHERE id = $1 AND status = 'active'",
    )
    .bind(&run_id)
    .bind(CAMPAIGN_AI_MODEL)
    .bind(CAMPAIGN_RUN_MAX_CONCURRENT)
    .execute(&mut *tx)
    .await
    .map_err(db)?;
    tx.commit().await.map_err(db)
}

/// Every opted-in active campaign hands off what became ready; run by the card-terminal
/// hook and each minute, which catches cards finished outside the hook (GitHub sync).
pub(crate) async fn hand_off_auto_campaigns_pg(
    pool: &sqlx::PgPool,
    engine: &PolicyEngine,
) -> Result<(), String> {
    let active = campaigns::list_auto_queue_active(pool)
        .await
        .map_err(|error| format!("load auto-queue campaigns: {error}"))?;
    for campaign in active {
        match hand_off_ready_nodes_pg(pool, engine, &campaign).await {
            Ok(report) if !report.queued.is_empty() => {
                tracing::info!(campaign = %campaign.id, queued = report.queued.len(), "campaign handed ready nodes to auto-queue");
            }
            Ok(_) => {}
            // One broken campaign must not hold back the others.
            Err(error) => {
                tracing::warn!(campaign = %campaign.id, %error, "campaign handoff failed")
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "campaign_handoff_tests.rs"]
mod tests;
