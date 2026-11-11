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
/// Claude Code 2.1.319's effort selector as the pane showed it: a slider and hints share the rows.
const OVERLAY_2_1_319: &str = "   Effort
                             Faster                             Smarter
                             ──────────▲───────────────────────────────      Ultracode  off
                             low     medium     high     xhigh      max      Tab to toggle
                                           The default effort for this model
   ←/→ to adjust · Enter to confirm · s for this session only · Esc to cancel";

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
        let reads: Vec<Value> = rig
            .requests()
            .into_iter()
            .filter(|r| r["method"] == "pane.read")
            .collect();
        assert!(
            !reads.is_empty() && reads.iter().all(|r| r["params"]["strip_ansi"] == true),
            "readiness requires plain pane text: {reads:?}"
        );
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
        assert_eq!(
            settings.channel_effort_levels.get(&CHANNEL.to_string()).map(String::as_str),
            Some("high"),
            "the channel's settings name the level its live pane runs"
        );
        drop(settings);
        assert!(
            !fx.shared
                .overrides
                .session_reset_pending
                .contains(&ChannelId::new(CHANNEL))
        );
        rig.answer("pane.read", screen(READY));
        rig.answer_after_send("pane.read", screen("  Effort   low ─ medium ─ [high] ─ xhigh ─ max\n  ←/→ to adjust · Enter to confirm · Esc to cancel"));
        fx.record_on_confirm(&rig, json!({"session_id":NATIVE,"written_at":"fresh","effort":{"level":"high"}}));
        assert!(fx.command().await.contains("적용했습니다"), "decorated known layout");
        rig.answer("pane.read", screen(READY));
        rig.answer_after_send("pane.read", screen(OVERLAY_2_1_319));
        fx.record_on_confirm(&rig, json!({"session_id":NATIVE,"written_at":"fresh","effort":{"level":"high"}}));
        let notice = fx.command().await;
        assert!(notice.contains("적용했습니다"), "Claude Code 2.1.319 layout: {notice}");
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

impl Fixture {
    async fn fast(
        &self,
        rig: &HerdrRig,
        runtime: u64,
        parent: u64,
        before: Option<Value>,
        after: Option<Value>,
    ) -> String {
        let shared = self.shared.clone();
        self.run_fast(rig, before, after, async move {
            super::super::fast_mode::fast_save_notice(
                &shared,
                "test-token",
                ChannelId::new(runtime),
                &ProviderKind::Claude,
                async { ChannelId::new(parent) },
            )
            .await
        })
        .await
    }

