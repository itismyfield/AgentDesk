use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::super::{GhIssue, GhPullRequestReference, sync_github_issues_for_repo_pg_with_adapter};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::campaigns::{
    self, Campaign, CampaignError, CampaignInput, DriftReason, Node, NodeStatus,
    issue_sync::AUTO_COMPLETED_STAGE,
};

const REPO: &str = "Owner/Repo";

fn issue_url(number: i64) -> String {
    format!("https://github.com/owner/repo/issues/{number}")
}

fn pr_url(number: i64) -> String {
    format!("https://github.com/owner/repo/pull/{number}")
}

fn at(minutes: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-25T13:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + ChronoDuration::minutes(minutes)
}

fn pr_closer(number: i64) -> Value {
    json!({"__typename": "PullRequest", "number": number, "url": pr_url(number), "merged": true})
}

/// A closed issue whose latest ClosedEvent happened `event_offset_secs` after `closedAt`.
fn closed(
    number: i64,
    reason: &str,
    closed_at: DateTime<Utc>,
    closer: Value,
    event_offset_secs: i64,
) -> Value {
    json!({"number": number, "state": "CLOSED", "stateReason": reason,
           "closedAt": closed_at.to_rfc3339(),
           "timelineItems": {"nodes": [{
               "createdAt": (closed_at + ChronoDuration::seconds(event_offset_secs)).to_rfc3339(),
               "actor": {"login": "mistaken-agent"}, "closer": closer}]}})
}

fn reopened(number: i64) -> Value {
    json!({"number": number, "state": "OPEN", "stateReason": "REOPENED", "closedAt": null,
           "timelineItems": {"nodes": []}})
}

/// A row of the `gh issue list` window, with the linked pull requests GitHub reports.
fn listed(
    number: i64,
    state: &str,
    closed_at: Option<DateTime<Utc>>,
    linked_prs: &[i64],
) -> GhIssue {
    GhIssue {
        number,
        state: state.to_owned(),
        title: format!("issue {number}"),
        labels: Vec::new(),
        body: None,
        url: Some(issue_url(number)),
        closed_at: closed_at.map(|t| t.to_rfc3339()),
        closed_by_pull_requests_references: linked_prs
            .iter()
            .map(|pr| GhPullRequestReference {
                number: Some(*pr),
                url: Some(pr_url(*pr)),
            })
            .collect(),
    }
}

/// Stands in for GitHub's GraphQL endpoint: answers the issues a query names from a script.
/// A query that does not ask for the fields the closer evidence needs is refused.
#[derive(Default)]
struct ScriptedGitHub {
    issues: std::sync::Mutex<HashMap<i64, Value>>,
    fail: AtomicBool,
    gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

const REQUIRED_SELECTIONS: &[&str] = &[
    "state",
    "stateReason",
    "closedAt",
    "timelineItems(itemTypes: [CLOSED_EVENT], last: 1)",
    "createdAt",
    "actor { login }",
    "closer {",
    "__typename",
    "... on PullRequest { number url merged }",
    "... on Commit { oid url }",
];

impl ScriptedGitHub {
    fn new(issues: impl IntoIterator<Item = Value>) -> Self {
        let github = Self::default();
        github.script(issues);
        github
    }

    /// Answers stop before returning until `release` is notified; `entered` fires first.
    fn gated(issues: impl IntoIterator<Item = Value>) -> (Arc<Self>, Arc<Notify>, Arc<Notify>) {
        let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let github = Self {
            gate: Some((entered.clone(), release.clone())),
            ..Self::default()
        };
        github.script(issues);
        (Arc::new(github), entered, release)
    }

    fn script(&self, issues: impl IntoIterator<Item = Value>) {
        let mut scripted = self.issues.lock().unwrap();
        for issue in issues {
            scripted.insert(issue["number"].as_i64().unwrap(), issue);
        }
    }

