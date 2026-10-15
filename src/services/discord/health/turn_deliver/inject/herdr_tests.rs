//! Busy input on a Herdr-hosted Claude channel from the external deliver entry and the queued head:
//! the Bound execution on an in-process Herdr socket, a scripted tmux kept untouched, and PG.
#![cfg(unix)]

use std::io::Write;
use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId};
use serde_json::{Value, json};

use super::super::super::inject_tests::{InjectPane, queue_texts, register_inject_runtime};
use super::super::super::{HumanInputDelivery, HumanInputRequest, deliver_human_input};
use super::super::test_hook;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::{SharedData, router};
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

const BORDER: &str = "────────────────────────────────────────────────────────────";
const BUSY_SCREEN: &str = "⏺ Working on it.\n\n✻ Thinking… (12s · esc to interrupt)\n\n";
const FOOTER: &str = "  ⏵⏵ bypass permissions on (shift+tab to cycle)";

/// Every runtime thread admits Herdr input, as an unstopped node does.
fn runtime() -> tokio::runtime::Runtime {
    let open = || {
        let stop = std::env::temp_dir().join(format!("adk-bi-none-{}", uuid::Uuid::new_v4()));
        std::mem::forget(force_for_test(Admission::new(None, Some(stop))));
    };
    open();
    let mut runtime = tokio::runtime::Builder::new_current_thread();
    runtime.enable_all().on_thread_start(open).build().unwrap()
}

/// The busy Claude screen with an empty composer, as Herdr reads the pane.
fn busy_screen() -> Value {
    let text = format!("{BUSY_SCREEN}{BORDER}\n❯\u{00a0}\n{BORDER}\n{FOOTER}");
    json!({"type": "pane_read", "read": {
        "pane_id": PANE, "workspace_id": "w1", "tab_id": "w1:1", "source": "recent_unwrapped",
        "format": "text", "text": text, "revision": 3, "truncated": false
    }})
}

/// The channel's sessions row holding a Bound Claude execution running on the rig's pane.
async fn bind_row(rig: &HerdrRig, pool: &sqlx::PgPool, token: &str, channel: u64, logical: &str) {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    rig.run_provider(&rig.context(&nonce), false);
    let record = bound(rig, token, channel, logical, nonce);
    let build = crate::services::discord::adk_session::build_namespaced_session_key;
    let key = build(token, &ProviderKind::Claude, logical);
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
    )
    .bind(&key)
    .bind(token)
    .bind(channel.to_string())
    .bind(serde_json::to_value(record).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

/// A Bound Claude execution `nonce` on the rig's pane, owned by the channel's `logical` session.
fn bound(
    rig: &HerdrRig,
    token: &str,
    channel: u64,
    logical: &str,
    nonce: String,
) -> HostedExecution {
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: token.into(),
        channel_id: channel.to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.clone(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(&nonce)),
        source_ref: SourceRef {
            runtime_root: owner.runtime_root.clone(),
            channel: owner.channel_id.clone(),
            provider: "claude".into(),
            logical_key: logical.into(),
            execution_nonce: nonce,
            initial_source: None,
            baseline_event_seq: None,
        },
        owner,
    }
}

/// A Herdr-configured channel whose Bound execution runs on `rig`, with the busy transcript and
/// scripted tmux of an inject pane under the same session name.
async fn herdr_channel(
    rig: &HerdrRig,
    pool: &sqlx::PgPool,
    token: &str,
    channel: u64,
) -> InjectPane {
    let pane = InjectPane::new(channel, "external");
    let logical = pane.session();
    let marker = crate::services::tmux_common::session_temp_path(&logical, "host_kind");
    std::fs::write(marker, "herdr").unwrap();
    rig.answer("pane.read", busy_screen());
    bind_row(rig, pool, token, channel, &logical).await;
    pane
}

/// What Claude records in the channel transcript once a pasted frame's Enter reaches the pane.
#[derive(Clone, Copy, Debug)]
enum Logged {
    Exact,
    /// Another body under the frame's header, as a submit mixed with other keys.
    OtherBody,
}

