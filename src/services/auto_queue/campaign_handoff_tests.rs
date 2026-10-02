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

async fn wait_for_lock_waiter(pool: &sqlx::PgPool, query_like: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                            WHERE datname = current_database() AND wait_event_type = 'Lock'
                              AND query LIKE $1)",
        )
        .bind(query_like)
        .fetch_one(pool)
        .await
        .expect("inspect lock waits");
        if waiting {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "handoff never waited on {query_like}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn spawn_handoff(
    pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    campaign: &Campaign,
) -> tokio::task::JoinHandle<HandoffReport> {
    let (pool, engine, campaign) = (pool.clone(), engine.clone(), campaign.clone());
    tokio::spawn(async move { handoff(&pool, &engine, &campaign).await })
}

/// Completion or cancel holds the run token while the handoff picks that run.
async fn run_finishing_mid_handoff_gets_no_entry(final_status: &str) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('live', $1, 'agent-x', 'active')",
    )
    .bind(REPO)
    .execute(&pool)
    .await
    .expect("seed live run");
    let mut holder = pool.begin().await.expect("begin run-token holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'live'))")
        .execute(&mut *holder)
        .await
        .expect("hold run token");

    let task = spawn_handoff(&pool, &engine, &campaign);
    wait_for_lock_waiter(&pool, "%aq_run:%").await;
    sqlx::query("UPDATE auto_queue_runs SET status = $1, completed_at = NOW() WHERE id = 'live'")
        .bind(final_status)
        .execute(&mut *holder)
        .await
        .expect("finish live run");
    holder.commit().await.expect("commit finished run");
    let report = task.await.expect("handoff task");

    assert_eq!(queued_nodes(&report), ["a"]);
    let (run_id, _, _) = entry_for(&pool, "card-1").await.expect("a queued");
    assert_ne!(run_id, "live", "no entry lands in a {final_status} run");
    let run_status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .expect("new run");
    assert_eq!(run_status, "active");

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_campaign_handoff_skips_a_run_completed_while_it_waits_pg() {
    run_finishing_mid_handoff_gets_no_entry("completed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_campaign_handoff_skips_a_run_cancelled_while_it_waits_pg() {
    run_finishing_mid_handoff_gets_no_entry("cancelled").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_handoff_does_not_keep_the_token_of_a_run_that_finished_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at)
         VALUES ('old', $1, 'agent-x', 'active', NOW() - INTERVAL '1 minute'),
                ('new', $1, 'agent-x', 'active', NOW())",
    )
    .bind(REPO)
    .execute(&pool)
    .await
    .expect("seed two live runs");
    let mut finisher = pool.begin().await.expect("begin new-run holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'new'))")
        .execute(&mut *finisher)
        .await
        .expect("hold new run token");
    let mut old_holder = pool.begin().await.expect("begin old-run holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'old'))")
        .execute(&mut *old_holder)
        .await
        .expect("hold old run token");
    let old_holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old_holder)
        .await
        .expect("old holder pid");

    let task = spawn_handoff(&pool, &engine, &campaign);
    wait_for_lock_waiter(&pool, "%aq_run:%").await;
    sqlx::query(
        "UPDATE auto_queue_runs SET status = 'completed', completed_at = NOW() WHERE id = 'new'",
    )
    .execute(&mut *finisher)
    .await
    .expect("finish new run");
    finisher.commit().await.expect("commit finished run");
    // The handoff moves on to the old run and waits for its token.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
    )
    .bind(old_holder_pid)
    .fetch_one(&pool)
    .await
    .expect("inspect blockers")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "handoff never reached the old run"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut probe = pool.begin().await.expect("begin probe");
    let free: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext('aq_run:' || 'new'))")
            .fetch_one(&mut *probe)
            .await
            .expect("probe new run token");
    probe.rollback().await.expect("release probe");
    assert!(
        free,
        "the finished run's token was released before waiting on another"
    );

    old_holder.commit().await.expect("release old run token");
    let report = task.await.expect("handoff task");
    assert_eq!(queued_nodes(&report), ["a"]);
    let (run_id, _, _) = entry_for(&pool, "card-1").await.expect("a queued");
    assert_eq!(run_id, "old");

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_activate_does_not_complete_a_run_the_handoff_is_filling_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    seed(&pool).await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('empty', $1, 'agent-x', 'active')",
    )
    .bind(REPO)
    .execute(&pool)
    .await
    .expect("seed empty run");
    // What the handoff does under the run token: append an entry, then commit.
    let mut appender = pool.begin().await.expect("begin appender");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || 'empty'))")
        .execute(&mut *appender)
        .await
        .expect("hold run token");
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, thread_group, batch_phase)
         VALUES ('appended', 'empty', 'card-1', 'agent-x', 0, 0)",
    )
    .execute(&mut *appender)
    .await
    .expect("append entry");

    let activate = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let ctx = crate::services::auto_queue::AutoQueueLogContext::new().run("empty");
            super::super::activate_command::complete_run_if_empty(&pool, "empty", &ctx)
                .await
                .is_ok()
        })
    };
    wait_for_lock_waiter(&pool, "%aq_run:%").await;
    appender.commit().await.expect("commit appended entry");

    assert!(
        activate.await.expect("activate task"),
        "the run has an entry to dispatch"
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'empty'")
            .fetch_one(&pool)
            .await
            .expect("run status");
    assert_eq!(status, "active");

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_campaign_handoff_drops_nodes_when_a_newer_save_lands_first_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;
    let mut holder = pool.begin().await.expect("begin handoff-lock holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('campaign-handoff'))")
        .execute(&mut *holder)
        .await
        .expect("hold handoff lock");

    let task = spawn_handoff(&pool, &engine, &campaign);
    wait_for_lock_waiter(&pool, "%campaign-handoff%").await;
    let mut paused = serde_json::to_value(&campaign).expect("encode");
    paused["status"] = serde_json::json!("paused");
    let paused: CampaignInput = serde_json::from_value(paused).expect("decode");
    campaigns::replace(&pool, &campaign.id, campaign.revision, paused)
        .await
        .expect("pause while the handoff waits");
    holder.commit().await.expect("release handoff lock");
    let report = task.await.expect("handoff task");

    assert!(
        report.queued.is_empty(),
        "a pause saved first holds back new work"
    );
    assert!(
        report
            .waiting
            .iter()
            .any(|w| w.node_id == "a" && w.reason == "campaign_changed")
    );
    assert!(entry_for(&pool, "card-1").await.is_none());

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_minute_handoff_catches_cards_closed_by_github_sync_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;
    let mut input: CampaignInput =
        serde_json::from_value(serde_json::to_value(&campaign).expect("encode")).expect("decode");
    input.auto_queue = Some(true);
    campaigns::replace(&pool, &campaign.id, campaign.revision, input)
        .await
        .expect("opt in");
    sqlx::query("UPDATE kanban_cards SET status = 'in_progress' WHERE id = 'card-1'")
        .execute(&pool)
        .await
        .expect("a is being worked on");

    let gh_issue = |number: i64, state: &str| crate::github::sync::GhIssue {
        number,
        state: state.to_owned(),
        title: format!("issue {number}"),
        labels: Vec::new(),
        body: None,
        url: Some(issue(number)),
        closed_at: None,
        closed_by_pull_requests_references: Vec::new(),
    };
    let issues = [
        gh_issue(1, "CLOSED"),
        gh_issue(2, "OPEN"),
        gh_issue(4, "OPEN"),
    ];
    let synced = crate::github::sync::sync_github_issues_for_repo_pg(&pool, REPO, &issues)
        .await
        .expect("github sync");
    assert_eq!(synced.closed_count, 1, "closing #1 finishes a's card");

    hand_off_auto_campaigns_pg(&pool, &engine)
        .await
        .expect("minute handoff");
    assert!(
        entry_for(&pool, "card-2").await.is_some(),
        "b is queued although no transition hook saw a finish"
    );

    pool.close().await;
    fixture.drop().await;
}

