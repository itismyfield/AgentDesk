//! The forced cancel and force-kill select the hosted row production writers left, with no SQL on
//! the session: the hook upsert's canonical row, the Herdr launch's record, its bind, the turn start.

use crate::db::dispatched_sessions::hosted_execution::ProcessStamp;

use super::*;

const AGENT: &str = "writer-agent";
const PROVIDER_SESSION: &str = "claude-writer";
const TOKEN_HASH: &str = "discord_6772d0000000a0d3";
const OTHER_HASH: &str = "discord_0000000000000d3a";

/// The live terminate and turn switches on until dropped, the child's config restored then.
struct LiveOn(crate::config::Config);

impl LiveOn {
    fn new() -> Self {
        let current = crate::config_live_reload::current();
        let previous = current.map(|c| (*c).clone()).unwrap_or_default();
        let mut config = previous.clone();
        config.runtime.herdr_terminate_enabled = Some(true);
        config.runtime.herdr_turn_enabled = Some(true);
        crate::config_live_reload::install(config);
        Self(previous)
    }
}

impl Drop for LiveOn {
    fn drop(&mut self) {
        crate::config_live_reload::install(self.0.clone());
    }
}

/// The first launch's Herdr host: resume-on-restore off, the rig server's pane created, and that
/// pane's root shell and provider read back as the launch evidence.
struct FirstLaunch;

impl HerdrLaunchHost for FirstLaunch {
    fn restore_resume(&self, _: &HerdrLaunchEndpoint) -> RestoreResume {
        RestoreResume::Off {
            witness: ServerWitness::for_test(1),
        }
    }

    fn create(&self, _: &HerdrCreateRequest) -> HerdrCreateOutcome {
        HerdrCreateOutcome::Created {
            pane_id: "w1-1".into(),
        }
    }

    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        let stamp = |pid, start: &str| ProcessStamp {
            pid,
            start: start.into(),
        };
        Ok(ExpectedExecution {
            binding_provider: "claude".into(),
            binding_nonce: probe.execution_nonce.clone(),
            root: stamp(10, "darwin:1001.000000"),
            provider_process: stamp(20, "darwin:1002.000000"),
            provenance: "herdr_launch:ppid+env;exec=?".into(),
        })
    }
}

/// A throwaway PG and runtime whose session row only production writers wrote: the hook upsert
/// made it, the launch recorded its execution `nonce`, and the bind made that execution Bound.
struct Launched {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    registry: Arc<HealthRegistry>,
    key: String,
    nonce: String,
    _os: Option<herdr::LiveSwitches>,
    _live: LiveOn,
}

impl Launched {
    async fn new(rig: &herdr::P11TerminationRig) -> Self {
        let _ = std::fs::remove_dir_all(runtime_store::discord_pending_queue_root().unwrap());
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        // The runtime's bot namespace in its production form, which the hook upsert validates.
        let mut shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        Arc::get_mut(&mut shared)
            .expect("a fresh runtime")
            .token_hash = TOKEN_HASH.into();
        shared.settings.write().await.provider = ProviderKind::Claude;
        let registry = Arc::new(HealthRegistry::new());
        registry
            .register("claude".to_string(), shared.clone())
            .await;
        let key = format!(
            "claude/{}/{}:AgentDesk-claude-e07",
            shared.token_hash,
            herdr::NODE
        );
        let mut launched = Self {
            db,
            pool,
            shared,
            registry,
            key,
            nonce: String::new(),
            _os: None,
            _live: LiveOn::new(),
        };
        launched.dispatch_fixtures().await;
        let hash = launched.shared.token_hash.clone();
        launched
            .hook(
                json!({"session_key": launched.key, "status": "idle", "provider": "claude",
                "channel_id": CHANNEL.to_string(), "identity_kind": "discord_channel",
                "discord_token_hash": hash, "agent_id": AGENT,
                "claude_session_id": PROVIDER_SESSION}),
            )
            .await;
        assert_eq!(
            herdr::row(&launched.pool, &launched.key).await,
            None,
            "a legacy row"
        );
        let configured = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
        let endpoint = HerdrLaunchEndpoint {
            execution_node: configured.execution_node,
            config_key: configured.key,
            socket_addr: configured.socket_path.display().to_string(),
            herdr_session: configured.herdr_session,
        };
        let owner = rig.record(&hash, &launched.key).owner;
        let host = Arc::new(FirstLaunch);
        launched.nonce = launch_fresh(&launched.pool, endpoint, owner.clone(), host).await;
        let (row, record) = observed(&launched.pool, &launched.key).await;
        assert_eq!(record.state, herdr::HostedState::Pending, "{record:?}");
        let bound = hosted::bind_pg(&launched.pool, &row, &owner, &launched.nonce).await;
        assert_eq!(bound, Ok(HostedCasOutcome::Written));
        launched._os = Some(launched.provider_names_its_context(rig).await);
        launched
    }

