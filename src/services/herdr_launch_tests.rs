use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::{expected, owner, pending, record};
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

const CHANNEL: &str = "1479671301387059300";
const SESSION_KEY: &str = "claude/discord_0123456789abcdef/test-node:AgentDesk-claude-hosted";

fn endpoint() -> HerdrLaunchEndpoint {
    HerdrLaunchEndpoint {
        execution_node: "test-node".into(),
        config_key: "herdr.default".into(),
        socket_addr: "/adk/herdr.sock".into(),
        herdr_session: "agentdesk".into(),
        restore_resume: Some(false),
    }
}

fn launch(endpoint: Option<HerdrLaunchEndpoint>) -> HerdrLaunch {
    HerdrLaunch {
        endpoint,
        owner: owner(CHANNEL),
        channel_id: Some(1),
        expected_native_session_id: None,
        resume: false,
    }
}

fn command(_: &PreparedIncarnation) -> Result<HerdrLaunchCommand, String> {
    Ok(HerdrLaunchCommand {
        cwd: "/tmp".into(),
        command: "bash launch.sh".into(),
    })
}

fn stamps(root_pid: u32) -> (ProcessStamp, ProcessStamp) {
    let evidence = expected("unused", root_pid);
    (evidence.root, evidence.provider_process)
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// Records every Herdr call; `on_create`/`on_evidence` run inside the call, on its thread.
struct FakeHost {
    reply: HerdrCreateOutcome,
    evidence: Option<(ProcessStamp, ProcessStamp)>,
    creates: AtomicUsize,
    probes: AtomicUsize,
    on_create: Hook,
    on_evidence: Hook,
}

impl FakeHost {
    fn new(reply: HerdrCreateOutcome) -> Self {
        Self {
            reply,
            evidence: Some(stamps(100)),
            creates: AtomicUsize::new(0),
            probes: AtomicUsize::new(0),
            on_create: Box::new(|| {}),
            on_evidence: Box::new(|| {}),
        }
    }

    fn created(pane: &str) -> Self {
        Self::new(HerdrCreateOutcome::Created {
            pane_id: pane.into(),
        })
    }
}

impl HerdrLaunchHost for FakeHost {
    fn create(&self, _request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        self.creates.fetch_add(1, Ordering::SeqCst);
        (self.on_create)();
        self.reply.clone()
    }

    fn launch_evidence(&self, _location: &HostedLocation) -> Option<(ProcessStamp, ProcessStamp)> {
        self.probes.fetch_add(1, Ordering::SeqCst);
        (self.on_evidence)();
        self.evidence.clone()
    }
}

fn marker() -> HostKindMarker {
    read_host_kind_marker(&owner(CHANNEL).logical_key)
}

#[tokio::test]
async fn herdr_launch_refuses_an_incomplete_endpoint_or_restore_resume_before_any_io() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    // A lazy pool connects on first use; an admitted launch would fail on the row instead.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1:1/unused")
        .unwrap();
    let with = |change: fn(&mut HerdrLaunchEndpoint)| {
        let mut endpoint = endpoint();
        change(&mut endpoint);
        Some(endpoint)
    };
    let cases = [
        (None, ENDPOINT_MISSING),
        (with(|e| e.config_key = " ".into()), ENDPOINT_MISSING),
        (with(|e| e.execution_node.clear()), ENDPOINT_MISSING),
        (with(|e| e.herdr_session.clear()), ENDPOINT_MISSING),
        (
            with(|e| e.socket_addr = "herdr.sock".into()),
            ENDPOINT_MISSING,
        ),
        (with(|e| e.restore_resume = None), RESTORE_RESUME_NOT_OFF),
        (
            with(|e| e.restore_resume = Some(true)),
            RESTORE_RESUME_NOT_OFF,
        ),
    ];
    for (endpoint, reason) in cases {
        let host = Arc::new(FakeHost::created("pane-1"));
        let prepared = AtomicUsize::new(0);
        let result = launch_herdr_session(
            &pool,
            launch(endpoint.clone()),
            |incarnation| {
                prepared.fetch_add(1, Ordering::SeqCst);
                command(incarnation)
            },
            host.clone(),
        )
        .await;
        assert_eq!(
            result,
            Err(HerdrLaunchError::Unsupported(reason)),
            "{endpoint:?}"
        );
        assert_eq!(host.creates.load(Ordering::SeqCst), 0);
        assert_eq!(prepared.load(Ordering::SeqCst), 0);
        assert_eq!(marker(), HostKindMarker::Absent);
    }
}

async fn seed_row(pool: &PgPool, raw: Option<Value>) {
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
    )
    .bind(SESSION_KEY)
    .bind(&owner(CHANNEL).discord_token_hash)
    .bind(CHANNEL)
    .bind(raw)
    .execute(pool)
    .await
    .unwrap();
}