    fn answer(&self, args: &[String]) -> Result<String, String> {
        let query = args
            .iter()
            .find_map(|arg| arg.strip_prefix("query="))
            .ok_or("no query")?;
        if let Some(missing) = REQUIRED_SELECTIONS
            .iter()
            .find(|field| !query.contains(**field))
        {
            return Err(format!("unscripted query without {missing}"));
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err("scripted GitHub outage".into());
        }
        let pattern = regex::Regex::new(r"(i\d+): issue\(number: (\d+)\)").unwrap();
        let scripted = self.issues.lock().unwrap();
        let repository: serde_json::Map<String, Value> = pattern
            .captures_iter(query)
            .map(|c| {
                let number: i64 = c[2].parse().unwrap();
                (
                    c[1].to_owned(),
                    scripted.get(&number).cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        Ok(json!({"data": {"repository": repository}}).to_string())
    }
}

impl crate::github::GitHubAdapter for ScriptedGitHub {
    fn is_available(&self) -> bool {
        true
    }

    fn run(&self, _args: &[&str]) -> Result<String, String> {
        Err("campaign node sync reads issues through GraphQL only".into())
    }

    fn run_async<'a>(
        &'a self,
        args: Vec<String>,
        _timeout: std::time::Duration,
        _timeout_context: String,
    ) -> crate::github::GitHubFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if let Some((entered, release)) = &self.gate {
                entered.notify_one();
                release.notified().await;
            }
            self.answer(&args)
        })
    }
}

fn node(id: &str, status: &str, stage: &str, issue: Option<i64>) -> Value {
    json!({"id": id, "title": id, "status": status, "stage": stage, "round": 1,
           "issue_url": issue.map(issue_url)})
}

async fn seed(pool: &sqlx::PgPool, id: &str, status: &str, nodes: Vec<Value>) -> Campaign {
    let input: CampaignInput = serde_json::from_value(json!({
        "title": id, "status": status, "round": 1, "nodes": nodes
    }))
    .expect("campaign fixture");
    campaigns::create(pool, id.into(), input)
        .await
        .expect("create campaign")
}

async fn sync(pool: &sqlx::PgPool, issues: &[GhIssue], github: &ScriptedGitHub) {
    sync_github_issues_for_repo_pg_with_adapter(pool, REPO, issues, github)
        .await
        .expect("github sync");
}

/// Reads the campaign and edits one node the way the coordinator helper does: the whole
/// document read back, one node changed, saved against the revision just read.
async fn put_node(
    pool: &sqlx::PgPool,
    campaign: &str,
    node_id: &str,
    edit: impl FnOnce(&mut Value),
) -> Result<Campaign, CampaignError> {
    let current = campaigns::get(pool, campaign).await?;
    let mut document = serde_json::to_value(&current).unwrap();
    let node = document["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == node_id)
        .expect("node to edit");
    edit(node);
    let input: CampaignInput = serde_json::from_value(document).unwrap();
    campaigns::replace(pool, campaign, current.revision, input).await
}

async fn stored(pool: &sqlx::PgPool, campaign: &str, node_id: &str) -> Node {
    campaigns::get(pool, campaign)
        .await
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.input.id == node_id)
        .unwrap()
}

fn last_evidence(node: &Node) -> &campaigns::EvidenceRecord {
    node.input.evidence_records.last().expect("evidence record")
}

