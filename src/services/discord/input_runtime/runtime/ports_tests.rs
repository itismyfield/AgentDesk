use super::*;
use crate::services::discord::input_runtime::clear::ClearHost;
use crate::services::discord::input_runtime::fence::{Gate, Mode};
use crate::services::discord::input_runtime::supervisor::mapping::Check;
use crate::services::discord::{DiscordSession, make_shared_data_for_tests_with_storage};
use crate::services::tui_input::rows::RowState;
use crate::services::tui_input::transition::{Move, Outcome};
use serde_json::json;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn shared(channel: u64, pool: Option<sqlx::PgPool>) -> Arc<SharedData> {
    let shared = make_shared_data_for_tests_with_storage(pool);
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Claude;
        settings.owner_user_id = Some(7);
        settings.allowed_channel_ids = vec![channel];
    }
    shared.core.lock().await.sessions.insert(
        ChannelId::new(channel),
        DiscordSession {
            session_id: Some("g2b-session".into()),
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(channel),
            channel_name: Some(format!("g2b-{channel}-cc")),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: shared.restart.current_generation,
        },
    );
    shared
}

async fn ports(shared: Arc<SharedData>, channel: u64, probe: Probe) -> RuntimePorts {
    RuntimePorts::new(
        shared,
        Arc::new(Http::new("test-token")),
        fence::population_root().unwrap(),
        channel,
        probe,
    )
    .await
    .unwrap()
}

#[test]
fn b_runtime_ports_reject_unresolved_resources_without_effects() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_910;
        let shared = shared(channel, None).await;
        let population = fence::population_root().unwrap();
        let http = Arc::new(Http::new("test-token"));
        assert!(
            RuntimePorts::new(
                shared.clone(),
                http.clone(),
                population.join("foreign"),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        shared.settings.write().await.provider = ProviderKind::Codex;
        assert!(
            RuntimePorts::new(
                shared.clone(),
                http.clone(),
                population.clone(),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        shared.settings.write().await.provider = ProviderKind::Claude;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&ChannelId::new(channel))
            .unwrap()
            .channel_id = Some(channel + 1);
        assert!(
            RuntimePorts::new(shared, http, population.clone(), channel, Probe::default())
                .await
                .is_err()
        );
        assert!(!population.join("input_ledger").exists());
        assert!(!population.join("discord_pending_queue").exists());
    });
}

#[test]
fn b_adopt_requires_all_provider_mapping_views_ports() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_911;
        let selected = shared(channel, None).await;
        let mut foreign = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut foreign).unwrap().provider = ProviderKind::Codex;
        let probe = Probe::default();
        let port = ports(selected.clone(), channel, probe.clone()).await;
        assert_eq!(port.mapping().check(channel), Check::Pending);
        assert!(probe.adopt(
            &[ProviderKind::Claude, ProviderKind::Codex],
            vec![selected, foreign.clone()]
        ));
        for reverse in [false, true] {
            let (parent, thread) = if reverse {
                (channel + 1, channel)
            } else {
                (channel, channel + 1)
            };
            foreign
                .dispatch
                .thread_parents
                .insert(ChannelId::new(parent), ChannelId::new(thread));
            let check = port.mapping().check(channel);
            assert!(
                matches!(check, Check::Mapped(_)),
                "actual foreign runtime map must be read"
            );
            let Check::Mapped(edge) = check else {
                unreachable!()
            };
            assert_eq!(
                (edge.writer.as_str(), edge.parent, edge.thread),
                ("codex", parent, thread)
            );
            foreign.dispatch.thread_parents.clear();
        }
        let partial = Probe::default();
        assert!(partial.adopt(
            &[ProviderKind::Claude, ProviderKind::Codex],
            vec![port.shared.clone()]
        ));
        let partial_port = ports(port.shared.clone(), channel, partial).await;
        assert_eq!(partial_port.mapping().check(channel), Check::Unavailable);
        assert_eq!(port.mapping().check(channel), Check::Empty);
    });
}