    async fn run_fast(
        &self,
        rig: &HerdrRig,
        before: Option<Value>,
        after: Option<Value>,
        command: impl std::future::Future<Output = std::io::Result<String>> + Send + 'static,
    ) -> String {
        rig.answer_after_send("pane.read", screen(READY));
        let path = self.record.clone();
        rig.on_send(move |params| {
            if params["keys"] == json!(["enter"]) {
                if let Some(mut value) = after.clone() {
                    if value["written_at"] == "fresh" {
                        value["written_at"] = json!(now());
                    }
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, value.to_string()).unwrap();
                }
            }
        });
        self.run_with_before(rig, before, command).await
    }

    /// Runs `command`, writing `before` as the fresh record once it first reads the pane.
    async fn run_with_before(
        &self,
        rig: &HerdrRig,
        before: Option<Value>,
        command: impl std::future::Future<Output = std::io::Result<String>> + Send + 'static,
    ) -> String {
        let requests = rig.requests().len();
        let command = tokio::spawn(command);
        await_within(
            async {
                while !command.is_finished()
                    && !rig.requests()[requests..]
                        .iter()
                        .any(|r| r["method"] == "pane.read")
                {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        if let Some(mut value) = before {
            if value["written_at"] == "fresh" {
                value["written_at"] = json!(now());
            }
            std::fs::create_dir_all(self.record.parent().unwrap()).unwrap();
            std::fs::write(&self.record, value.to_string()).unwrap();
        }
        await_within(command, WAIT_CAP)
            .await
            .unwrap()
            .unwrap_or_else(|e| e.to_string())
    }

    async fn assert_fast_preserved(
        &self,
        rig: &HerdrRig,
        keys: &AtomicUsize,
        runtime: u64,
        saved: bool,
    ) {
        let settings = self.shared.settings.read().await;
        assert!(
            settings.channel_fast_mode_reset_pending.is_empty(),
            "no persisted pending reset"
        );
        assert!(settings.channel_relaunch_pending.is_empty());
        assert!(
            self.shared
                .overrides
                .fast_mode_session_reset_pending
                .is_empty()
        );
        assert!(
            self.shared.overrides.session_reset_pending.is_empty(),
            "no pending reset"
        );
        assert_eq!(
            settings.channel_fast_modes.get(&runtime.to_string()),
            Some(&saved),
            "the channel's settings name the mode its live pane runs"
        );
        assert!(
            runtime == CHANNEL
                || settings.channel_fast_modes.get(&CHANNEL.to_string()) == Some(&true),
            "a thread pane leaves its parent's mode"
        );
        assert_eq!(keys.load(Ordering::SeqCst), 0, "no tmux input");
        assert_eq!(
            self.shared.core.lock().await.sessions[&ChannelId::new(runtime)]
                .session_id
                .as_deref(),
            Some(NATIVE)
        );
        let bound: Value =
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                .bind(runtime.to_string())
                .fetch_one(self.shared.pg_pool.as_ref().unwrap())
                .await
                .unwrap();
        assert_eq!(
            bound,
            serde_json::to_value(&self.bound).unwrap(),
            "same execution/nonce/pane"
        );
        assert!(
            rig.requests().iter().all(|r| !r["method"]
                .as_str()
                .and_then(|method| method.rsplit('.').next())
                .is_some_and(|operation| matches!(operation, "close" | "create"))),
            "no close/create: {:?}",
            rig.requests()
        );
    }
}

fn with_fast_rig(run: impl FnOnce(&HerdrRig, &Arc<AtomicUsize>, &tokio::runtime::Runtime)) {
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
    let _hosting = hosting();
    let rig = HerdrRig::start();
    let keys = Arc::new(AtomicUsize::new(0));
    let rt = runtime(&rig, keys.clone());
    run(&rig, &keys, &rt);
}

#[test]
fn herdr_fast_toggles_native_once_without_reset_pg() {
    with_fast_rig(|rig, keys, rt| {
        rt.block_on(async {
            let fx = Fixture::new(rig).await;
            fx.shared
                .settings
                .write()
                .await
                .channel_fast_modes
                .insert(CHANNEL.to_string(), true);
            for enabled in [false, true] {
                rig.on_send(|_| {});
                let _ = std::fs::remove_file(&fx.record);
                let sent = rig.sends().len();
                let notice = fx
                    .fast(
                        rig,
                        CHANNEL,
                        CHANNEL,
                        Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":enabled})),
                        Some(
                            json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":!enabled}),
                        ),
                    )
                    .await;
                fx.assert_fast_preserved(rig, keys, CHANNEL, !enabled).await;
                assert!(
                    notice.contains(&format!("fast mode를 `{}`로 적용했습니다", !enabled)),
                    "{notice}"
                );
                let sends = rig.sends();
                assert_eq!(
                    sends.len() - sent,
                    2,
                    "one native /fast and Enter: {sends:?}"
                );
                assert_eq!(sends[sent]["text"], "/fast");
                assert_eq!(sends[sent + 1]["keys"], json!(["enter"]));
                assert!(sends[sent..].iter().all(|p| p["pane_id"] == PANE));
            }
            // Direct settings callers use the same runtime pane and also stop before reset storage.
            let shared = fx.shared.clone();
            let sent = rig.sends().len();
            let changed = fx
                .run_fast(
                    rig,
                    Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":false})),
                    Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":true})),
                    async move {
                        super::super::config::update_channel_fast_mode(
                            &shared,
                            "test-token",
                            ChannelId::new(CHANNEL),
                            &ProviderKind::Claude,
                            true,
                            ChannelId::new(CHANNEL),
                        )
                        .await
                        .map(|changed| changed.to_string())
                    },
                )
                .await;
            assert_eq!(changed, "true");
            assert_eq!(rig.sends().len() - sent, 2);
            fx.assert_fast_preserved(rig, keys, CHANNEL, true).await;
            // The initial fresh receipt is not a post-toggle confirmation, even when ACK succeeds.
            let sent = rig.sends().len();
            let notice = fx
                .fast(
                    rig,
                    CHANNEL,
                    CHANNEL,
                    Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":false})),
                    None,
                )
                .await;
            assert!(
                !notice.contains("적용했습니다"),
                "ACK is not application: {notice}"
            );
            assert_eq!(
                rig.sends().len() - sent,
                2,
                "no retry after unconfirmed ACK"
            );
            fx.assert_fast_preserved(rig, keys, CHANNEL, true).await;
            fx.finish().await;
        })
    });
}

#[test]
fn herdr_fast_mismatch_never_reports_success_or_retries_toggle_pg() {
    with_fast_rig(|rig, keys, rt| {
        rt.block_on(async {
            let fx = Fixture::new(rig).await;
            fx.shared
                .settings
                .write()
                .await
                .channel_fast_modes
                .insert(CHANNEL.to_string(), true);
            for after in [
                Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":false})),
                Some(json!({"session_id":NATIVE,"written_at":0.0,"fast_mode":true})),
                Some(json!({"session_id":"other","written_at":"fresh","fast_mode":true})),
                Some(json!({"session_id":NATIVE,"written_at":"fresh"})),
                None,
            ] {
                let _ = std::fs::remove_file(&fx.record);
                let sent = rig.sends().len();
                let notice = fx
                    .fast(
                        rig,
                        CHANNEL,
                        CHANNEL,
                        Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":false})),
                        after,
                    )
                    .await;
                assert!(notice.contains("적용 미확인"), "{notice}");
                assert!(!notice.contains("적용했습니다"), "{notice}");
                assert_eq!(
                    rig.sends().len() - sent,
                    2,
                    "exactly one toggle, no retries"
                );
                fx.assert_fast_preserved(rig, keys, CHANNEL, true).await;
            }
            for before in [
                Some(json!({"session_id":NATIVE,"written_at":0.0,"fast_mode":false})),
                Some(json!({"session_id":"other","written_at":"fresh","fast_mode":false})),
                Some(json!({"session_id":NATIVE,"written_at":"fresh"})),
                None,
            ] {
                let _ = std::fs::remove_file(&fx.record);
                let sent = rig.sends().len();
                let notice = fx.fast(rig, CHANNEL, CHANNEL, before, None).await;
                assert!(!notice.contains("적용했습니다"), "{notice}");
                assert_eq!(
                    rig.sends().len(),
                    sent,
                    "unknown current state never toggles"
                );
            }
            fx.finish().await;
        })
    });
}

/// Claude Code 2.1.319's `/fast` overlay, which applies only on Space and then Enter.
fn fast_overlay(state: &str) -> Value {
    screen(&format!(
        "↯ Fast mode\nHigh-speed mode for Opus 5.5. Draws from usage credits at a higher rate. Separate rate limits apply.\n  Fast mode  {state}  $8/$40 per Mtok\nLearn more: https://code.claude.com/docs/en/fast-mode\nSpace to toggle · Enter to confirm · Esc to cancel"
    ))
}

#[test]
fn herdr_fast_overlay_toggles_only_from_its_shown_state_and_closes_on_failure_pg() {
    with_fast_rig(|rig, keys, rt| {
        rt.block_on(async {
            let fx = Fixture::new(rig).await;
            for (shown, confirms, expected) in [
                ("OFF", true, &["/fast", "enter", " ", "enter"][..]),
                ("OFF", false, &["/fast", "enter", " ", "enter", "esc"][..]),
                ("ON", false, &["/fast", "enter", "esc"][..]),
            ] {
                let _ = std::fs::remove_file(&fx.record);
                fx.shared
                    .settings
                    .write()
                    .await
                    .channel_fast_modes
                    .insert(CHANNEL.to_string(), false);
                rig.answer("pane.read", screen(READY));
                rig.answer_after_send("pane.read", fast_overlay(shown));
                rig.answer_after_key("esc", "pane.read", screen(READY));
                let (path, mut toggled) = (fx.record.clone(), false);
                // Only an Enter after Space applies; the opening Enter just shows the overlay.
                rig.on_send(move |p| {
                    toggled |= p["text"] == " ";
                    if confirms && toggled && p["keys"] == json!(["enter"]) {
                        let fresh =
                            json!({"session_id":NATIVE,"written_at":now(),"fast_mode":true});
                        std::fs::write(&path, fresh.to_string()).unwrap();
                    }
                });
                let (sent, shared) = (rig.sends().len(), fx.shared.clone());
                let before = json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":false});
                let notice = fx
                    .run_with_before(rig, Some(before), async move {
                        let parent = async { ChannelId::new(CHANNEL) };
                        super::super::fast_mode::fast_save_notice(
                            &shared,
                            "test-token",
                            ChannelId::new(CHANNEL),
                            &ProviderKind::Claude,
                            parent,
                        )
                        .await
                    })
                    .await;
                let sends: Vec<String> = rig.sends()[sent..]
                    .iter()
                    .map(|p| {
                        p["text"]
                            .as_str()
                            .unwrap_or_else(|| p["keys"][0].as_str().unwrap())
                            .to_string()
                    })
                    .collect();
                assert_eq!(sends, expected, "{shown}: {notice}");
                assert_eq!(
                    notice.contains("fast mode를 `true`로 적용했습니다"),
                    confirms,
                    "{shown}: {notice}"
                );
                fx.assert_fast_preserved(rig, keys, CHANNEL, confirms).await;
            }
            fx.finish().await;
        })
    });
}

#[test]
fn herdr_thread_fast_targets_runtime_pane_without_parent_reset_pg() {
    with_fast_rig(|rig, keys, rt| {
        rt.block_on(async {
        let mut fx = Fixture::new(rig).await;
        let thread = CHANNEL + 1;
        let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL,NODE),(thread,NODE)]);
        map_channel(&fx.shared, ChannelId::new(thread), "herdr-thread-fast").await;
        {
            let mut core = fx.shared.core.lock().await;
            let session = core.sessions.get_mut(&ChannelId::new(thread)).unwrap();
            session.session_id = Some(NATIVE.into());
            session.current_path = Some("/work".into());
        }
        fx.logical = ProviderKind::Claude.build_tmux_session_name("herdr-thread-fast");
        std::fs::write(crate::services::tmux_common::session_temp_path(&fx.logical,"host_kind"),"herdr").unwrap();
        fx.bound.owner.channel_id = thread.to_string();
        fx.bound.owner.logical_key = fx.logical.clone();
        fx.bound.source_ref.channel = thread.to_string();
        fx.bound.source_ref.logical_key = fx.logical.clone();
        let pool = fx.shared.pg_pool.as_ref().unwrap();
        sqlx::query("UPDATE sessions SET channel_id=$1, hosted_execution=$2, session_key=$3")
            .bind(thread.to_string()).bind(serde_json::to_value(&fx.bound).unwrap())
            .bind(crate::services::discord::adk_session::build_namespaced_session_key(&fx.shared.token_hash,&ProviderKind::Claude,&fx.logical)).execute(pool).await.unwrap();
        // A live parent pane is deliberately a different location with its own native session.
        let mut parent = fx.bound.clone();
        parent.owner.channel_id = CHANNEL.to_string();
        parent.owner.logical_key = ProviderKind::Claude.build_tmux_session_name("herdr-effort");
        parent.source_ref.channel = CHANNEL.to_string();
        parent.source_ref.logical_key = parent.owner.logical_key.clone();
        parent.location.as_mut().unwrap().pane_id = "parent-pane".into();
        sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,cwd,agent_id,claude_session_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,'/work','settings-agent',$4,$5)")
            .bind(crate::services::discord::adk_session::build_namespaced_session_key(&fx.shared.token_hash,&ProviderKind::Claude,&parent.owner.logical_key))
            .bind(&fx.shared.token_hash).bind(CHANNEL.to_string()).bind("parent-native").bind(serde_json::to_value(&parent).unwrap()).execute(pool).await.unwrap();
        fx.shared.settings.write().await.channel_fast_modes.insert(CHANNEL.to_string(), true);
        let notice = fx.fast(rig, thread, CHANNEL,
            Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":false})),
            Some(json!({"session_id":NATIVE,"written_at":"fresh","fast_mode":true}))).await;
        assert!(notice.contains("fast mode를 `true`로 적용했습니다"), "{notice}");
        assert_eq!(rig.sends().len(),2);
        assert!(rig.sends().iter().all(|p|p["pane_id"]==PANE), "parent pane receives zero keys");
        // Check the thread row separately; the parent row remains byte-identical.
        let parent_after: Value = sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
            .bind(CHANNEL.to_string()).fetch_one(pool).await.unwrap();
        assert_eq!(parent_after,serde_json::to_value(&parent).unwrap());
        fx.assert_fast_preserved(rig,keys,thread,true).await;
        fx.finish().await;
    })
    });
}

