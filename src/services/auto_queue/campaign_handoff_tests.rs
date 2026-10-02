use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::campaigns::CampaignInput;

const REPO: &str = "Owner/Repo";

fn issue(number: i64) -> String {
    format!("https://github.com/owner/repo/issues/{number}")
}

async fn seed_card(pool: &sqlx::PgPool, number: i64, status: &str, agent: Option<&str>) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, github_issue_number)
         VALUES ($1, $1, $2, $3, $4, $5)",
    )
    .bind(format!("card-{number}"))
    .bind(status)
    .bind(agent)
    .bind(REPO)
    .bind(number)
    .execute(pool)
    .await
    .expect("seed campaign node card");
}

/// a(#1) -> b(#2); c has no issue, d's card has no agent, e's issue has no card.
async fn seed(pool: &sqlx::PgPool) -> Campaign {
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-x', 'Agent X', 'claude', '9100')",
    )
    .execute(pool)
    .await
    .expect("seed agent");
    seed_card(pool, 1, "backlog", Some("agent-x")).await;
    seed_card(pool, 2, "backlog", Some("agent-x")).await;
    seed_card(pool, 4, "ready", None).await;
    let node = |id: &str, issue_url: Option<String>, deps: &[&str]| {
        serde_json::json!({"id": id, "title": id, "status": "pending", "stage": "implement",
                           "round": 1, "issue_url": issue_url, "dependencies": deps})
    };
    let input: CampaignInput = serde_json::from_value(serde_json::json!({
        "title": "Handoff", "status": "active", "round": 1,
        "nodes": [node("a", Some(issue(1)), &[]), node("b", Some(issue(2)), &["a"]),
                  node("c", None, &[]), node("d", Some(issue(4)), &[]),
                  node("e", Some(issue(5)), &[])]
    }))
    .expect("campaign fixture");
    campaigns::create(pool, "handoff".into(), input)
        .await
        .expect("create campaign")
}

fn engine(pool: &sqlx::PgPool) -> PolicyEngine {
    PolicyEngine::new_with_pg(&crate::config::Config::default(), Some(pool.clone()))
        .expect("test engine")
}

async fn handoff(pool: &sqlx::PgPool, engine: &PolicyEngine, campaign: &Campaign) -> HandoffReport {
    hand_off_ready_nodes_pg(pool, engine, campaign)
        .await
        .expect("campaign handoff")
}

fn queued_nodes(report: &HandoffReport) -> Vec<&str> {
    report.queued.iter().map(|q| q.node_id.as_str()).collect()
}