/// Claude logs each pasted frame as `logged` when its Enter arrives, as it records a queued prompt.
fn claude_logs(rig: &HerdrRig, pane: &InjectPane, logged: Logged) {
    let transcript = pane.path("transcript.jsonl");
    let mut pasted = String::new();
    rig.on_send(move |params| {
        if let Some(text) = params["text"].as_str() {
            let text = text.trim_start_matches("\u{1b}[200~");
            pasted = text.trim_end_matches("\u{1b}[201~").to_string();
            return;
        }
        if params["keys"] != json!(["enter"]) {
            return;
        }
        let content = match logged {
            Logged::Exact => pasted.clone(),
            Logged::OtherBody => format!("{pasted}\nand more"),
        };
        let record = json!({"type": "queue-operation", "operation": "enqueue",
            "content": content, "sessionId": "5340", "timestamp": "2026-10-15T00:00:00.000Z"});
        let mut file = std::fs::OpenOptions::new().append(true).open(&transcript);
        writeln!(file.as_mut().unwrap(), "{record}").unwrap();
    });
}

fn request(channel: u64) -> HumanInputRequest {
    HumanInputRequest {
        channel_id: ChannelId::new(channel),
        provider: ProviderKind::Claude,
        text: "status?".to_string(),
        author_id: 200,
        source: "imessage".to_string(),
        metadata: None,
        channel_name_hint: None,
    }
}

/// A busy Herdr turn takes the input as one gated paste and Enter on its pane and Claude logs that
/// frame; no tmux command runs, and a channel without Herdr still pastes through its tmux session.
#[test]
fn herdr_busy_input_reaches_the_bound_pane_once_and_never_tmux_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (herdr, plain) = (5_340_811, 5_340_812);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(herdr, NODE)]);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    runtime().block_on(async {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let shared = register_inject_runtime(&registry, &[herdr, plain], Some(pool.clone())).await;
        let pane = herdr_channel(&rig, &pool, &shared.token_hash, herdr).await;
        claude_logs(&rig, &pane, Logged::Exact);
        let delivered = deliver_human_input(&registry, request(herdr)).await;
        assert!(
            matches!(delivered, Ok(HumanInputDelivery::Injected { .. })),
            "{delivered:?}"
        );
        let sends = rig.sends();
        assert_eq!(sends.len(), 2, "{sends:?}");
        let pasted = sends[0]["text"].as_str().unwrap();
        assert!(
            pasted.starts_with("\u{1b}[200~[📱 imessage · 200 · "),
            "{pasted:?}"
        );
        assert!(pasted.ends_with("]\nstatus?\u{1b}[201~"), "{pasted:?}");
        assert_eq!(sends[1]["keys"], json!(["enter"]));
        assert_eq!(pane.tmux_calls(), 0);
        assert!(queue_texts(&shared, herdr).await.is_empty());

        let tmux = InjectPane::new(plain, "external");
        let delivered = deliver_human_input(&registry, request(plain)).await;
        assert!(
            matches!(delivered, Ok(HumanInputDelivery::Injected { .. })),
            "{delivered:?}"
        );
        assert_eq!(tmux.keys(), ["paste-buffer", "send-keys"]);
        assert_eq!(rig.sends().len(), 2);
    });
}

/// The server's ACK of the paste and Enter without the exact frame in the transcript stays
/// unconfirmed under tmux's reason names; the input is neither queued nor sent again.
#[test]
fn herdr_ack_without_the_exact_receipt_stays_unconfirmed_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (unlogged, mixed) = (5_340_813, 5_340_814);
    let channels = [(unlogged, NODE), (mixed, NODE)];
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &channels);
    runtime().block_on(async {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let ids = [unlogged, mixed];
        let shared = register_inject_runtime(&registry, &ids, Some(pool.clone())).await;
        for (channel, logged, reason) in [
            (unlogged, None, "not_observed"),
            (mixed, Some(Logged::OtherBody), "contaminated_submit"),
        ] {
            let rig = HerdrRig::start();
            let _registry = rig.registry_on_this_thread();
            let pane = herdr_channel(&rig, &pool, &shared.token_hash, channel).await;
            if let Some(logged) = logged {
                claude_logs(&rig, &pane, logged);
            }
            let delivered = deliver_human_input(&registry, request(channel)).await;
            match delivered {
                Ok(HumanInputDelivery::Unconfirmed { detail, .. }) => assert_eq!(detail, reason),
                other => panic!("{logged:?}: {other:?}"),
            }
            assert_eq!(rig.sends().len(), 2, "{:?}", rig.sends());
            assert_eq!(pane.tmux_calls(), 0);
            assert!(queue_texts(&shared, channel).await.is_empty());
        }
    });
}