#[test]
fn herdr_first_input_and_effort_never_submit_mixed_content_pg() {
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
        use crate::services::claude::herdr_turn::{HerdrTurn, owned_prompt_for_tests};
        use crate::services::claude_tui::owner_input::{self, Lineage};
        use crate::services::session_host::herdr_socket_rig_tests::HerdrRequest;
        let fx = Fixture::new(&rig).await;
        // Normal submission must leave a ready composer; only /effort opens the overlay.
        rig.answer_after_send("pane.read", screen(READY));
        let submitted = Arc::new(std::sync::Mutex::new((String::new(), Vec::<String>::new())));
        let seen = submitted.clone();
        rig.on_send(move |params| {
            let mut state = seen.lock().unwrap();
            if let Some(text) = params["text"].as_str() {
                state.0.push_str(
                    text.trim_start_matches("\x1b[200~")
                        .trim_end_matches("\x1b[201~"),
                );
            }
            if params["keys"] == json!(["enter"]) {
                let text = std::mem::take(&mut state.0);
                state.1.push(text);
            }
        });
        let (captured_tx, captured_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (literal_tx, literal_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let captured = std::sync::atomic::AtomicBool::new(false);
        rig.on_reply(move |call| {
            if matches!(
                call.request,
                HerdrRequest::PaneRead {
                    strip_ansi: false,
                    ..
                }
            ) && !captured.swap(true, Ordering::SeqCst)
            {
                captured_tx.send(()).unwrap();
                recv_within(&release_rx.lock().unwrap(), WAIT_CAP);
            }
        });
        let done_rx = std::sync::Mutex::new(done_rx);
        let target = crate::services::session_host::herdr_endpoints().target(&fx.bound).unwrap();
        let _actions = crate::services::claude_tui::host_input::watch_actions(target, move |action| {
            if matches!(action, crate::services::claude_tui::input::TuiInputAction::Literal(text) if text == "/effort") {
                literal_tx.send(()).unwrap();
                recv_within(&done_rx.lock().unwrap(), WAIT_CAP);
            }
        });
        let record = fx.bound.clone();
        let pool = fx.shared.pg_pool.as_ref().unwrap().clone();
        let prompt = "normal first prompt";
        let normal = tokio::task::spawn_blocking(move || {
            let _launch = owner_input::install_launch(Box::new(move |_, _, _, _, send| {
                send(prompt.into()).map_err(|e| e.into_legacy())
            }));
            let loc = record.location.as_ref().unwrap();
            let turn = HerdrTurn {
                pool: &pool,
                owner: record.owner.clone(),
                channel_id: CHANNEL,
                endpoint: crate::services::herdr_launch::HerdrLaunchEndpoint {
                    execution_node: NODE.into(),
                    config_key: KEY.into(),
                    socket_addr: loc.socket_addr.clone(),
                    herdr_session: SESSION.into(),
                },
                row: None,
                prompt,
                working_dir: "/work",
                session_id: Some(NATIVE),
                force_fresh: false,
                system_prompt: None,
                model: None,
                hook_endpoint: None,
                cancel: None,
            };
            let result = owned_prompt_for_tests(
                &turn,
                &record,
                (
                    NATIVE,
                    crate::config::runtime_root()
                        .unwrap()
                        .join("not-created.jsonl"),
                ),
                Lineage::Existing,
            );
            done_tx.send(()).unwrap();
            result
        });
        recv_within(&captured_rx, WAIT_CAP);
        let normal_holds_composer =
            composer_lock::try_with_composer_mutation_lock(&fx.logical, || ()).is_none();
        let shared = fx.shared.clone();
        let command = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(effort_save_notice(
                &shared,
                "test-token",
                ChannelId::new(CHANNEL),
                &ProviderKind::Claude,
                EffortLevel::High,
            ))
        });
        await_within(
            async {
                while rig
                    .requests()
                    .iter()
                    .filter(|r| r["method"] == "pane.read")
                    .count()
                    < 2
                {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        // With the fence removed, force the reported stale-capture interleaving before releasing.
        if !normal_holds_composer {
            await_within(
                tokio::task::spawn_blocking(move || recv_within(&literal_rx, WAIT_CAP)),
                WAIT_CAP,
            )
            .await
            .unwrap();
        }
        release_tx.send(()).unwrap();
        await_within(normal, WAIT_CAP).await.unwrap().unwrap();
        let notice = await_within(command, WAIT_CAP).await.unwrap();
        assert!(notice.contains("overlay"), "{notice}");
        assert_eq!(
            submitted.lock().unwrap().1,
            [prompt, "/effort"],
            "first-input Paste/Enter and effort Literal/Enter must stay separate"
        );
        fx.finish().await;
    });
}

#[test]
fn herdr_effort_rejects_unknown_option_layout_before_navigation_pg() {
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
    for labels in [
        "low medium high max",
        "medium low high xhigh max",
        "low medium high xhigh max ultra",
        "low medium high xhigh max\nultra",
        "low medium ⚡ high xhigh max",
    ] {
        let rig = HerdrRig::start();
        runtime(&rig, Arc::new(AtomicUsize::new(0))).block_on(async {
            let fx = Fixture::new(&rig).await;
            rig.answer_after_send("pane.read", screen(&format!("Effort\n{labels}\n←/→ to adjust · Enter to confirm")));
            let notice = fx.command().await;
            let sends = rig.sends();
            assert_eq!(sends.len(), 3, "unknown options permit only opening /effort and Enter, then Escape: {labels}; {notice}");
            assert_eq!(sends[2]["keys"], json!(["esc"]), "the opened selector is closed: {labels}");
            assert!(!notice.contains("적용했습니다"), "{notice}");
            fx.finish().await;
        });
    }
}

/// What Herdr answers for a pane whose history outgrew the read: its newest 80 rows, `truncated`.
fn long_history(text: &str) -> Value {
    let mut read = screen(&format!(
        "{}{text}",
        "history\n".repeat(80 - text.lines().count())
    ));
    read["read"]["truncated"] = json!(true);
    read
}

#[test]
fn herdr_effort_on_long_history_names_read_failure_and_closes_selector_pg() {
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
        let mut refused = screen(READY);
        refused["read"]["source"] = json!("visible");
        rig.answer("pane.read", refused);
        let notice = fx.command().await;
        assert!(
            notice.contains("pane 읽기 실패") && notice.contains("read source"),
            "{notice}"
        );
        assert!(rig.sends().is_empty());
        rig.answer("pane.read", long_history(READY));
        rig.answer_after_send(
            "pane.read",
            long_history("Effort\nlow medium high max\n←/→ to adjust · Enter to confirm"),
        );
        rig.answer_after_key("esc", "pane.read", long_history(READY));
        let notice = fx.command().await;
        assert!(
            notice.contains("배치 미확인") && !notice.contains("선택창"),
            "{notice}"
        );
        assert_eq!(
            rig.sends().last().unwrap()["keys"],
            json!(["esc"]),
            "{notice}"
        );
        // The next first input reaches the composer instead of answering a selector.
        let target = crate::services::session_host::herdr_endpoints()
            .target(&fx.bound)
            .unwrap();
        use crate::services::claude_tui::input::TuiInputAction::{Enter, Literal};
        let plan = [Literal("next prompt".into()), Enter];
        let logical = fx.logical.clone();
        let source = crate::config::runtime_root()
            .unwrap()
            .join("next-input.jsonl");
        crate::services::claude_tui::readiness::tests::pre_source(&logical, &source);
        let next = tokio::task::spawn_blocking(move || {
            crate::services::claude::herdr_turn::composer::run_ready(
                &logical, &source, &target, &plan, None,
            )
        });
        let next = await_within(next, WAIT_CAP).await.unwrap();
        assert!(
            matches!(
                next,
                crate::services::claude_tui::host_input::InputRun::Applied
            ),
            "{next:?}"
        );
        fx.finish().await;
    });
}

#[test]
fn herdr_effort_partial_navigation_failure_sends_no_confirmation_pg() {
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
        rig.answer_sends_then_leave_unanswered(3);
        let notice = fx.command().await;
        let sends = rig.sends();
        assert_eq!(
            sends.len(),
            5,
            "stop after the second navigation send loses its ACK, then close the selector: {sends:?}"
        );
        assert_eq!(sends[4]["keys"], json!(["esc"]));
        assert_eq!(
            sends
                .iter()
                .filter(|p| p["keys"] == json!(["enter"]))
                .count(),
            1,
            "only the opening Enter may be sent"
        );
        assert!(!notice.contains("적용했습니다"), "{notice}");
        fx.finish().await;
    });
}
