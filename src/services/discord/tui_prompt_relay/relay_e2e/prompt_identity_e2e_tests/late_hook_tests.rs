//! Delayed hook claims and queued notices retain their original execution.

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::claude_tui::hook_server::observation_ingress::tests::claude;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, PANE};
use dedupe::binding_context::{
    BINDING_HEADER, ClaudeResumeSource, HookBindingEnvelope, PreparedIncarnation,
};
use dedupe::binding_events::{self as log, BindingEvent, BindingTarget, SourceId};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const LOGICAL: &str = "herdr-p3a-late-hook";
const SID: &str = "75030000-0000-4000-8000-000000000001";
const OLD_PROMPT: &str = "old execution human question";
const NEW_PROMPT: &str = "new execution human question";

fn isolated(name: &str) -> bool {
    const CHILD: &str = "ADK_P3A_LATE_HOOK_CHILD";
    if let Some(value) = std::env::var_os(CHILD) {
        assert_eq!(value, name);
        crate::config::session_hosts::install_for_test(
            Some("local"),
            &[(super::super::CHANNEL_ID, "local")],
        );
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    let exact = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture", "--test-threads=1"])
        .env(CHILD, name)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("{stdout}\n{stderr}");
    assert!(
        output.status.success()
            && stdout.matches("running 1 test").count() == 1
            && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "{exact}: {}\n{stdout}\n{stderr}",
        output.status
    );
    false
}

fn run(name: &str, late: bool, permit: bool) {
    if !isolated(name) {
        return;
    }
    let rig = HerdrRig::start();
    let endpoint = crate::config::session_hosts::herdr_endpoint(super::super::CHANNEL_ID).unwrap();
    let registry = rig.thread_installer(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let events = tempfile::tempdir().unwrap();
    let root = events.path().to_path_buf();
    let install = move || {
        registry();
        dedupe::binding_events::set_test_root(Some(&root));
    };
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(Box::pin(async {
            if permit {
                permit_scenario(rig, events.path()).await;
            } else {
                scenario(rig, events.path(), late).await;
            }
        }));
}

#[test]
fn old_hook_after_ingress_and_before_observation_creates_no_ownership_pg() {
    run(
        "old_hook_after_ingress_and_before_observation_creates_no_ownership_pg",
        false,
        false,
    );
}

#[test]
fn old_observation_queued_before_new_bound_posts_no_notice_pg() {
    run(
        "old_observation_queued_before_new_bound_posts_no_notice_pg",
        true,
        false,
    );
}

#[test]
fn staged_direct_notice_keeps_the_effect_gate_through_its_post_pg() {
    run(
        "staged_direct_notice_keeps_the_effect_gate_through_its_post_pg",
        false,
        true,
    );
}

struct Fixture {
    h: RelayE2eHarness,
    database: TestPostgresDb,
    rig: HerdrRig,
    source: SourceId,
    log_path: PathBuf,
    record: HostedExecution,
    key: String,
    old: PreparedIncarnation,
    new: PreparedIncarnation,
}

impl Fixture {
    async fn start(rig: HerdrRig, events: &Path) -> Self {
        let mut database = None;
        let h = Box::pin(RelayE2eHarness::start_inner(
            super::super::ProviderStub::Success,
            true,
            async {
                let db = TestPostgresDb::create().await;
                let pool = db.connect_and_migrate().await;
                database = Some(db);
                Some(pool)
            },
            true,
        ))
        .await;
        h.answer_placeholders_immediately();
        h.cache_relay_transport();
        h.use_mock_provider_bot(WAIT).await;
        let endpoint =
            crate::config::session_hosts::herdr_endpoint(super::super::CHANNEL_ID).unwrap();
        let projects = h.root.path().join("claude/projects");
        let path = projects.join("direct").join(format!("{SID}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let historical = json!({"type":"user", "uuid":"7503-history-row", "sessionId":SID,
            "timestamp":chrono::Utc::now().to_rfc3339(),
            "message":{"role":"user", "content":"historical source row"}});
        std::fs::write(&path, format!("{historical}\n")).unwrap();
        dedupe::register_tmux_channel(LOGICAL, super::super::CHANNEL_ID);
        dedupe::register_provider_session(PROVIDER_KEY, SID, LOGICAL);
        dedupe::register_tmux_runtime_binding(LOGICAL, claude(&path, SID));
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for(SID, &path).unwrap();
        let old = PreparedIncarnation::prepare_at(
            "claude",
            LOGICAL,
            Some(super::super::CHANNEL_ID),
            Some(SID),
            false,
            Some(projects),
        )
        .unwrap();
        let mut resumed = old.context.clone();
        resumed.created_at = chrono::Utc::now();
        resumed.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        resumed.launch_mode = "resume".into();
        resumed.claude_resume = Some(ClaudeResumeSource {
            source: source.clone(),
            previous_nonce: old.context.execution_nonce.clone(),
        });
        let new = PreparedIncarnation::create(resumed).unwrap();
        let root = crate::config::runtime_root().unwrap().display().to_string();
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: h.shared.token_hash.clone(),
            channel_id: super::super::CHANNEL_ID.to_string(),
            logical_key: LOGICAL.into(),
            owner_node: endpoint.execution_node.clone(),
            runtime_root: root.clone(),
        };
        let nonce = old.context.execution_nonce.clone();
        let record = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: nonce.clone(),
            owner: owner.clone(),
            expected: Some(rig.expected(&nonce)),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: endpoint.execution_node,
                endpoint_config_key: endpoint.key,
                socket_addr: endpoint.socket_path.display().to_string(),
                named_session: endpoint.herdr_session,
                pane_id: PANE.into(),
            }),
            source_ref: SourceRef {
                runtime_root: root,
                channel: owner.channel_id.clone(),
                provider: "claude".into(),
                logical_key: LOGICAL.into(),
                execution_nonce: nonce,
                initial_source: None,
                baseline_event_seq: None,
            },
        };
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &h.shared.token_hash,
            &h.data.provider,
            LOGICAL,
        );
        let pool = h.shared.pg_pool.as_ref().unwrap();
        sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)")
            .bind(&key).bind(&owner.discord_token_hash).bind(&owner.channel_id)
            .bind(serde_json::to_value(&record).unwrap()).execute(pool).await.unwrap();
        let log_path = events
            .join(log::BINDING_EVENTS_DIR)
            .join(format!("{}.log", super::super::CHANNEL_ID));
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        let fx = Self {
            h,
            database: database.unwrap(),
            rig,
            source,
            log_path,
            record,
            key,
            old,
            new,
        };
        fx.publish(false).await;
        fx
    }

    async fn publish(&self, new: bool) {
        use std::io::Write;
        let context = if new { &self.new } else { &self.old };
        let nonce = &context.context.execution_nonce;
        let mut record = self.record.clone();
        record.execution_nonce.clone_from(nonce);
        record.source_ref.execution_nonce.clone_from(nonce);
        record.expected = Some(self.rig.expected(nonce));
        self.rig.run_provider(&context.path, false);
        self.rig.show_panes(&[PANE]);
        std::fs::write(
            crate::services::tmux_common::session_temp_path(LOGICAL, "spawn_nonce"),
            nonce,
        )
        .unwrap();
        std::fs::write(
            crate::services::tmux_common::session_temp_path(LOGICAL, "host_kind"),
            "herdr",
        )
        .unwrap();
        dedupe::install_herdr_execution(LOGICAL, nonce);
        dedupe::admit_herdr_execution(LOGICAL, nonce);
        let event = BindingEvent {
            seq: log::records_strict(super::super::CHANNEL_ID)
                .unwrap()
                .unwrap_or_default()
                .len() as u64
                + 1,
            channel_id: super::super::CHANNEL_ID,
            provider: "claude".into(),
            tmux_session: LOGICAL.into(),
            execution_nonce: Some(nonce.clone()),
            old: new.then(|| self.source.clone()),
            new: BindingTarget::Source(self.source.clone()),
            cause: if new {
                log::BindingCause::Resume
            } else {
                log::BindingCause::Startup
            },
            parent_hint: None,
            evidence: log::BindingEvidence {
                hook_event: Some("session_start".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .unwrap();
        let mut value = serde_json::to_value(event).unwrap();
        value["verified"] = json!(true);
        writeln!(file, "{value}").unwrap();
        sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE session_key=$2")
            .bind(serde_json::to_value(record).unwrap())
            .bind(&self.key)
            .execute(self.h.shared.pg_pool.as_ref().unwrap())
            .await
            .unwrap();
        assert!(
            dedupe::hosted_source::current_execution_for_adoption(
                &self.h.shared,
                PROVIDER_KEY,
                self.h.channel_id,
                LOGICAL
            )
            .await,
            "fixture must install a live canonical Bound execution"
        );
    }

    async fn hook(&self, hooks: &HookServerState, new: bool, kind: &str, prompt: &str) {
        let context = if new { &self.new } else { &self.old };
        let envelope = HookBindingEnvelope::capture_from_env(PROVIDER_KEY, |name| {
            (name == "AGENTDESK_BINDING_CONTEXT").then(|| context.path.clone().into_os_string())
        })
        .encode()
        .unwrap();
        let payload = json!({"hook_event_name":kind, "session_id":SID, "transcript_path":self.source.path,
            "prompt":prompt, "prompt_id":if new { "new-hook" } else { "old-hook" }});
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/hooks/claude/{kind}?session_id={SID}"))
            .header("content-type", "application/json")
            .header(BINDING_HEADER, envelope)
            .body(Body::from(payload.to_string()))
            .unwrap();
        let answer = hook_receiver_router_with_state(hooks.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert!(
            answer.status().is_success(),
            "actual ingress ACK: {}",
            answer.status()
        );
    }
}

async fn scenario(rig: HerdrRig, events: &Path, late: bool) {
    let fx = Fixture::start(rig, events).await;
    let hooks =
        HookServerState::new_with_claude_projects_root(fx.h.root.path().join("claude/projects"));
    let probe = Arc::new(super::super::super::HookObserverProbe::default());
    let relay_paused = Arc::new(tokio::sync::Notify::new());
    let relay_release = Arc::new(tokio::sync::Notify::new());
    let held = Arc::new(Mutex::new(None::<dedupe::ObservedTuiPrompt>));
    let relay_count = Arc::new(AtomicUsize::new(0));
    let shared = fx.h.shared.clone();
    let (paused, release, saved, completed) = (
        relay_paused.clone(),
        relay_release.clone(),
        held.clone(),
        relay_count.clone(),
    );
    super::super::super::spawn_tui_prompt_relay_observer_inner(
        PROVIDER_KEY.into(),
        hooks.subscribe(),
        move |prompt| {
            let (shared, paused, release, saved, completed) = (
                shared.clone(),
                paused.clone(),
                release.clone(),
                saved.clone(),
                completed.clone(),
            );
            Box::pin(async move {
                if late && prompt.prompt == OLD_PROMPT {
                    *saved.lock().unwrap() = Some(prompt.clone());
                    paused.notify_one();
                    release.notified().await;
                }
                super::super::super::relay_observed_prompt(&shared, prompt).await;
                completed.fetch_add(1, Ordering::SeqCst);
            })
        },
        Some(probe.clone()),
    );
    if !late {
        probe.pause_after_first.store(true, Ordering::SeqCst);
        fx.hook(&hooks, false, "Stop", "").await;
        assert!(
            wait_until(WAIT, || {
                let ready = probe.paused.load(Ordering::SeqCst);
                Box::pin(async move { ready })
            })
            .await,
            "actual observer paused after its priming non-prompt hook"
        );
    }
    fx.hook(&hooks, false, "UserPromptSubmit", OLD_PROMPT).await;
    if late {
        tokio::time::timeout(WAIT, relay_paused.notified())
            .await
            .unwrap();
        assert_eq!(
            probe.observation_calls.load(Ordering::SeqCst),
            1,
            "real observation reached its lease commit"
        );
        assert!(dedupe::external_input_relay_lease_present(
            PROVIDER_KEY,
            LOGICAL,
            fx.h.channel_id.get()
        ));
        assert!(dedupe::is_ssh_direct_observation_pending(
            PROVIDER_KEY,
            LOGICAL
        ));
    }
    fx.publish(true).await;
    let lease = dedupe::external_input_relay_lease(PROVIDER_KEY, LOGICAL, fx.h.channel_id.get());
    if late {
        relay_release.notify_one();
        wait_for_relays(&relay_count, 1).await;
        assert_eq!(
            announcements_of(&fx.h, OLD_PROMPT),
            0,
            "queued old nonce must not post its first notice"
        );
        assert_eq!(
            dedupe::external_input_relay_lease(PROVIDER_KEY, LOGICAL, fx.h.channel_id.get())
                .map(|lease| lease.generation),
            lease.map(|lease| lease.generation),
            "a late adoption fence preserves its prior admitted lease"
        );
        let pin = held
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .hosted_source
            .clone()
            .unwrap();
        assert!(
            dedupe::hosted_source::validate(&pin),
            "new direct adoption does not invalidate the admitted old reader's source"
        );
    } else {
        probe.pause_after_first.store(false, Ordering::SeqCst);
        probe.release.notify_one();
        assert!(
            wait_until(WAIT, || {
                let ready = probe.processed.load(Ordering::SeqCst) >= 2;
                Box::pin(async move { ready })
            })
            .await,
            "old hook reached the actual observation entry"
        );
        assert_eq!(probe.observation_calls.load(Ordering::SeqCst), 1);
        assert!(
            !dedupe::external_input_relay_lease_present(
                PROVIDER_KEY,
                LOGICAL,
                fx.h.channel_id.get()
            ),
            "old hook creates no first lease after the current execution changes"
        );
        assert!(
            !dedupe::is_ssh_direct_observation_pending(PROVIDER_KEY, LOGICAL),
            "old hook creates no SSH ownership"
        );
        assert_eq!(
            relay_count.load(Ordering::SeqCst),
            0,
            "old hook publishes no new observation"
        );
        assert_eq!(announcements_of(&fx.h, OLD_PROMPT), 0);
    }
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only(
            &fx.h.data.provider,
            fx.h.channel_id.get()
        )
        .is_none(),
        "old hook creates no synthetic lifecycle"
    );
    fx.hook(&hooks, true, "UserPromptSubmit", NEW_PROMPT).await;
    wait_for_relays(&relay_count, if late { 2 } else { 1 }).await;
    assert_eq!(
        announcements_of(&fx.h, NEW_PROMPT),
        1,
        "current nonce's actual hook has a positive announcement"
    );
    assert_eq!(announcements_of(&fx.h, OLD_PROMPT), 0);
    assert!(
        fx.rig.sends().is_empty(),
        "mirroring direct input sends no provider input"
    );
    assert!(fx.h.unhandled_requests().is_empty());
    drop(hooks);
    let Fixture { h, database, .. } = fx;
    let pool = h.shared.pg_pool.clone().unwrap();
    drop(h);
    pool.close().await;
    database.drop().await;
}

async fn staged_relay_with_permit(
    shared: &Arc<crate::services::discord::SharedData>,
    prompt: dedupe::ObservedTuiPrompt,
    store: &crate::services::claude::herdr_resume_plan::legacy_handoff::Store,
    channel: poise::serenity_prelude::ChannelId,
) {
    let _permit = store.direct_effect_permit().unwrap();
    assert_eq!(
        dedupe::hosted_source::adoption_channel(shared, &prompt, Some(channel)).await,
        Some(channel),
        "the staged permit revalidates the original canonical nonce before its first effect"
    );
    super::super::super::relay_observed_prompt(shared, prompt).await;
}

async fn selector_clear_under_the_same_gate(
    store: Arc<crate::services::claude::herdr_resume_plan::legacy_handoff::Store>,
    pool: sqlx::PgPool,
    key: String,
    next_nonce: String,
    entered: Arc<AtomicUsize>,
) -> Result<(), String> {
    use crate::services::claude::herdr_resume_plan::legacy_handoff::Invalidation;
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        store.with_selector_change(Invalidation::Clear, || {
            entered.fetch_add(1, Ordering::SeqCst);
            runtime.block_on(async {
                sqlx::query("UPDATE sessions SET hosted_execution=NULL WHERE session_key=$1")
                    .bind(&key)
                    .execute(&pool)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>(())
            })?;
            std::fs::write(
                crate::services::tmux_common::session_temp_path(LOGICAL, "spawn_nonce"),
                next_nonce,
            )
            .map_err(|error| error.to_string())?;
            dedupe::withhold_herdr_execution(LOGICAL, None);
            Ok(())
        })
    })
    .await
    .unwrap()
}

