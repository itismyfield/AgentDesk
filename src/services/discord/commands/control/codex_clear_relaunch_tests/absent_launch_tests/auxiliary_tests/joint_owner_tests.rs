//! Joint A2+B1 owner proofs preserve absence admission and the real FIFO provider rail.

use super::super::negative_tests::{payload_snapshot, persisted_queue_bytes};
use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::discord::health::HealthRegistry;

const REGISTRY_NAME: &str = "AgentDesk-codex-bundle-registered-foreign";
const ROUTINE_NAME: &str = "AgentDesk-claude-routine-bundle-foreign";
const FOREIGN_CHANNEL: u64 = CANARY_CHANNEL + 9_900;
const ROUTINE_CHANNEL: u64 = CANARY_CHANNEL + 9_901;

pub(in crate::services::discord) struct OwnerScope {
    _registry: Arc<HealthRegistry>,
}

#[derive(Clone, Copy)]
struct Evidence {
    registry: bool,
    routine: bool,
    no_core: bool,
}

pub(in crate::services::discord) async fn install_positive(
    fixture: &mut CanaryFixture,
    no_core: bool,
) -> OwnerScope {
    install(
        fixture,
        Evidence {
            registry: true,
            routine: true,
            no_core,
        },
    )
    .await
}

async fn install(fixture: &mut CanaryFixture, evidence: Evidence) -> OwnerScope {
    let registry = Arc::new(HealthRegistry::new());
    Arc::get_mut(&mut fixture.shared)
        .expect("install membership before publishing the caller Arc")
        .health_registry = Arc::downgrade(&registry);
    registry
        .register("codex".into(), fixture.shared.clone())
        .await;
    let pool = fixture.shared.pg_pool.clone().unwrap();
    let mut registered = Vec::new();
    for (provider, token) in [
        (ProviderKind::Codex, "bundle-registered-codex-runtime"),
        (ProviderKind::Claude, "bundle-registered-claude-runtime"),
    ] {
        let mut runtime =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let mutable = Arc::get_mut(&mut runtime).unwrap();
        mutable.provider = provider.clone();
        mutable.token_hash = token.into();
        mutable.health_registry = Arc::downgrade(&registry);
        runtime.settings.write().await.provider = provider.clone();
        registry
            .register(provider.as_str().into(), runtime.clone())
            .await;
        registered.push(runtime);
    }
    let omit_registry = std::env::var("AGENTDESK_N2B_TEST_MUTATION")
        .is_ok_and(|value| value == "joint-owner-proof");
    if evidence.registry && !omit_registry {
        assert!(
            registered[0]
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(
                    REGISTRY_NAME,
                    serenity::ChannelId::new(FOREIGN_CHANNEL),
                )
        );
    }
    assert_eq!(
        registered[0]
            .tmux_watchers
            .owner_channel_for_tmux_session(REGISTRY_NAME),
        (evidence.registry && !omit_registry).then_some(serenity::ChannelId::new(FOREIGN_CHANNEL)),
    );
    if evidence.routine {
        sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('bundle-owner-routine', 'fixture.js', 'bundle owner')")
            .execute(&pool).await.unwrap();
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &registered[1].token_hash,
            &ProviderKind::Claude,
            ROUTINE_NAME,
        );
        sqlx::query("INSERT INTO routine_runs (id, routine_id, status, turn_id, result_json, owned_tmux_session, finished_at) VALUES ('bundle-owner-receipt', 'bundle-owner-routine', 'succeeded', 'bundle-foreign-started-turn', $1, $2, NOW())")
            .bind(json!({"channel_id": ROUTINE_CHANNEL.to_string()}))
            .bind(&key)
            .execute(&pool).await.unwrap();
        let read_back: (String, String, String) = sqlx::query_as(
            "SELECT owned_tmux_session, turn_id, result_json ->> 'channel_id' FROM routine_runs WHERE id='bundle-owner-receipt'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            read_back,
            (
                key,
                "bundle-foreign-started-turn".into(),
                ROUTINE_CHANNEL.to_string()
            ),
        );
    }
    fixture
        .fake
        .add_unwatched_inventory(&format!("{REGISTRY_NAME}\n{ROUTINE_NAME}"));
    for name in [REGISTRY_NAME, ROUTINE_NAME] {
        assert!(
            fixture
                .shared
                .tmux_watchers
                .owner_channel_for_tmux_session(name)
                .is_none()
        );
    }
    if evidence.no_core {
        make_binding_only(fixture).await;
    }
    OwnerScope {
        _registry: registry,
    }
}

