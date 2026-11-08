//! The effort command through exact sessions lookup, the Input gate and a recording Herdr socket.
#![cfg(unix)]

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use poise::serenity_prelude::ChannelId;
use serde_json::{Value, json};

use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::claude_tui::{
    composer_lock,
    hook_bundle::statusline,
    host_input::{SpyGuard, SpyState},
};
use crate::services::discord::host_defer_gate::tests::map_channel;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use crate::test_support::wait::{WAIT_CAP, await_within, join_within, recv_within};

const CHANNEL: u64 = 7_940_001;
const NATIVE: &str = "79400000-0000-4000-8000-000000000001";
const READY: &str = "────────────────────────────────────────────────────────────\n❯\u{a0}\n────────────────────────────────────────────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
const OVERLAY: &str =
    "Effort\n  low    medium    high    xhigh    max\n←/→ to adjust · Enter to confirm";

fn screen(text: &str) -> Value {
    json!({"type":"pane_read", "read":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","source":"recent_unwrapped","format":"text","text":text,"revision":1,"truncated":false}})
}

struct Fixture {
    shared: Arc<SharedData>,
    record: std::path::PathBuf,
    logical: String,
    bound: HostedExecution,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
}

impl Fixture {
    async fn new(rig: &HerdrRig) -> Self {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        map_channel(&shared, ChannelId::new(CHANNEL), "herdr-effort").await;
        {
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&ChannelId::new(CHANNEL)).unwrap();
            session.session_id = Some(NATIVE.into());
            session.current_path = Some("/work".into());
        }
        shared.settings.write().await.agent = Some("settings-agent".into());
        let logical = ProviderKind::Claude.build_tmux_session_name("herdr-effort");
        std::fs::write(
            crate::services::tmux_common::session_temp_path(&logical, "host_kind"),
            "herdr",
        )
        .unwrap();
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        rig.run_provider(&rig.context(&nonce), false);
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: CHANNEL.to_string(),
            logical_key: logical.clone(),
            owner_node: NODE.into(),
            runtime_root: "/adk/runtime".into(),
        };
        let bound = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: nonce.clone(),
            expected: Some(rig.expected(&nonce)),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            source_ref: SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: CHANNEL.to_string(),
                provider: "claude".into(),
                logical_key: logical.clone(),
                execution_nonce: nonce,
                initial_source: None,
                baseline_event_seq: None,
            },
            owner,
        };
        sqlx::query("INSERT INTO agents (id, name) VALUES ('settings-agent','Settings'), ('other-agent','Other')").execute(&pool).await.unwrap();
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Claude,
            &logical,
        );
        sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,cwd,agent_id,claude_session_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,'/work','settings-agent',$4,$5)")
            .bind(key).bind(&shared.token_hash).bind(CHANNEL.to_string()).bind(NATIVE).bind(serde_json::to_value(&bound).unwrap()).execute(&pool).await.unwrap();
        rig.answer("pane.get", json!({"type":"pane_info","pane":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":1,"cwd":"/work","foreground_cwd":"/work"}}));
        rig.answer("pane.read", screen(READY));
        rig.answer_after_send("pane.read", screen(OVERLAY));
        let record = statusline::record_path(&statusline::record_dir(), NATIVE);
        let _ = std::fs::remove_file(&record);
        Self {
            shared,
            record,
            logical,
            bound,
            db,
        }
    }

    async fn command(&self) -> String {
        await_within(
            effort_save_notice(
                &self.shared,
                "test-token",
                ChannelId::new(CHANNEL),
                &ProviderKind::Claude,
                EffortLevel::High,
            ),
            WAIT_CAP,
        )
        .await
    }

    fn record_on_confirm(&self, rig: &HerdrRig, value: Value) {
        let path = self.record.clone();
        let mut enters = 0;
        rig.on_send(move |params| {
            if params["keys"] == json!(["enter"]) {
                enters += 1;
                if enters == 2 {
                    let mut value = value.clone();
                    if value["written_at"] == "fresh" {
                        value["written_at"] = json!(now());
                    }
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, value.to_string()).unwrap();
                }
            }
        });
    }

    async fn finish(self) {
        let _ = std::fs::remove_file(self.record);
        self.shared.pg_pool.as_ref().unwrap().close().await;
        self.db.drop().await;
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn runtime(rig: &HerdrRig, tmux_keys: Arc<AtomicUsize>) -> tokio::runtime::Runtime {
    let registry = rig.thread_installer(NODE, KEY, rig.socket(), SESSION);
    let install = move || {
        registry();
        std::mem::forget(crate::config::session_hosts::force_for_test(
            Some(NODE),
            &[(CHANNEL, NODE)],
        ));
        std::mem::forget(crate::services::herdr_admission::force_for_test(
            crate::services::herdr_admission::Admission::new(
                true,
                Some(std::env::temp_dir().join("adk-i7940-no-stop")),
            ),
        ));
        let count = tmux_keys.clone();
        std::mem::forget(SpyGuard::install(SpyState {
            on_send: Some(Box::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        }));
    };
    install();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
}

fn hosting() -> impl Drop {
    let mut config = crate::config::Config::default();
    config.providers.insert(
        "claude".into(),
        crate::config::ProviderConfig {
            tui_hosting: Some(true),
            ..Default::default()
        },
    );
    crate::services::provider_hosting::install_provider_hosting_config(&config);
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::services::provider_hosting::install_provider_hosting_config(
                &crate::config::Config::default(),
            );
        }
    }
    Restore
}

#[test]
fn herdr_effort_applies_and_reports_only_fresh_matching_level_pg() {
    let _hosting_lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _node = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(NODE),
    );
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &crate::config::runtime_root().unwrap().join("missing.yaml"),
    );
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let _hosting = hosting();
    let rig = HerdrRig::start();
    let keys = Arc::new(AtomicUsize::new(0));
    runtime(&rig, keys.clone()).block_on(async {
        let fx = Fixture::new(&rig).await;
        fx.record_on_confirm(
            &rig,
            json!({"session_id":NATIVE,"written_at":"fresh","effort":{"level":"high"}}),
        );
        let notice = fx.command().await;
        assert_eq!(
            keys.load(Ordering::SeqCst),
            0,
            "Herdr never sends tmux keys"
        );
        assert!(
            notice.contains("effort를 `high`로 적용했습니다"),
            "{notice}"
        );
        let sends = rig.sends();
        assert!(sends.iter().all(|p| p["pane_id"] == PANE));
        let navigation: Vec<&str> = sends[2..]
            .iter()
            .flat_map(|p| p["keys"].as_array().unwrap())
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(
            navigation,
            ["left", "left", "left", "left", "right", "right", "enter"]
        );
        assert_eq!(sends[0]["text"], "/effort");
        assert_eq!(sends[1]["keys"], json!(["enter"]));
        assert_eq!(sends.last().unwrap()["keys"], json!(["enter"]));
        assert!(sends.iter().any(|p| {
            p["keys"]
                .as_array()
                .is_some_and(|k| k.contains(&json!("left")))
        }));
        let settings = fx.shared.settings.read().await;
        assert!(settings.channel_relaunch_pending.is_empty());
        assert!(settings.channel_effort_levels.is_empty());
        drop(settings);
        assert!(
            !fx.shared
                .overrides
                .session_reset_pending
                .contains(&ChannelId::new(CHANNEL))
        );
        std::fs::remove_file(&fx.record).unwrap();
        rig.on_send(|_| {});
        rig.answer("pane.read", screen(READY));
        rig.answer_after_send("pane.read", screen(OVERLAY));
        let without_receipt = fx.command().await;
        assert!(
            !without_receipt.contains("적용했습니다"),
            "ACK is not application: {without_receipt}"
        );
        fx.finish().await;
    });
}