#[test]
fn b_runtime_ports_sources_are_read_only_and_unreadable_is_not_empty() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_914;
        let selected = shared(channel, None).await;
        let population = fence::population_root().unwrap();
        let path = crate::services::discord::inflight::inflight_state_path(
            &crate::services::discord::inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            channel,
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"unreadable inflight").unwrap();
        assert!(
            RuntimePorts::new(
                selected.clone(),
                Arc::new(Http::new("test-token")),
                population.clone(),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"unreadable inflight");
        let mut row = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            Some(format!("g2b-{channel}-cc")),
            7,
            63_258_141,
            0,
            "active sources".into(),
            Some("g2b-session".into()),
            None,
            None,
            None,
            0,
        );
        row.source_message_ids = vec![63_258_142, 63_258_141, 0];
        let bytes = serde_json::to_vec(&row).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let port = ports(selected.clone(), channel, Probe::default()).await;
        assert_eq!(port.resources.active_sources, [63_258_141, 63_258_142]);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "read-only source probe cannot backfill old rows"
        );
        row.provider = "codex".into();
        std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
        assert!(
            RuntimePorts::new(
                selected,
                Arc::new(Http::new("test-token")),
                population.clone(),
                channel,
                Probe::default()
            )
            .await
            .is_err()
        );
        assert!(!population.join("input_ledger").exists());
    });
}

#[test]
fn b_runtime_ports_freeze_failure_preserves_legacy_and_closing() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let channel = 6_326_912;
        let shared = shared(channel, None).await;
        let mut port = ports(shared.clone(), channel, Probe::default()).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let closing = Arc::new(gate.close().unwrap());
        let queue = port
            .root
            .join("discord_pending_queue/claude/test-token-hash")
            .join(format!("{channel}.json"));
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        let original = serde_json::to_vec(
            &json!([{"author_id":7,"message_id":63258121,"text":"preserve","channel_id":channel}]),
        )
        .unwrap();
        std::fs::write(&queue, &original).unwrap();
        let held = shared
            .acquire_session_transition(ChannelId::new(channel))
            .await
            .unwrap();
        assert_eq!(port.freeze(closing.clone()).await, Err(Failure::Busy));
        assert_eq!(gate.mode(), Mode::Closing);
        assert_eq!(std::fs::read(&queue).unwrap(), original);
        assert!(port.transition(closing.clone()).is_err());
        assert!(!port.root.join("input_ledger").exists());
        drop(held);
        assert!(
            port.clear().await.is_none(),
            "missing PG cannot be clear success"
        );
        port.freeze(closing.clone()).await.unwrap();
        assert_eq!(gate.mode(), Mode::Frozen);
        assert!(
            port.transition(closing).is_err(),
            "missing PG cannot become a Move host"
        );
        assert!(queue.exists());
    });
}