#[tokio::test]
async fn postgres_closed_issue_completes_or_skips_linked_nodes_with_the_actual_closer_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    // #1's card already finished before the issue closed; the other issues have no card.
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, repo_id, github_issue_number)
         VALUES ('card-1', 'card-1', 'done', $1, 1)",
    )
    .bind(REPO)
    .execute(&pool)
    .await
    .unwrap();
    let campaign = seed(
        &pool,
        "close",
        "active",
        vec![
            node("listed-pr", "pending", "implement", Some(1)),
            node("listed-manual", "running", "review", Some(2)),
            node("window-pr", "blocked", "merge", Some(3)),
            node("window-manual", "failed", "fix", Some(4)),
            node("unconfirmed", "pending", "implement", Some(5)),
            node("unlinked", "pending", "implement", None),
        ],
    )
    .await;
    // The first linked PR of #1 and #2 is an open one that did not close anything.
    let window = [
        listed(1, "CLOSED", Some(at(0)), &[901]),
        listed(2, "CLOSED", Some(at(1)), &[901]),
        listed(5, "CLOSED", Some(at(5)), &[]),
    ];
    let github = ScriptedGitHub::new([
        closed(1, "COMPLETED", at(0), pr_closer(902), 0),
        closed(2, "NOT_PLANNED", at(1), Value::Null, 0),
        closed(3, "COMPLETED", at(3), pr_closer(903), 30),
        closed(4, "DUPLICATE", at(4), Value::Null, -60),
        // The latest closing event is two hours older than closedAt: not this closing.
        closed(5, "COMPLETED", at(5), pr_closer(905), -7200),
    ]);
    sync(&pool, &window, &github).await;

    let after = campaigns::get(&pool, "close").await.unwrap();
    assert_eq!(
        after.revision,
        campaign.revision + 1,
        "one write per campaign per sync"
    );
    let by_id: HashMap<&str, &Node> = after
        .nodes
        .iter()
        .map(|n| (n.input.id.as_str(), n))
        .collect();

    let pr = by_id["listed-pr"];
    assert_eq!(pr.input.status, NodeStatus::Completed);
    assert_eq!(pr.input.stage, AUTO_COMPLETED_STAGE);
    let record = last_evidence(pr);
    assert_eq!(record.references, vec![issue_url(1), pr_url(902)]);
    assert_eq!(record.recorded_at, Some(at(0)));
    assert!(record.summary.contains("by PR #902"), "{}", record.summary);
    let auto = pr.auto_closed_from.as_ref().expect("previous state kept");
    assert_eq!(
        (auto.status, auto.stage.as_str()),
        (NodeStatus::Pending, "implement")
    );
    assert_eq!(auto.updated_at, campaign.nodes[0].updated_at);
    assert_eq!(auto.issue_closed_at, Some(at(0)));

    let manual = by_id["listed-manual"];
    assert_eq!(manual.input.status, NodeStatus::Skipped);
    let record = last_evidence(manual);
    assert_eq!(
        record.references,
        vec![issue_url(2)],
        "a linked PR is not the closer"
    );
    assert!(!record.summary.contains("901"), "{}", record.summary);
    assert!(
        record.summary.contains("@mistaken-agent"),
        "{}",
        record.summary
    );

    let window_pr = by_id["window-pr"];
    assert_eq!(window_pr.input.status, NodeStatus::Completed);
    assert_eq!(
        last_evidence(window_pr).references,
        vec![issue_url(3), pr_url(903)]
    );
    assert_eq!(
        last_evidence(window_pr).recorded_at,
        Some(at(3) + ChronoDuration::seconds(30))
    );
    let window_manual = by_id["window-manual"];
    assert_eq!(window_manual.input.status, NodeStatus::Skipped);
    assert_eq!(last_evidence(window_manual).references, vec![issue_url(4)]);

    let unconfirmed = by_id["unconfirmed"];
    assert_eq!(
        unconfirmed.input.status,
        NodeStatus::Pending,
        "an unmatched closing event waits"
    );
    assert!(unconfirmed.auto_closed_from.is_none());
    assert_eq!(by_id["unlinked"].input.status, NodeStatus::Pending);

    // Next cycle GitHub reports the event of this closing, so the node completes with it.
    github.script([closed(5, "COMPLETED", at(5), pr_closer(905), 0)]);
    sync(&pool, &window, &github).await;
    let unconfirmed = stored(&pool, "close", "unconfirmed").await;
    assert_eq!(unconfirmed.input.status, NodeStatus::Completed);
    assert_eq!(
        last_evidence(&unconfirmed).references,
        vec![issue_url(5), pr_url(905)]
    );

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_reopened_issue_restores_the_auto_closed_node_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed(
        &pool,
        "reopen",
        "active",
        vec![node("a", "running", "review r2", Some(7))],
    )
    .await;
    let github = ScriptedGitHub::new([closed(7, "COMPLETED", at(0), pr_closer(907), 0)]);
    sync(&pool, &[], &github).await;
    assert_eq!(
        stored(&pool, "reopen", "a").await.input.status,
        NodeStatus::Completed
    );

    // A writer that does not know the server fields sends the node back without them.
    put_node(&pool, "reopen", "a", |node| {
        node.as_object_mut().unwrap().remove("auto_closed_from");
    })
    .await
    .expect("old writer save");
    assert!(
        stored(&pool, "reopen", "a")
            .await
            .auto_closed_from
            .is_some()
    );

    // While GitHub cannot be read the node keeps its state.
    github.script([reopened(7)]);
    github.fail.store(true, Ordering::SeqCst);
    sync(&pool, &[], &github).await;
    let kept = stored(&pool, "reopen", "a").await;
    assert_eq!(kept.input.status, NodeStatus::Completed);
    assert!(kept.auto_closed_from.is_some());

    github.fail.store(false, Ordering::SeqCst);
    sync(&pool, &[], &github).await;
    let restored = stored(&pool, "reopen", "a").await;
    assert_eq!(
        (restored.input.status, restored.input.stage.as_str()),
        (NodeStatus::Running, "review r2")
    );
    assert!(restored.auto_closed_from.is_none());
    assert_eq!(last_evidence(&restored).result.as_deref(), Some("restored"));

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_reopen_keeps_a_hand_changed_node_and_reports_drift_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed(
        &pool,
        "hand",
        "active",
        vec![
            node("stage-changed", "running", "review", Some(11)),
            node("status-round-trip", "running", "review", Some(12)),
            node("evidence-only", "running", "review", Some(13)),
        ],
    )
    .await;
    let github = ScriptedGitHub::new([
        closed(11, "COMPLETED", at(0), pr_closer(911), 0),
        closed(12, "COMPLETED", at(0), pr_closer(912), 0),
        closed(13, "COMPLETED", at(0), pr_closer(913), 0),
    ]);
    sync(&pool, &[], &github).await;

    put_node(&pool, "hand", "stage-changed", |node| {
        node["stage"] = json!("완료: 배포 확인")
    })
    .await
    .unwrap();
    // Status away and back to the value the sync wrote; stage untouched throughout.
    put_node(&pool, "hand", "status-round-trip", |node| {
        node["status"] = json!("running")
    })
    .await
    .unwrap();
    put_node(&pool, "hand", "status-round-trip", |node| {
        node["status"] = json!("completed")
    })
    .await
    .unwrap();
    put_node(&pool, "hand", "evidence-only", |node| {
        node["evidence"] = json!(["checked after merge"])
    })
    .await
    .unwrap();

    github.script([reopened(11), reopened(12), reopened(13)]);
    sync(&pool, &[], &github).await;

    for (id, stage) in [
        ("stage-changed", "완료: 배포 확인"),
        ("status-round-trip", AUTO_COMPLETED_STAGE),
    ] {
        let kept = stored(&pool, "hand", id).await;
        assert_eq!(
            (kept.input.status, kept.input.stage.as_str()),
            (NodeStatus::Completed, stage),
            "{id}"
        );
        assert_eq!(
            kept.issue_drift.as_ref().map(|drift| drift.reason),
            Some(DriftReason::ChangedAfterAutoClose),
            "{id}"
        );
    }
    let restored = stored(&pool, "hand", "evidence-only").await;
    assert_eq!(
        (restored.input.status, restored.input.stage.as_str()),
        (NodeStatus::Running, "review")
    );
    assert_eq!(
        restored.input.evidence,
        vec!["checked after merge".to_owned()]
    );

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_post_merge_hold_keeps_a_node_open_when_its_issue_closes_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let mut held = node("held", "running", "merged", Some(21));
    held["post_merge_hold"] = json!("release deploy, then 24h relay observation");
    let held_campaign = seed(&pool, "held", "active", vec![held]).await;
    seed(
        &pool,
        "stage-text",
        "active",
        vec![node("s", "running", "merged_awaiting_deploy", Some(22))],
    )
    .await;
    let paused = seed(
        &pool,
        "paused",
        "paused",
        vec![node("p", "pending", "implement", Some(23))],
    )
    .await;
    let github = ScriptedGitHub::new([
        closed(21, "COMPLETED", at(0), pr_closer(921), 0),
        closed(22, "COMPLETED", at(0), pr_closer(922), 0),
        closed(23, "COMPLETED", at(0), pr_closer(923), 0),
    ]);
    sync(&pool, &[], &github).await;

    let after = campaigns::get(&pool, "held").await.unwrap();
    assert_eq!(
        after.revision, held_campaign.revision,
        "a held node is not written"
    );
    assert_eq!(after.nodes[0].input.status, NodeStatus::Running);
    assert!(after.nodes[0].auto_closed_from.is_none());
    assert_eq!(
        stored(&pool, "stage-text", "s").await.input.status,
        NodeStatus::Completed,
        "stage text alone does not hold a node"
    );
    let paused_after = campaigns::get(&pool, "paused").await.unwrap();
    assert_eq!(
        paused_after.revision, paused.revision,
        "inactive campaigns are not touched"
    );
    assert_eq!(paused_after.nodes[0].input.status, NodeStatus::Pending);

    pool.close().await;
    fixture.drop().await;
}