#[test]
fn herdr_effort_mismatch_or_stale_record_never_reports_success_pg() {
    let _hosting_lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _node = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(NODE),
    );
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &crate::config::runtime_root().unwrap().join("missing.yaml"),
    );
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let _hosting = hosting();
    for record in [
        Some(json!({"session_id":NATIVE,"written_at":"fresh","effort":{"level":"low"}})),
        Some(json!({"session_id":NATIVE,"written_at":0.0,"effort":{"level":"high"}})),
        Some(json!({"session_id":"other","written_at":"fresh","effort":{"level":"high"}})),
        Some(json!({"session_id":NATIVE,"written_at":"fresh"})),
        None,
    ] {
        let rig = HerdrRig::start();
        let keys = Arc::new(AtomicUsize::new(0));
        runtime(&rig, keys.clone()).block_on(async {
            let fx = Fixture::new(&rig).await;
            if let Some(value) = record {
                fx.record_on_confirm(&rig, value);
            }
            let notice = fx.command().await;
            assert!(!notice.contains("적용했습니다"), "{notice}");
            assert!(notice.contains("적용 미확인"), "{notice}");
            assert!(
                rig.sends().len() >= 3,
                "the command reached the pane: {notice}"
            );
            assert_eq!(keys.load(Ordering::SeqCst), 0);
            fx.finish().await;
        });
    }
}