async fn make_binding_only(fixture: &CanaryFixture) {
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&channel);
    let mut config = (*crate::config_live_reload::current().unwrap()).clone();
    let mut configured = config.agents[0].channels.get("codex").unwrap().clone();
    let crate::config::AgentChannel::Detailed(binding) = &mut configured else {
        panic!("the fixture requires its exact detailed Codex registration")
    };
    binding.name = None;
    binding.aliases.clear();
    config.agents[0].channels.insert("codex", configured);
    let yaml = serde_yaml::to_string(&config).unwrap();
    for path in [
        fixture.root.path().join("config.yaml"),
        crate::runtime_layout::config_file_path(fixture.root.path()),
    ] {
        fs::write(path, &yaml).unwrap();
    }
    crate::config_live_reload::install(config);
    assert!(
        !fixture
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&channel)
    );
    assert!(
        fixture
            .shared
            .tmux_watchers
            .channel_binding(&channel)
            .is_none()
    );
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &fixture.shared.token_hash,
        &ProviderKind::Codex,
        CANARY_TMUX,
    );
    let row: (String, String, String, Option<serde_json::Value>) = sqlx::query_as(
        "SELECT provider, identity_kind, channel_id, hosted_execution FROM sessions WHERE session_key=$1",
    )
    .bind(&key)
    .fetch_one(fixture.shared.pg_pool.as_ref().unwrap())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "codex".into(),
            "discord_channel".into(),
            CANARY_CHANNEL.to_string(),
            None
        )
    );
    assert_eq!(
        crate::services::discord::settings::resolve_workspace(channel, None).as_deref(),
        fixture.root.path().to_str(),
        "binding-only admission must retain its exact private workspace"
    );
    assert!(
        crate::services::discord::adk_session::registered_channel_fallback_name(
            channel,
            &ProviderKind::Codex,
        )
        .is_none(),
        "binding-only admission must derive its canonical name without a fallback"
    );
    let registry = fixture.shared.health_registry.upgrade().unwrap();
    let mut hashes = registry.registered_token_hashes(&ProviderKind::Codex).await;
    hashes.push(fixture.shared.token_hash.clone());
    hashes.sort();
    hashes.dedup();
    let derived = crate::services::discord::host_key_derivation::derive_channel_session_name(
        fixture.shared.pg_pool.as_ref().unwrap(),
        &hashes,
        &ProviderKind::Codex,
        CANARY_CHANNEL,
    )
    .await
    .unwrap_or_else(|reason| panic!("binding-only canonical name was not resolved: {reason:?}"));
    assert_eq!(derived, CANARY_TMUX);
    let raw = dedupe::binding_events::records_strict(CANARY_CHANNEL)
        .unwrap()
        .unwrap();
    let prior = raw
        .iter()
        .find(|event| {
            event.tmux_session == CANARY_TMUX
                && event.execution_nonce.as_deref()
                    == Some(fixture.context.execution_nonce.as_str())
                && match &event.new {
                    dedupe::binding_events::BindingTarget::Source(source)
                    | dedupe::binding_events::BindingTarget::Resolved { source, .. } => {
                        source == &fixture.old_source
                    }
                    _ => false,
                }
        })
        .expect("the actual N2 hook must retain its exact durable Source binding");
    assert!(!dedupe::codex_verified_event_allowed(prior));
    use crate::services::tui_o::writer::binding::{
        BindingEvents, BindingRecord, ChannelBindingLog,
    };
    let filtered = ChannelBindingLog::new(
        CANARY_CHANNEL,
        crate::services::tui_o::shadow::ShadowProvider::Codex,
    )
    .binding_events_since(CANARY_CHANNEL, 0)
    .unwrap();
    assert!(filtered.iter().any(|event| {
        event.seq == prior.seq && matches!(&event.record, BindingRecord::Rejected { .. })
    }));
    eprintln!("BUNDLE_NO_CORE_BINDING raw_exact_n2=1 source_permission=0 filtered_rejected=1");
    let observed = activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel).await;
    let unbound =
        activity::tests::unbound_verdict_for_tests(&fixture.shared, &ProviderKind::Codex, channel)
            .await;
    eprintln!(
        "BUNDLE_NO_CORE_PRECONDITIONS canonical_name={derived} activity={observed:?} unbound={unbound:?}"
    );
    assert_eq!(
        observed.activity,
        activity::Activity::Idle,
        "binding-only exact foreign ownership must prove idle: {observed:?}; {unbound:?}"
    );
    assert!(
        !crate::services::discord::host_defer_gate::nameless_channel_deferred(
            &fixture.shared,
            &ProviderKind::Codex,
            CANARY_CHANNEL,
        )
        .await,
        "binding-only canonical row must pass the nameless host gate"
    );
    assert!(
        !crate::services::discord::router::hosted_tui_promote_readiness_blocked(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
        )
        .await,
        "binding-only exact absence must pass hosted launch readiness"
    );
}