async fn wait_for_row_lock_waiter(pool: &sqlx::PgPool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
             WHERE wait_event_type = 'Lock' AND query LIKE '%FOR UPDATE%')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sync never waited on the row lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_issue_sync_and_a_concurrent_put_keep_every_node_change_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    let github = Arc::new(ScriptedGitHub::new([
        closed(31, "COMPLETED", at(0), pr_closer(931), 0),
        closed(32, "COMPLETED", at(0), pr_closer(932), 0),
    ]));

    // A coordinator read the document, then the sync wrote: the stale save is refused,
    // and the reload-and-reapply keeps both changes.
    seed(
        &pool,
        "stale-put",
        "active",
        vec![
            node("a", "running", "review", Some(31)),
            node("b", "pending", "implement", None),
        ],
    )
    .await;
    let read = campaigns::get(&pool, "stale-put").await.unwrap();
    sync(&pool, &[], &github).await;
    let mut stale: CampaignInput =
        serde_json::from_value(serde_json::to_value(&read).unwrap()).unwrap();
    stale.nodes[1].next_action = Some("write the handoff note".into());
    assert!(matches!(
        campaigns::replace(&pool, "stale-put", read.revision, stale).await,
        Err(CampaignError::Conflict)
    ));
    put_node(&pool, "stale-put", "b", |node| {
        node["next_action"] = json!("write the handoff note")
    })
    .await
    .expect("reapplied save");
    let both = campaigns::get(&pool, "stale-put").await.unwrap();
    assert_eq!(both.nodes[0].input.status, NodeStatus::Completed);
    assert!(both.nodes[0].auto_closed_from.is_some());
    assert_eq!(
        both.nodes[1].input.next_action.as_deref(),
        Some("write the handoff note")
    );

    // A save holding the row when the sync arrives: the sync plans on what that save wrote.
    let held = seed(
        &pool,
        "locked-put",
        "active",
        vec![
            node("a", "running", "review", Some(32)),
            node("b", "pending", "implement", None),
        ],
    )
    .await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT document FROM campaigns WHERE id = 'locked-put' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let mut saved = held.clone();
    saved.revision += 1;
    saved.nodes[1].input.next_action = Some("saved while the sync waited".into());
    sqlx::query("UPDATE campaigns SET revision = $2, document = $3 WHERE id = $1")
        .bind("locked-put")
        .bind(saved.revision)
        .bind(sqlx::types::Json(&saved))
        .execute(&mut *holder)
        .await
        .unwrap();
    let task = {
        let (pool, github) = (pool.clone(), github.clone());
        tokio::spawn(async move { sync(&pool, &[], &github).await })
    };
    wait_for_row_lock_waiter(&pool).await;
    holder.commit().await.unwrap();
    task.await.unwrap();
    let both = campaigns::get(&pool, "locked-put").await.unwrap();
    assert_eq!(both.revision, saved.revision + 1);
    assert_eq!(both.nodes[0].input.status, NodeStatus::Completed);
    assert_eq!(
        both.nodes[1].input.next_action.as_deref(),
        Some("saved while the sync waited")
    );

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_a_node_reopened_by_hand_is_not_closed_again_for_the_same_closing_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed(
        &pool,
        "again",
        "active",
        vec![node("a", "pending", "implement", Some(41))],
    )
    .await;
    let github = ScriptedGitHub::new([closed(41, "COMPLETED", at(0), pr_closer(941), 0)]);
    sync(&pool, &[], &github).await;
    put_node(&pool, "again", "a", |node| {
        node["status"] = json!("running")
    })
    .await
    .unwrap();

    sync(&pool, &[], &github).await;
    assert_eq!(
        stored(&pool, "again", "a").await.input.status,
        NodeStatus::Running
    );

    // Reopened and closed again between two syncs: a new closing completes it again.
    github.script([closed(41, "COMPLETED", at(90), pr_closer(942), 0)]);
    sync(&pool, &[], &github).await;
    let closed_again = stored(&pool, "again", "a").await;
    assert_eq!(closed_again.input.status, NodeStatus::Completed);
    let auto = closed_again.auto_closed_from.unwrap();
    assert_eq!(
        (auto.status, auto.issue_closed_at),
        (NodeStatus::Running, Some(at(90)))
    );
    assert!(!auto.manually_changed);

    pool.close().await;
    fixture.drop().await;
}