#[test]
fn herdr_settings_reject_foreign_channel_cwd_or_agent_before_keys_pg() {
    let _hosting_lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _node = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(NODE),
    );
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &crate::config::runtime_root().unwrap().join("missing.yaml"),
    );
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let _hosting = hosting();
    for case in [
        "channel", "cwd", "agent", "pane-cwd", "pending", "gate", "modal",
    ] {
        let rig = HerdrRig::start();
        let keys = Arc::new(AtomicUsize::new(0));
        runtime(&rig,keys.clone()).block_on(async {
            let mut fx = Fixture::new(&rig).await;
            let pool = fx.shared.pg_pool.as_ref().unwrap();
            match case {
                "channel" => {fx.bound.owner.channel_id = (CHANNEL+1).to_string(); fx.bound.source_ref.channel = (CHANNEL+1).to_string();},
                "pending" => fx.bound.state = HostedState::Pending,
                "cwd" => {sqlx::query("UPDATE sessions SET cwd='/other'").execute(pool).await.unwrap();},
                "agent" => {sqlx::query("UPDATE sessions SET agent_id='other-agent'").execute(pool).await.unwrap();},
                "pane-cwd" => rig.answer("pane.get",json!({"type":"pane_info","pane":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","revision":1,"cwd":"/other","foreground_cwd":"/other"}})),
                "gate" => {rig.run_provider(&rig.context(&uuid::Uuid::new_v4().simple().to_string()),false);},
                "modal" => rig.answer("pane.read", screen(&format!("{READY}\nAllow / Deny\nEnter to confirm · Esc to cancel"))),
                _ => unreachable!(),
            }
            if matches!(case,"channel"|"pending") {sqlx::query("UPDATE sessions SET hosted_execution=$1").bind(serde_json::to_value(&fx.bound).unwrap()).execute(pool).await.unwrap();}
            let notice = fx.command().await;
            assert!(rig.sends().is_empty(),"{case}: {:?}",rig.sends());
            assert_eq!(keys.load(Ordering::SeqCst),0);
            assert!(!notice.contains("적용했습니다"),"{case}: {notice}");
            assert!(notice.to_ascii_lowercase().contains("pane") || notice.contains("cwd") || notice.contains("agent") || notice.contains("입력") || notice.contains("refused"),"{case}: {notice}");
            fx.finish().await;
        });
    }
}

#[test]
fn herdr_settings_rechecks_draft_under_composer_lock_pg() {
    let _hosting_lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _node = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(NODE),
    );
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &crate::config::runtime_root().unwrap().join("missing.yaml"),
    );
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let _hosting = hosting();
    let rig = HerdrRig::start();
    runtime(&rig, Arc::new(AtomicUsize::new(0))).block_on(async {
        let fx = Fixture::new(&rig).await;
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let logical = fx.logical.clone();
        let writer = std::thread::spawn(move || {
            composer_lock::with_composer_mutation_lock(&logical, || {
                held_tx.send(()).unwrap();
                recv_within(&release_rx, WAIT_CAP);
            })
        });
        recv_within(&held_rx, WAIT_CAP);
        let shared = fx.shared.clone();
        let command = tokio::spawn(async move {
            effort_save_notice(
                &shared,
                "test-token",
                ChannelId::new(CHANNEL),
                &ProviderKind::Claude,
                EffortLevel::High,
            )
            .await
        });
        await_within(
            async {
                while !rig.requests().iter().any(|r| r["method"] == "pane.read") {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        let draft = READY.replace("❯\u{a0}", "❯ preserve my draft");
        rig.answer("pane.read", screen(&draft));
        release_tx.send(()).unwrap();
        join_within(writer, WAIT_CAP);
        let notice = await_within(command, WAIT_CAP).await.unwrap();
        assert!(
            notice.contains("draft") || notice.contains("초안"),
            "{notice}"
        );
        assert!(rig.sends().is_empty(), "{:?}", rig.sends());
        fx.finish().await;
    });
}

#[test]
fn herdr_effort_waits_for_overlay_before_navigation_pg() {
    let _hosting_lock = crate::services::provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _node = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_INSTANCE_ID",
        std::ffi::OsStr::new(NODE),
    );
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &crate::config::runtime_root().unwrap().join("missing.yaml"),
    );
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let _hosting = hosting();
    for appears in [false, true] {
        let rig = HerdrRig::start();
        runtime(&rig,Arc::new(AtomicUsize::new(0))).block_on(async {
            let fx = Fixture::new(&rig).await;
            rig.answer_after_send("pane.read",screen(READY));
            let shared = fx.shared.clone();
            let command = tokio::spawn(async move {effort_save_notice(&shared,"test-token",ChannelId::new(CHANNEL),&ProviderKind::Claude,EffortLevel::High).await});
            await_within(async {while rig.sends().len()<2 && !command.is_finished() {tokio::task::yield_now().await;}},WAIT_CAP).await;
            assert_eq!(rig.sends().len(),2,"only /effort and Enter precede overlay");
            assert!(!command.is_finished(),"closed-only is not confirmation");
            if appears {
                fx.record_on_confirm(&rig,json!({"session_id":NATIVE,"written_at":"fresh","effort":{"level":"high"}}));
                // The opening Enter was already sent before this hook was installed.
                let path = fx.record.clone();
                rig.on_send(move |p| {if p["keys"] == json!(["enter"]) {std::fs::create_dir_all(path.parent().unwrap()).unwrap();std::fs::write(&path,json!({"session_id":NATIVE,"written_at":now(),"effort":{"level":"high"}}).to_string()).unwrap();}});
                rig.answer("pane.read",screen(OVERLAY));
            }
            let notice = await_within(command,WAIT_CAP).await.unwrap();
            if appears {assert!(notice.contains("적용했습니다"),"{notice}");} else {assert!(notice.contains("overlay"),"{notice}");assert_eq!(rig.sends().len(),2);}
            fx.finish().await;
        });
    }
}