async fn permit_scenario(rig: HerdrRig, events: &Path) {
    use crate::services::claude::herdr_resume_plan::legacy_handoff::{Owner, Store};
    let fx = Fixture::start(rig, events).await;
    fx.publish(true).await;
    let root = crate::config::runtime_root()
        .unwrap()
        .canonicalize()
        .unwrap();
    let owner = Owner {
        runtime_root: root.clone(),
        provider: "claude".into(),
        token_hash: fx.h.shared.token_hash.clone(),
        channel: fx.h.channel_id.get(),
        logical_key: LOGICAL.into(),
    };
    let store = Arc::new(Store::open(&root, &owner).unwrap());
    let hooks =
        HookServerState::new_with_claude_projects_root(fx.h.root.path().join("claude/projects"));
    let probe = Arc::new(super::super::super::HookObserverProbe::default());
    let relayed = Arc::new(AtomicUsize::new(0));
    let (shared, guarded, completed, channel) = (
        fx.h.shared.clone(),
        store.clone(),
        relayed.clone(),
        fx.h.channel_id,
    );
    super::super::super::spawn_tui_prompt_relay_observer_inner(
        PROVIDER_KEY.into(),
        hooks.subscribe(),
        move |prompt| {
            let (shared, guarded, completed) = (shared.clone(), guarded.clone(), completed.clone());
            Box::pin(async move {
                staged_relay_with_permit(&shared, prompt, &guarded, channel).await;
                completed.fetch_add(1, Ordering::SeqCst);
            })
        },
        Some(probe.clone()),
    );
    fx.h.hold_next_note();
    fx.hook(&hooks, true, "UserPromptSubmit", NEW_PROMPT).await;
    assert!(
        fx.h.wait_for_held_note(WAIT).await,
        "the actual first Discord POST reached the transport barrier"
    );
    assert_eq!(probe.observation_calls.load(Ordering::SeqCst), 1);
    let pool = fx.h.shared.pg_pool.clone().unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    let blocked = selector_clear_under_the_same_gate(
        store.clone(),
        pool.clone(),
        fx.key.clone(),
        fx.old.context.execution_nonce.clone(),
        entered.clone(),
    )
    .await;
    let entered_before_post_return = entered.load(Ordering::SeqCst);
    let before: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key=$1")
            .bind(&fx.key)
            .fetch_one(&pool)
            .await
            .unwrap();
    fx.h.release_held_note();
    wait_for_relays(&relayed, 1).await;
    assert!(
        blocked.is_err(),
        "selector mutation cannot overtake an in-flight direct POST"
    );
    assert_eq!(
        entered_before_post_return, 0,
        "the common effect gate blocks the selector commit itself"
    );
    assert!(
        before.is_some(),
        "the canonical Bound row is unchanged while its POST awaits"
    );
    assert_eq!(
        announcements_of(&fx.h, NEW_PROMPT),
        1,
        "the guarded first notice completed exactly once"
    );
    selector_clear_under_the_same_gate(
        store,
        pool.clone(),
        fx.key.clone(),
        fx.old.context.execution_nonce.clone(),
        entered.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "the same selector commit progresses after permit release"
    );
    let after: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE session_key=$1")
            .bind(&fx.key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        after.is_none(),
        "the gated selector callback actually committed to PostgreSQL"
    );
    assert!(fx.rig.sends().is_empty());
    assert!(fx.h.unhandled_requests().is_empty());
    drop(hooks);
    let Fixture { h, database, .. } = fx;
    drop(h);
    pool.close().await;
    database.drop().await;
}