/// A pane the gate refuses before the paste, or a Herdr server that cannot be reached, sends
/// nothing and the input waits at the queue front.
#[test]
fn herdr_refusal_or_unreachable_server_hands_the_input_back_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (replaced, gone) = (5_340_821, 5_340_822);
    let channels = [(replaced, NODE), (gone, NODE)];
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &channels);
    runtime().block_on(async {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let ids = [replaced, gone];
        let shared = register_inject_runtime(&registry, &ids, Some(pool.clone())).await;
        for channel in ids {
            let rig = HerdrRig::start();
            let _registry = rig.registry_on_this_thread();
            let pane = herdr_channel(&rig, &pool, &shared.token_hash, channel).await;
            let veto = if channel == replaced {
                let other = uuid::Uuid::new_v4().simple().to_string();
                rig.run_provider(&rig.context(&other), true);
                "herdr_refused"
            } else {
                drop(rig);
                "pane_unavailable"
            };
            let delivered = deliver_human_input(&registry, request(channel)).await;
            match delivered {
                Ok(HumanInputDelivery::Queued { inject_veto, .. }) => {
                    assert_eq!(inject_veto.as_deref(), Some(veto));
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(queue_texts(&shared, channel).await, ["status?"]);
            assert_eq!(pane.tmux_calls(), 0);
        }
    });
}

/// Queues `texts` on the channel as the route does while another path holds its transition.
async fn enqueue(registry: &HealthRegistry, shared: &SharedData, channel: u64, texts: &[&str]) {
    let lock = shared.session_transition_lock(ChannelId::new(channel));
    let _held = lock.lock_owned().await;
    crate::services::discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(async {
        for text in texts {
            let delivered = deliver_human_input(
                registry,
                HumanInputRequest {
                    text: text.to_string(),
                    ..request(channel)
                },
            )
            .await;
            assert!(
                matches!(delivered, Ok(HumanInputDelivery::Queued { .. })),
                "{delivered:?}"
            );
        }
    })
    .await;
}

/// The production queued-head offer over a REST handle that never answers, so no thread parent.
async fn try_busy(shared: &Arc<SharedData>, channel: u64) -> bool {
    let http = Arc::new(
        serenity::HttpBuilder::new("herdr-queued-token")
            .proxy("http://127.0.0.1:9")
            .ratelimiter_disabled(true)
            .build(),
    );
    let deps = router::IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared,
        token: "herdr-queued-token",
    };
    let gate = Some(router::queued_head_inject_covers as fn(ChannelId, Option<ChannelId>) -> bool);
    let channel = ChannelId::new(channel);
    let try_busy = crate::services::discord::queue_io::queued_head_inject::try_busy;
    try_busy(&deps, &ProviderKind::Claude, channel, gate, "herdr_test").await
}

/// Live intake passes over a Herdr channel, but its queued head reaches the Bound pane once and,
/// logged by Claude, only the head leaves the queue; a tmux channel types as before.
#[test]
fn queued_herdr_head_reaches_the_bound_pane_once_and_tmux_types_as_before_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (herdr, plain) = (5_340_841, 5_340_842);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(herdr, NODE)]);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    runtime().block_on(async {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let shared = register_inject_runtime(&registry, &[herdr, plain], Some(pool.clone())).await;
        let pane = herdr_channel(&rig, &pool, &shared.token_hash, herdr).await;
        claude_logs(&rig, &pane, Logged::Exact);
        let tmux = InjectPane::new(plain, "external");
        let _gates = [herdr, plain].map(router::busy_inject_support::open_gate);
        for (channel, live) in [(herdr, false), (plain, true)] {
            let id = ChannelId::new(channel);
            assert_eq!(router::live_intake_inject_covers(id, None), live);
            assert!(router::queued_head_inject_covers(id, None));
            test_hook::set_queued_head(channel, true);
            enqueue(&registry, &shared, channel, &["status?", "tail B"]).await;
        }
        assert!(rig.sends().is_empty());

        assert!(try_busy(&shared, herdr).await);
        let sends = rig.sends();
        assert_eq!(sends.len(), 2, "{sends:?}");
        let pasted = sends[0]["text"].as_str().unwrap();
        assert!(pasted.ends_with("]\nstatus?\u{1b}[201~"), "{pasted:?}");
        assert_eq!(sends[1]["keys"], json!(["enter"]));
        assert_eq!(pane.tmux_calls(), 0);
        assert_eq!(queue_texts(&shared, herdr).await, ["tail B"]);

        assert!(try_busy(&shared, plain).await);
        assert_eq!(tmux.keys(), ["paste-buffer", "send-keys"]);
        assert_eq!(queue_texts(&shared, plain).await, ["tail B"]);
        assert_eq!(rig.sends().len(), 2);
    });
}

