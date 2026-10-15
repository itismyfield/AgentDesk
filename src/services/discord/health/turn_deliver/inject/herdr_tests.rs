//! Busy input on a Herdr-hosted Claude channel from the external deliver entry: the channel's
//! Bound execution on an in-process Herdr socket, a scripted tmux that must stay untouched, and PG.
#![cfg(unix)]

use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};

use super::super::super::inject_tests::{InjectPane, queue_texts, register_inject_runtime};
use super::super::super::{HumanInputDelivery, HumanInputRequest, deliver_human_input};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::discord::health::HealthRegistry;
use crate::services::herdr_admission::{Admission, force_for_test};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};

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
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: token.into(),
        channel_id: channel.to_string(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: "/adk/runtime".into(),
    };
    let record = HostedExecution {
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
    };
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

/// A busy Herdr turn takes the input as one gated paste and Enter on its pane; no tmux command
/// runs, and a channel without Herdr still pastes through its tmux session.
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