/// Runs a sync whose GitHub answer is held back, returning once it has asked.
async fn start_gated_sync(
    pool: &sqlx::PgPool,
    answer: Value,
) -> (tokio::task::JoinHandle<()>, Arc<Notify>) {
    let (github, entered, release) = ScriptedGitHub::gated([answer]);
    let pool = pool.clone();
    let task = tokio::spawn(async move { sync(&pool, &[], &github).await });
    // Without a GitHub read there is nothing to hold back; fail rather than wait forever.
    tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
        .await
        .expect("the sync never read the linked issue from GitHub");
    (task, release)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_a_stale_github_observation_does_not_undo_a_newer_write_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(8).await;
    seed(
        &pool,
        "stale",
        "active",
        vec![
            node("restored", "running", "review", Some(51)),
            node("reopened-by-hand", "pending", "implement", Some(52)),
        ],
    )
    .await;
    let closing_51 = closed(51, "COMPLETED", at(0), pr_closer(951), 0);
    sync(&pool, &[], &ScriptedGitHub::new([closing_51.clone()])).await;

    // A read CLOSED, then the issue reopened and B restored the node before A applied.
    let (a, release) = start_gated_sync(&pool, closing_51).await;
    sync(&pool, &[], &ScriptedGitHub::new([reopened(51)])).await;
    release.notify_one();
    a.await.unwrap();
    let restored = stored(&pool, "stale", "restored").await;
    assert_eq!(restored.input.status, NodeStatus::Running);
    assert!(restored.auto_closed_from.is_none());

    // A read OPEN, then the issue closed, B completed the node and a person reopened it.
    // A's old OPEN must not consume the record that keeps the node open for this closing.
    let closing_52 = closed(52, "COMPLETED", at(0), pr_closer(952), 0);
    let (a, release) = start_gated_sync(&pool, reopened(52)).await;
    sync(&pool, &[], &ScriptedGitHub::new([closing_52.clone()])).await;
    put_node(&pool, "stale", "reopened-by-hand", |node| {
        node["status"] = json!("running")
    })
    .await
    .unwrap();
    release.notify_one();
    a.await.unwrap();
    let kept = stored(&pool, "stale", "reopened-by-hand").await;
    assert!(
        kept.auto_closed_from
            .as_ref()
            .is_some_and(|auto| auto.manually_changed)
    );
    sync(&pool, &[], &ScriptedGitHub::new([closing_52])).await;
    assert_eq!(
        stored(&pool, "stale", "reopened-by-hand")
            .await
            .input
            .status,
        NodeStatus::Running
    );

    pool.close().await;
    fixture.drop().await;
}

#[tokio::test]
async fn postgres_every_out_of_window_linked_issue_is_fetched_in_one_sync_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let numbers: Vec<i64> = (1001..=1201).collect();
    seed(
        &pool,
        "wide",
        "active",
        numbers
            .iter()
            .map(|n| node(&format!("n{n}"), "pending", "implement", Some(*n)))
            .collect(),
    )
    .await;
    let github = ScriptedGitHub::new(
        numbers
            .iter()
            .map(|n| closed(*n, "COMPLETED", at(0), pr_closer(n + 5000), 0)),
    );
    sync(&pool, &[], &github).await;
    let after = campaigns::get(&pool, "wide").await.unwrap();
    assert!(
        after
            .nodes
            .iter()
            .all(|node| node.input.status == NodeStatus::Completed),
        "{} of 201 completed",
        after
            .nodes
            .iter()
            .filter(|n| n.input.status == NodeStatus::Completed)
            .count()
    );

    pool.close().await;
    fixture.drop().await;
}