#[test]
fn b_runtime_ports_freeze_clear_move_fail_closed_pg() {
    assert_eq!(
        std::env::var("AGENTDESK_REQUIRE_PG").as_deref(),
        Ok("1"),
        "required PG lane must be explicit"
    );
    // Ledger directory creation refuses symlink ancestors, including macOS /var and /tmp.
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent.canonicalize().unwrap()).unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    runtime().block_on(async {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create("adk_g2b_ports", "G2-B production ports").await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let channel = 6_326_913;
        let waiting_channel = 6_326_915;
        let waiting_shared = shared(waiting_channel, Some(pool.clone())).await;
        let registry = Box::leak(Box::new(crate::services::discord::input_runtime::supervisor::Registry::new()));
        let reserved = registry.reserve_for_test(&ProviderKind::Claude, waiting_channel, &fence::population_root().unwrap()).unwrap();
        let runtime = crate::services::discord::input_runtime::runtime::Runtime::new(
            reserved,
            crate::services::discord::input_runtime::supervisor::Config {
                provider: ProviderKind::Claude, channel: waiting_channel,
                root: fence::population_root().unwrap(),
                request: crate::services::discord::input_runtime::supervisor::Request::Ledger,
                refusal: None,
                binding: Arc::new(crate::services::tui_o::writer::binding::ChannelBindingLog::new(waiting_channel, crate::services::tui_o::shadow::ShadowProvider::Claude)),
            },
            waiting_shared.clone(), Arc::new(Http::new("test-token")),
            vec![ProviderKind::Claude, ProviderKind::Codex],
        ).await.unwrap();
        assert!(registry.health_reasons().iter().any(|reason| reason.contains("runtime_view_pending")));
        let (views, handoff) = tokio::sync::oneshot::channel();
        let (commands, mailbox) = tokio::sync::mpsc::channel(4);
        let waiting = tokio::spawn(runtime.run(handoff, mailbox));
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "actual runtime retains reservation while final views are pending");
        assert!(!fence::population_root().unwrap().join("input_ledger").exists(), "actual runtime cannot open a WAL before final views");
        assert_eq!(fence::lookup(&ProviderKind::Claude, waiting_channel).unwrap().mode(), Mode::Closing);
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        drop(views);
        drop(commands);
        drop(waiting_shared);
        let shared = shared(channel, Some(pool.clone())).await;
        let mut port = ports(shared.clone(), channel, Probe::default()).await;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let closing = Arc::new(gate.close().unwrap());
        let queue = port.root.join("discord_pending_queue/claude/test-token-hash").join(format!("{channel}.json"));
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        std::fs::write(&queue, serde_json::to_vec(&json!([{"author_id":7,"message_id":63258131,"text":"move actual port","channel_id":channel}])).unwrap()).unwrap();
        port.freeze(closing.clone()).await.unwrap();
        assert_eq!(gate.mode(), Mode::Frozen);
        let lock = shared.acquire_session_transition(ChannelId::new(channel)).await.unwrap();
        drop(lock);
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(&shared.token_hash, &shared.provider, &port.resources.tmux);
        crate::services::discord::inflight::seed_session_row_keyed(&pool, &session_key, channel, None).await;
        let (clear, guard) = port.clear().await.expect("real LedgerClear must locate the seeded PG session");
        assert_eq!(clear.identity().tmux, port.resources.tmux);
        drop(guard);
        drop(clear);
        let mut host = port.transition(closing.clone()).unwrap();
        let population = port.root.clone();
        let (mut host, row) = tokio::task::spawn_blocking(move || fence::blocking(|| {
            let _worker = fence::effect::worker_scope(None);
            let mut lease = crate::services::tui_input::ledger::LedgerLease::new(&population, channel);
            let captured = crate::services::tui_input::transition::Host::collect(&mut host, lease.get().unwrap());
            assert!(captured.is_ok(), "actual production Files::frozen collects with worker and Closing capability: {:?}", captured.as_ref().err());
            assert_eq!(captured.unwrap().len(), 1, "actual production port must capture the queued source");
            let prepared = Move::prepare(&mut lease, &mut host);
            assert!(prepared.is_ok(), "actual production Files::frozen retains Closing capability: {:?}", prepared.as_ref().err());
            let mut movement = prepared.unwrap();
            assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
            let row = lease.get().unwrap().rows().unwrap().row(63_258_131).unwrap().clone();
            assert_eq!(row.state, RowState::Received);
            assert!(!queue.exists(), "actual Files retirement executed");
            (host, row)
        })).await.unwrap();
        host.notice(Some(63_258_131), "row_held").unwrap();
        assert_eq!(RuntimePorts::take_notices(&mut host), [(Some(63_258_131), "row_held")]);
        assert!(RuntimePorts::take_notices(&mut host).is_empty());
        shared.settings.write().await.owner_user_id = Some(8);
        assert_eq!(host.enqueue(63_258_131, &row).unwrap(), EnqueueOutcome::Rejected, "handback author is rechecked, never authorized by channel alone");
        shared.settings.write().await.owner_user_id = Some(7);
        let current = shared.settings.write().await;
        assert!(host.enqueue(63_258_131, &row).is_err(), "unreadable auth snapshot holds responsibility");
        drop(current);
        shared.settings.write().await.provider = ProviderKind::Codex;
        assert!(host.enqueue(63_258_131, &row).is_err(), "changed provider settings cannot authorize old handback");
        assert_eq!(gate.mode(), Mode::Frozen);
        drop(host);
        drop(port);
        drop(shared);
        pool.close().await;
        db.drop().await;
    });
}