async fn stored(pool: &PgPool) -> Option<Value> {
    sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key = $1")
        .bind(SESSION_KEY)
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
}

fn wire(record: &HostedExecution) -> Value {
    serde_json::to_value(record).unwrap()
}

fn decoded(raw: Option<Value>) -> HostedRecord {
    HostedRecord::decode(raw.as_ref())
}

/// Blocks a Herdr call's thread on the database, as a concurrent actor would act.
fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(future)
}

#[tokio::test(flavor = "multi_thread")]
async fn herdr_launch_commits_pending_then_marker_then_one_create_and_fills_that_nonce_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    seed_row(&pool, None).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut host = FakeHost::created("pane-7");
    let (task_pool, task_seen) = (pool.clone(), seen.clone());
    host.on_create = Box::new(move || {
        let record = decoded(run(stored(&task_pool)));
        task_seen.lock().unwrap().push((record, marker()));
    });
    let host = Arc::new(host);
    let outcome = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone())
        .await
        .unwrap();

    let HerdrLaunchOutcome::Launched {
        execution_nonce,
        location,
        evidence: true,
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    let at_create = seen.lock().unwrap().clone();
    let pending = pending(&owner(CHANNEL), &execution_nonce);
    assert_eq!(
        at_create,
        [(
            HostedRecord::Known(pending.clone()),
            HostKindMarker::Known(HostKind::Herdr)
        )],
        "create runs only after the Pending commit and the marker"
    );
    assert_eq!(location.pane_id, "pane-7");
    assert_eq!(
        (
            location.endpoint_config_key.as_str(),
            location.socket_addr.as_str()
        ),
        ("herdr.default", "/adk/herdr.sock")
    );
    let (root, provider_process) = stamps(100);
    let mut filled = pending;
    filled.location = Some(location);
    filled.expected = Some(ExpectedExecution {
        binding_provider: "claude".into(),
        binding_nonce: execution_nonce.clone(),
        root,
        provider_process,
        provenance: EVIDENCE_PROVENANCE.into(),
    });
    assert_eq!(decoded(stored(&pool).await), HostedRecord::Known(filled));
    assert_eq!(host.creates.load(Ordering::SeqCst), 1);
    let presence = crate::services::tui_prompt_dedupe::binding_context::context_presence(
        "claude",
        &execution_nonce,
    );
    assert_eq!(
        presence,
        crate::services::tui_prompt_dedupe::binding_context::ContextPresence::Present
    );
    pool.close().await;
    db.drop().await;
}

#[derive(Debug, Clone, Copy)]
enum Rival {
    SecondLaunch,
    ExplicitDelete,
}

impl Rival {
    async fn act(self, pool: &PgPool) {
        match self {
            Self::SecondLaunch => {
                let key = HostedLookupKey::SessionKey(SESSION_KEY);
                let HostedLookup::Found(seen) = load_hosted_execution_pg(pool, key).await else {
                    panic!("rival read");
                };
                let rival = install_pending_pg(pool, &seen, pending(&owner(CHANNEL), "rival"));
                assert_eq!(rival.await, Ok(HostedCasOutcome::Written));
            }
            Self::ExplicitDelete => {
                let deleted =
                    crate::db::dispatched_sessions::delete_session_by_key_pg(pool, SESSION_KEY);
                assert_eq!(deleted.await.map(|result| result.deleted), Ok(1));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn herdr_launch_creates_nothing_unless_its_own_pending_commits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = owner(CHANNEL);
    let occupied = [
        (
            Some(wire(&pending(&owner, "n0"))),
            Some(HostedState::Pending),
        ),
        (
            Some(wire(&record(&owner, "n0", HostedState::Bound))),
            Some(HostedState::Bound),
        ),
        (Some(serde_json::json!({"schema": 2})), None),
    ];
    for (raw, state) in occupied {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, raw.clone()).await;
        let host = Arc::new(FakeHost::created("pane-1"));
        let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone());
        assert_eq!(result.await, Err(HerdrLaunchError::Occupied(state)));
        assert_eq!(host.creates.load(Ordering::SeqCst), 0);
        assert_eq!(stored(&pool).await, raw);
    }

    sqlx::query("DELETE FROM sessions")
        .execute(&pool)
        .await
        .unwrap();
    let host = Arc::new(FakeHost::created("pane-1"));
    let missing = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone());
    assert!(matches!(missing.await, Err(HerdrLaunchError::Row(_))));
    assert_eq!(host.creates.load(Ordering::SeqCst), 0);