    /// The fake host's pane provider as a launch leaves it, its environment naming this
    /// execution's binding context. The rig sets that up through an idle Codex row of another bot
    /// namespace, which no force path here can select.
    async fn provider_names_its_context(
        &self,
        rig: &herdr::P11TerminationRig,
    ) -> herdr::LiveSwitches {
        let root = crate::config::runtime_root().unwrap();
        let context = root.join(format!(
            "runtime/binding_contexts/claude/{}.json",
            self.nonce
        ));
        assert!(context.is_file(), "the launch wrote {}", context.display());
        let key = format!("codex/{OTHER_HASH}/{}:AgentDesk-codex-scratch", herdr::NODE);
        let retired = herdr::HostedState::Retired;
        let named = (OTHER_HASH, key.as_str());
        rig.bind_as(&self.pool, named, retired, "codex", &context)
            .await
    }

    /// The agent, its cards, both dispatches and an auto-queue run the force-kill's retry needs.
    async fn dispatch_fixtures(&self) {
        for statement in [
            "INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ('writer-agent', 'W', 'claude', '6772101')",
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id) VALUES ('writer-card', 'w', 'in_progress', 'writer-agent'), ('writer-aq-card', 'aq', 'in_progress', 'writer-agent')",
            "INSERT INTO task_dispatches (id, status, kanban_card_id, to_agent_id, dispatch_type, title, context, retry_count) VALUES ('dispatch-e07', 'dispatched', 'writer-card', 'writer-agent', 'implementation', 'dispatch-e07', '{}', 0), ('dispatch-e07-successor', 'dispatched', NULL, NULL, NULL, NULL, NULL, 0)",
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('writer-run', 'repo', 'writer-agent', 'active')",
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status) VALUES ('writer-entry', 'writer-run', 'writer-aq-card', 'writer-agent', 'failed')",
        ] {
            sqlx::query(statement).execute(&self.pool).await.unwrap();
        }
    }

    fn app(&self) -> axum::Router {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let (pool, registry) = (Some(self.pool.clone()), Some(self.registry.clone()));
        crate::server::routes::home_stop_test_app(config, pool, registry, herdr::NODE)
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let bearer = format!("Bearer {AUTH}");
        let headers = [("authorization", bearer.as_str()), JSON];
        let post = crate::server::routes::home_stop_test_post;
        post(&self.app(), uri, &headers, &body.to_string()).await
    }

    /// The production session upsert, as the bridge posts it.
    async fn hook(&self, marker: Value) {
        let (status, reply) = self.post("/dispatched-sessions/webhook", marker).await;
        assert!(status.is_success(), "{status}: {reply}");
    }

    /// The turn start's marker on the launched row: active on the dispatch, heartbeat now.
    async fn turn_started(&self) {
        let marker = json!({"session_key": self.key, "status": "turn_active", "provider": "claude",
            "channel_id": CHANNEL.to_string(), "dispatch_id": DISPATCH, "agent_id": AGENT,
            "turn_start_nonce": "turn-a", "dispatched_origin": true});
        self.hook(marker).await;
        test_turn::start(&self.shared, channel(), 51, "turn-a").await;
        let mut row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        row.dispatch_id = Some(DISPATCH.into());
        inflight::save_inflight_state(&row).unwrap();
    }

    /// Status, dispatch and heartbeat of session `key`, as the writers left them.
    async fn written(&self, key: &str) -> Written {
        let row = "SELECT status, active_dispatch_id, last_heartbeat FROM sessions WHERE session_key = $1";
        sqlx::query_as(row)
            .bind(key)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn dispatch(&self, id: &str) -> String {
        let status = "SELECT status FROM task_dispatches WHERE id = $1";
        sqlx::query_scalar(status)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

type Written = (
    String,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig_writer(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, false);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

fn closes(rig: &herdr::P11TerminationRig) -> usize {
    let writes = rig.writes();
    let close = |(method, pane): &(String, String)| method.ends_with(".close") && pane == "w1-1";
    assert!(writes.iter().all(close), "{writes:?}");
    writes.len()
}

// A forced cancel picks, among the channel's active rows, the launched row by the heartbeat its turn
// start wrote, and retires exactly the execution the launch recorded.
#[test]
fn force_production_launch_heartbeat_lookup_pg() {
    let Some((rig, runtime)) = rig_writer("force_production_launch_heartbeat_lookup_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let launched = Launched::new(&rig).await;
        let hash = launched.shared.token_hash.clone();
        let other = format!("codex/{hash}/{}:AgentDesk-codex-e07", herdr::NODE);
        launched
            .hook(
                json!({"session_key": other, "status": "turn_active", "provider": "codex",
                "channel_id": CHANNEL.to_string(), "identity_kind": "discord_channel",
                "discord_token_hash": hash}),
            )
            .await;
        let earlier = launched.written(&other).await;
        launched.turn_started().await;
        let ours = launched.written(&launched.key).await;
        assert_eq!(
            (ours.0.as_str(), ours.1.as_deref()),
            ("turn_active", Some(DISPATCH))
        );
        assert!(
            ours.2 > earlier.2,
            "the turn start's heartbeat is the newest: {ours:?} {earlier:?}"
        );
        let bound = Some((herdr::HostedState::Bound, launched.nonce.clone()));
        assert_eq!(herdr::row(&launched.pool, &launched.key).await, bound);

        let uri = format!("/turns/{CHANNEL}/cancel");
        let (status, reply) = launched.post(&uri, json!({"force": true})).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["session_key"], launched.key.as_str(), "{reply}");
        assert_eq!(
            reply["termination"],
            result("retired", "retired"),
            "{reply}"
        );
        let retired = Some((herdr::HostedState::Retired, launched.nonce.clone()));
        assert_eq!(herdr::row(&launched.pool, &launched.key).await, retired);
        assert_eq!(closes(&rig), 1, "the launched pane closed once");
        assert_eq!(launched.dispatch(DISPATCH).await, "cancelled");
        assert_eq!(
            launched.written(&other).await,
            earlier,
            "the other row is untouched"
        );
        launched.finish().await;
    });
}

// A force-kill of a row only production writers wrote retires the launched execution, then
// disconnects the session and makes one retry of its dispatch.
#[test]
fn force_production_launch_e08_terminates_pg() {
    let Some((rig, runtime)) = rig_writer("force_production_launch_e08_terminates_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let launched = Launched::new(&rig).await;
        launched.turn_started().await;
        let path = format!(
            "/sessions/{}/force-kill",
            launched.key.replace('/', "%2F").replace(':', "%3A")
        );
        let body = json!({"retry": true, "reason": "writer test"});
        let (status, reply) = launched.post(&path, body).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(
            reply["termination"],
            result("retired", "retired"),
            "{reply}"
        );
        assert!(reply["retry_dispatch_id"].is_string(), "{reply}");
        let retired = Some((herdr::HostedState::Retired, launched.nonce.clone()));
        assert_eq!(herdr::row(&launched.pool, &launched.key).await, retired);
        assert_eq!(closes(&rig), 1, "the launched pane closed once");
        let ours = launched.written(&launched.key).await;
        assert_eq!((ours.0.as_str(), ours.1.as_deref()), ("disconnected", None));
        assert_eq!(launched.dispatch(DISPATCH).await, "failed");
        let retry = reply["retry_dispatch_id"].as_str().unwrap();
        assert_eq!(launched.dispatch(retry).await, "pending");
        launched.finish().await;
    });
}