pub(super) async fn assert_bridge_once(
    api: &crate::services::discord::admin_host_guard::tests::Recorder,
    phase: &str,
) {
    let mut sent = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            sent.extend(api.take());
            if sent.iter().any(|call| {
                let mut parts = call.splitn(3, ' ');
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                let body = parts.next().unwrap_or_default();
                matches!(method, "POST" | "PATCH")
                    && path.contains(&format!("/channels/{CANARY_CHANNEL}/messages"))
                    && serde_json::from_str::<serde_json::Value>(body)
                        .is_ok_and(|value| value["content"] == "clear relaunch completed")
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("{phase}: the actual provider bridge produced no completed response; calls={sent:?}")
    });
    super::super::super::assert_one_completed_response(&sent);
}

#[tokio::test(flavor = "current_thread")]
async fn registered_and_routine_foreign_owners_drain_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::registered_and_routine_foreign_owners_drain_restored_fifo_pg"
    )) {
        super::full_fifo_with_owners(None, Some(false)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn no_core_registered_and_routine_foreign_owners_drain_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::no_core_registered_and_routine_foreign_owners_drain_restored_fifo_pg"
    )) {
        super::full_fifo_with_owners(None, Some(true)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_registered_owner_preserves_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_registered_owner_preserves_restored_fifo_pg"
    )) {
        missing_proof_holds(Evidence {
            registry: false,
            routine: true,
            no_core: false,
        })
        .await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_routine_receipt_preserves_restored_fifo_pg() {
    if isolated(concat!(
        module_path!(),
        "::missing_routine_receipt_preserves_restored_fifo_pg"
    )) {
        missing_proof_holds(Evidence {
            registry: true,
            routine: false,
            no_core: false,
        })
        .await;
    }
}

async fn missing_proof_holds(evidence: Evidence) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let api = recorder(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port,
    ));
    let _cli = fixture.cold_cli();
    target_missing(&fixture, true);
    let _owners = install(&mut fixture, evidence).await;
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&channel);
    let identities = restore_instructions(
        &mut fixture,
        vec![
            instruction(CANARY_CHANNEL + 101, Q1),
            instruction(CANARY_CHANNEL + 102, Q2),
        ],
    )
    .await;
    let queue_before = persisted_queue_bytes(&fixture);
    let before = fixture.shared.mailbox(channel).snapshot().await;
    let payload_before = payload_snapshot(&before.intervention_queue);
    let history_before = fs::read(fixture.history()).unwrap();
    let old_io = crate::services::codex::actual_submit_tests::source_io::Watch::start(
        &fixture.old_source.path,
    );
    assert_eq!(
        activity::tests::unbound_verdict_for_tests(&fixture.shared, &ProviderKind::Codex, channel,)
            .await
            .unwrap(),
        "Hold(\"owner_unresolved\")"
    );
    let absence_observation =
        crate::services::discord::codex_absence::readiness(CANARY_CHANNEL, CANARY_TMUX)
            .expect("the queue guard fixture must have a read-only absent inflight observation");
    assert!(
        dedupe::clear_relaunch::absent_capture(CANARY_TMUX, &absence_observation).is_some(),
        "the queue guard fixture must have exact absence launch evidence"
    );
    assert!(
        !dedupe::clear_relaunch::relaunch_open(CANARY_TMUX),
        "absence alone must not satisfy the explicit-clear queue exception"
    );
    let public_activity =
        activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel).await;
    assert_eq!(public_activity.activity, activity::Activity::Unknown);
    assert_eq!(public_activity.reason, "session_identity_unresolved");
    fixture.fake.take_calls();
    let taken = crate::services::discord::queue_dispatch::mailbox_take_next_automatic_intervention(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
    )
    .await;
    assert!(taken.persistence_error.is_none(), "{taken:?}");
    assert!(
        taken.into_intervention().is_none(),
        "a missing exact owner proof must preserve the FIFO head"
    );
    assert!(
        !kickoff(&fixture, &api).await,
        "a missing owner proof entered the provider"
    );
    let after = fixture.shared.mailbox(channel).snapshot().await;
    assert_eq!(queue_identities(&fixture).await, identities);
    assert_eq!(payload_snapshot(&after.intervention_queue), payload_before);
    assert_eq!(persisted_queue_bytes(&fixture), queue_before);
    assert!(after.cancel_token.is_none() && after.pending_user_dispatch.is_none());
    assert_eq!(fs::read(fixture.history()).unwrap(), history_before);
    assert!(!fixture.root.path().join("fresh-context").exists());
    assert_eq!(old_io.stats(), Default::default());
    assert!(fixture.fake.take_calls().iter().all(|call| {
        ["new-session", "kill-session", "send-keys", "paste-buffer"]
            .iter()
            .all(|effect| !call.starts_with(effect))
    }));
}