async fn entry_for(pool: &sqlx::PgPool, card_id: &str) -> Option<(String, String, i64)> {
    sqlx::query_as(
        "SELECT run_id, status, COALESCE(thread_group, 0)::BIGINT FROM auto_queue_entries
         WHERE kanban_card_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .expect("load entry")
}

#[tokio::test]
async fn postgres_campaign_hands_off_only_nodes_whose_dependencies_are_done_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;

    let first = handoff(&pool, &engine, &campaign).await;
    assert_eq!(queued_nodes(&first), ["a"]);
    let waiting: Vec<_> = first
        .waiting
        .iter()
        .map(|w| (w.node_id.as_str(), w.reason))
        .collect();
    assert_eq!(
        waiting,
        [
            ("c", "no_issue_card"),
            ("d", "no_assigned_agent"),
            ("e", "no_issue_card")
        ]
    );
    let (run_id, entry_status, group) = entry_for(&pool, "card-1").await.expect("a queued");
    assert_eq!((entry_status.as_str(), group), ("pending", 0));
    let run: (String, String, String, Option<String>, String) = sqlx::query_as(
        "SELECT r.status, r.ai_model, r.review_mode, r.agent_id, c.status
         FROM auto_queue_runs r, kanban_cards c WHERE r.id = $1 AND c.id = 'card-1'",
    )
    .bind(&run_id)
    .fetch_one(&pool)
    .await
    .expect("campaign run");
    let expected = ("active", "campaign", "disabled", Some("agent-x"), "ready");
    assert_eq!(
        (
            run.0.as_str(),
            run.1.as_str(),
            run.2.as_str(),
            run.3.as_deref(),
            run.4.as_str()
        ),
        expected,
        "a campaign run without phase gates, and the backlog card prepared like generate does"
    );

    let again = handoff(&pool, &engine, &campaign).await;
    assert!(again.queued.is_empty(), "a queued node is not queued twice");

    sqlx::query(
        "WITH card AS (UPDATE kanban_cards SET status = 'done' WHERE id = 'card-1')
         UPDATE auto_queue_entries SET status = 'done' WHERE kanban_card_id = 'card-1'",
    )
    .execute(&pool)
    .await
    .expect("finish card-1 and its entry");
    let mut held = campaign.clone();
    held.nodes[0].input.status = NodeStatus::Failed;
    let held_report = handoff(&pool, &engine, &held).await;
    assert!(
        held_report.queued.is_empty(),
        "a person's failed verdict outranks the finished card"
    );
    let next = handoff(&pool, &engine, &campaign).await;
    assert_eq!(
        queued_nodes(&next),
        ["b"],
        "a finished card satisfies its dependents"
    );
    let (b_run, _, b_group) = entry_for(&pool, "card-2").await.expect("b queued");
    assert_eq!(
        (b_run.as_str(), b_group),
        (run_id.as_str(), 1),
        "joins the live run in a new lane"
    );

    sqlx::query("UPDATE auto_queue_entries SET status = 'failed' WHERE kanban_card_id = 'card-2'")
        .execute(&pool)
        .await
        .expect("fail b");
    let stopped = handoff(&pool, &engine, &campaign).await;
    assert!(stopped.queued.is_empty());
    assert!(
        stopped
            .waiting
            .iter()
            .any(|w| w.node_id == "b" && w.reason == "previous_attempt_stopped"),
        "a stopped attempt waits for a person instead of looping"
    );

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_campaign_handoff_waits_while_the_agent_run_is_paused_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('held', $1, 'agent-x', 'paused')",
    )
    .bind(REPO)
    .execute(&pool)
    .await
    .expect("seed paused run");

    let report = handoff(&pool, &engine, &campaign).await;
    assert!(report.queued.is_empty());
    assert!(
        report
            .waiting
            .iter()
            .any(|w| w.node_id == "a" && w.reason == "run_paused"),
        "a paused queue is the operator's hold, not a reason to start a second run"
    );
    assert!(entry_for(&pool, "card-1").await.is_none());

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_card_terminal_hook_hands_off_opted_in_campaigns_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let engine = engine(&pool);
    let mut campaign = seed(&pool).await;
    let first = handoff(&pool, &engine, &campaign).await;
    assert_eq!(queued_nodes(&first), ["a"]);
    let mut input: CampaignInput =
        serde_json::from_value(serde_json::to_value(&campaign).expect("encode")).expect("decode");
    input.auto_queue = Some(true);
    campaign = campaigns::replace(&pool, &campaign.id, campaign.revision, input)
        .await
        .expect("opt in");
    assert!(campaign.auto_queue);

    sqlx::query("UPDATE kanban_cards SET status = 'done' WHERE id = 'card-1'")
        .execute(&pool)
        .await
        .expect("finish card-1");
    let hook_pool = pool.clone();
    tokio::task::spawn_blocking(move || {
        crate::kanban::fire_transition_hooks_with_backends(
            Some(&hook_pool),
            &engine,
            "card-1",
            "review",
            "done",
        )
    })
    .await
    .expect("terminal hooks");
    assert!(
        entry_for(&pool, "card-2").await.is_some(),
        "finishing a's card queues b without another request"
    );

    pool.close().await;
    fixture.drop().await;
}