/// A queued head claimed for one Herdr execution is not typed once the channel's Bound execution
/// is another by the paste; it goes back to the queue front untouched.
#[test]
fn queued_herdr_head_stays_queued_when_the_execution_changes_before_the_paste_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 5_340_843;
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(channel, NODE)]);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    runtime().block_on(async {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let shared = register_inject_runtime(&registry, &[channel], Some(pool.clone())).await;
        let pane = herdr_channel(&rig, &pool, &shared.token_hash, channel).await;
        let _gate = router::busy_inject_support::open_gate(channel);
        test_hook::set_queued_head(channel, true);
        enqueue(&registry, &shared, channel, &["status?", "tail B"]).await;

        let (reached, resume) = test_hook::park_queued(channel, "may_have_sent");
        let caller = tokio::spawn({
            let shared = shared.clone();
            async move { try_busy(&shared, channel).await }
        });
        let wait = std::time::Duration::from_secs(10);
        tokio::time::timeout(wait, reached.notified())
            .await
            .unwrap();
        // The old provider keeps running, so only the row tells the executions apart.
        let other = bound(
            &rig,
            &shared.token_hash,
            channel,
            &pane.session(),
            "f".repeat(32),
        );
        sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE channel_id = $2")
            .bind(serde_json::to_value(other).unwrap())
            .bind(channel.to_string())
            .execute(&pool)
            .await
            .unwrap();
        resume.notify_one();
        assert!(!caller.await.unwrap());
        assert!(rig.sends().is_empty(), "{:?}", rig.sends());
        assert_eq!(pane.tmux_calls(), 0);
        assert_eq!(queue_texts(&shared, channel).await, ["status?", "tail B"]);
    });
}

/// The channel's persisted queue, as a restarted reader loads it.
fn queue_file(shared: &SharedData, channel: u64) -> Value {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// A queued Herdr head whose ACKed paste and Enter Claude did not log exactly stays UNKNOWN at the
/// front: it is never typed again and no reader, restarted or not, reaches the tail behind it.
#[test]
fn queued_herdr_head_without_the_exact_receipt_stays_unknown_and_holds_the_tail_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (unlogged, mixed) = (5_340_844, 5_340_845);
    let channels = [(unlogged, NODE), (mixed, NODE)];
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &channels);
    runtime().block_on(async {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let registry = HealthRegistry::new();
        let ids = [unlogged, mixed];
        let shared = register_inject_runtime(&registry, &ids, Some(pool.clone())).await;
        for (channel, logged) in [(unlogged, None), (mixed, Some(Logged::OtherBody))] {
            let rig = HerdrRig::start();
            let _registry = rig.registry_on_this_thread();
            let pane = herdr_channel(&rig, &pool, &shared.token_hash, channel).await;
            if let Some(logged) = logged {
                claude_logs(&rig, &pane, logged);
            }
            let _gate = router::busy_inject_support::open_gate(channel);
            test_hook::set_queued_head(channel, true);
            enqueue(&registry, &shared, channel, &["status?", "tail B"]).await;

            assert!(try_busy(&shared, channel).await, "{logged:?}");
            assert_eq!(rig.sends().len(), 2, "{:?}", rig.sends());
            let disk = queue_file(&shared, channel);
            assert_eq!(disk["head_injection"]["phase"], "unknown", "{logged:?}");
            assert_eq!(disk["terminal_identities"], json!([]), "{logged:?}");
            assert_eq!(queue_texts(&shared, channel).await, ["status?", "tail B"]);

            try_busy(&shared, channel).await;
            assert_eq!(rig.sends().len(), 2, "UNKNOWN never types the head again");
            assert_eq!(queue_file(&shared, channel), disk, "{logged:?}");
            let persistence = crate::services::discord::queue_persistence_context(
                &shared,
                &ProviderKind::Claude,
                ChannelId::new(channel),
            );
            let restarted = ChannelMailboxRegistry::default().handle(ChannelId::new(channel));
            let take = restarted.take_next_soft(persistence).await;
            assert!(take.intervention.is_none(), "{logged:?} reached the tail");
            assert_eq!(queue_texts(&shared, channel).await, ["status?", "tail B"]);
            assert_eq!(pane.tmux_calls(), 0);
        }
    });
}