    // Another launch or an explicit delete lands between this launch's read and its CAS.
    for rival in [Rival::SecondLaunch, Rival::ExplicitDelete] {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, None).await;
        let host = Arc::new(FakeHost::created("pane-1"));
        let rival_pool = pool.clone();
        let result = launch_herdr_session(
            &pool,
            launch(Some(endpoint())),
            |incarnation| {
                tokio::task::block_in_place(|| run(rival.act(&rival_pool)));
                command(incarnation)
            },
            host.clone(),
        )
        .await;
        assert!(
            matches!(result, Err(HerdrLaunchError::Pending(_))),
            "{rival:?}: {result:?}"
        );
        assert_eq!(host.creates.load(Ordering::SeqCst), 0, "{rival:?}");
        assert_eq!(
            marker(),
            HostKindMarker::Absent,
            "{rival:?}: marker before Pending"
        );
    }
    assert_eq!(stored(&pool).await, None);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn herdr_launch_keeps_pending_and_never_recreates_after_a_lost_create_reply_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let replies = [
        HerdrCreateOutcome::Indeterminate("reply EOF after the request was written".into()),
        HerdrCreateOutcome::Created {
            pane_id: " ".into(),
        },
        HerdrCreateOutcome::NotSent("connect refused".into()),
    ];
    for reply in replies {
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
        seed_row(&pool, None).await;
        let host = Arc::new(FakeHost::new(reply.clone()));
        let result =
            launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone()).await;
        let HostedRecord::Known(left) = decoded(stored(&pool).await) else {
            panic!("{reply:?}: Pending must stay");
        };
        match (&reply, result) {
            (HerdrCreateOutcome::NotSent(_), Err(HerdrLaunchError::NotSent(_))) => {}
            (
                _,
                Ok(HerdrLaunchOutcome::Indeterminate {
                    execution_nonce, ..
                }),
            ) => {
                assert_eq!(execution_nonce, left.execution_nonce);
            }
            (_, other) => panic!("{reply:?}: {other:?}"),
        }
        assert_eq!(
            left,
            pending(&owner(CHANNEL), &left.execution_nonce),
            "{reply:?}"
        );
        assert_eq!(host.creates.load(Ordering::SeqCst), 1, "{reply:?}: resent");
        assert_eq!(host.probes.load(Ordering::SeqCst), 0, "{reply:?}: adopted");
        assert_eq!(marker(), HostKindMarker::Known(HostKind::Herdr));
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn herdr_launch_writes_the_pane_only_to_its_pending_and_keeps_stored_evidence_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;

    // The record moves to another nonce while the create call is out.
    seed_row(&pool, None).await;
    let other = wire(&pending(&owner(CHANNEL), "other"));
    let mut host = FakeHost::created("pane-1");
    let (task_pool, replaced) = (pool.clone(), other.clone());
    host.on_create = Box::new(move || {
        run(sqlx::query("UPDATE sessions SET hosted_execution = $1")
            .bind(replaced.clone())
            .execute(&task_pool))
        .unwrap();
    });
    let host = Arc::new(host);
    let result = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone());
    assert!(matches!(
        result.await,
        Ok(HerdrLaunchOutcome::Indeterminate { .. })
    ));
    assert_eq!(stored(&pool).await, Some(other));
    assert_eq!(host.probes.load(Ordering::SeqCst), 0);

    // Evidence stored first for this nonce is never replaced by a later reading.
    sqlx::query("DELETE FROM sessions")
        .execute(&pool)
        .await
        .unwrap();
    seed_row(&pool, None).await;
    let mut host = FakeHost::created("pane-1");
    host.evidence = Some(stamps(500));
    let task_pool = pool.clone();
    host.on_evidence = Box::new(move || {
        let HostedRecord::Known(current) = decoded(run(stored(&task_pool))) else {
            panic!("pending before evidence");
        };
        let first = expected(&current.execution_nonce, 100);
        let observed = match run(load_hosted_execution_pg(
            &task_pool,
            HostedLookupKey::SessionKey(SESSION_KEY),
        )) {
            HostedLookup::Found(observed) => observed,
            other => panic!("{other:?}"),
        };
        let owner = owner(CHANNEL);
        let location = current.location.clone().unwrap();
        let nonce = current.execution_nonce.clone();
        let written = run(record_launch_evidence_pg(
            &task_pool, &observed, &owner, &nonce, location, first,
        ));
        assert_eq!(written, Ok(HostedCasOutcome::Written));
    });
    let host = Arc::new(host);
    let outcome = launch_herdr_session(&pool, launch(Some(endpoint())), command, host.clone())
        .await
        .unwrap();
    let HerdrLaunchOutcome::Launched {
        execution_nonce,
        evidence: false,
        ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    let HostedRecord::Known(kept) = decoded(stored(&pool).await) else {
        panic!("record");
    };
    assert_eq!(kept.expected, Some(expected(&execution_nonce, 100)));
    pool.close().await;
    db.drop().await;
}