/// Generate commits an unstarted queue while the handoff waits to create a run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_campaign_handoff_waits_behind_a_queue_generated_meanwhile_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    let engine = engine(&pool);
    let campaign = seed(&pool).await;
    let mut holder = pool.begin().await.expect("begin run-creation holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run_create'))")
        .execute(&mut *holder)
        .await
        .expect("hold run creation");

    let task = spawn_handoff(&pool, &engine, &campaign);
    wait_for_lock_waiter(&pool, "%aq_run_create%").await;
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('preview', $1, 'agent-x', 'generated')",
    )
    .bind(REPO)
    .execute(&mut *holder)
    .await
    .expect("generate a queue");
    holder.commit().await.expect("commit generated queue");
    let report = task.await.expect("handoff task");

    assert!(
        report.queued.is_empty(),
        "no run starts beside an unstarted queue"
    );
    let a = report
        .waiting
        .iter()
        .find(|w| w.node_id == "a")
        .expect("a waits");
    assert_eq!(
        (a.reason, a.detail.as_deref()),
        ("queue_not_started", Some("preview"))
    );
    let runs: Vec<String> = sqlx::query_scalar("SELECT id FROM auto_queue_runs")
        .fetch_all(&pool)
        .await
        .expect("list runs");
    assert_eq!(runs, ["preview"]);

    pool.close().await;
    fixture.drop().await;
}
