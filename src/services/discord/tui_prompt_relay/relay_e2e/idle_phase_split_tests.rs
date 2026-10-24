//! Production idle workers must discover sources independently of Legacy delivery.
use super::super::source_tick::PHASE_PROBE;
use super::n1a_turn_mode::claude_idle_o_transition::fixture::*;
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::router::source_guard_support::{Lexed, forge, swap};
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingRecord, BindingTarget, ChannelBindingLog,
};
use crate::services::tui_o::writer::host::{self, HostParts, Readiness};
use crate::test_support::CommentForgery;

fn isolated(name: &str) -> bool {
    if std::env::var("ADK_IDLE_PHASE_CHILD").as_deref() == Ok(name) {
        return false;
    }
    let id = format!("services::discord::tui_prompt_relay::relay_e2e::idle_phase_split::{name}");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &id, "--nocapture"])
        .env("ADK_IDLE_PHASE_CHILD", name)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed; 0 failed"));
    true
}

async fn until(label: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{label} deadline: {error}"));
}

fn counts(tmux: &str) -> (usize, usize, bool) {
    PHASE_PROBE.lock().unwrap_or_else(|e| e.into_inner())[tmux]
}
fn probe(tmux: &str, disabled: bool) {
    PHASE_PROBE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(tmux.into(), (0, 0, disabled));
}

fn logged(channel: u64, path: &std::path::Path) -> bool {
    ChannelBindingLog::new(channel, ShadowProvider::Claude)
        .binding_events_since(channel, 0)
        .unwrap()
        .iter()
        .any(|e| {
            matches!(&e.record, BindingRecord::Bound { new: BindingTarget::Source(s), .. }
                | BindingRecord::Resolved { source: s, .. } if s.path == path)
        })
}

const BOOT: &str = include_str!("../../runtime_bootstrap.rs");
const RELAY: &str = include_str!("../../tui_prompt_relay.rs");
const BOOT_FN: &str = "pub(crate) async fn run_bot(";
const CONFIRM: &str = "turn_retirement::confirm_at_boot(";
const RELAY_START: &str =
    "super::tui_prompt_relay::spawn_tui_prompt_relay(shared.clone(), provider.clone());";
const IDLE_SPAWNS: [&str; 2] = [
    "spawn_claude_idle_transcript_relay(shared.clone());",
    "spawn_codex_idle_rollout_relay(shared.clone());",
];

/// Classification 2, comments excluded: boot confirmation precedes the relay start in `run_bot`
/// and each idle relay spawns once. A lexical tripwire: no reachability or `cfg` evaluation.
fn boot_relay_gaps(boot: &str, relay: &str) -> Vec<String> {
    let mut gaps = Vec::new();
    match Lexed::new(boot).block(BOOT_FN) {
        Some(body) => {
            let order = (body.find(CONFIRM), body.find(RELAY_START));
            if !matches!(order, (Some(confirm), Some(relay)) if confirm < relay) {
                gaps.push("boot confirmation must precede the relay start".to_string());
            }
        }
        None => gaps.push(format!("missing {BOOT_FN}")),
    }
    let relay = Lexed::new(relay);
    for spawn in IDLE_SPAWNS {
        let count = relay.count(spawn);
        if count != 1 {
            gaps.push(format!("{spawn} appears {count} times; want 1"));
        }
    }
    gaps
}

#[test]
fn bootstrap_keeps_confirmation_before_the_existing_relay() {
    assert_eq!(boot_relay_gaps(BOOT, RELAY), Vec::<String>::new());
}

#[test]
fn boot_relay_guard_rejects_commented_steps_and_counts_coded_spawns() {
    for forgery in CommentForgery::ALL {
        for needle in [BOOT_FN, CONFIRM, RELAY_START] {
            let forged = forge(BOOT, needle, forgery);
            assert!(
                !boot_relay_gaps(&forged, RELAY).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
        for spawn in IDLE_SPAWNS {
            let forged = forge(RELAY, spawn, forgery);
            assert!(!boot_relay_gaps(BOOT, &forged).is_empty());
        }
    }
    let late_confirm = swap(BOOT, CONFIRM, RELAY_START);
    assert!(!boot_relay_gaps(&late_confirm, RELAY).is_empty());
    // An extra relay start ahead of the confirmation counts; the same text as a comment does not.
    let statement = BOOT
        .find("let confirmed =")
        .expect("boot confirmation statement");
    for (early, gaps) in [
        (RELAY_START.to_string(), 1),
        (format!("// {RELAY_START}"), 0),
    ] {
        let forged = [
            &BOOT[..statement],
            early.as_str(),
            "\n    ",
            &BOOT[statement..],
        ]
        .concat();
        assert_eq!(boot_relay_gaps(&forged, RELAY).len(), gaps, "{early}");
    }
    let [claude, _] = IDLE_SPAWNS;
    let prose = RELAY.replacen(claude, &format!("{claude}\n        // {claude}"), 1);
    assert_eq!(boot_relay_gaps(BOOT, &prose), Vec::<String>::new());
    let twice = RELAY.replacen(claude, &format!("{claude}\n        {claude}"), 1);
    assert_eq!(boot_relay_gaps(BOOT, &twice).len(), 1);
}

#[test]
fn late_claude_source_posts_with_body_disabled_while_legacy_still_relays() {
    if isolated("late_claude_source_posts_with_body_disabled_while_legacy_still_relays") {
        return;
    }
    run_with_pending(
        async |mut f| {
            let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
            Arc::get_mut(&mut f.pane.shared).unwrap().health_registry = Arc::downgrade(&registry);
            assert!(!f.pane.path.exists());
            let _confirmed = TestConfirmation::new(f.pane.channel.get());
            let _selected = crate::services::tui_o::cutover::test_override::force_candidates(&[(
                f.pane.channel.get(),
                RuntimeHandoffKind::ClaudeTui,
            )]);
            probe(&f.pane.tmux, true);
            // This is runtime_bootstrap's production entry; no direct tick helper drives the scenario.
            super::super::spawn_tui_prompt_relay(f.pane.shared.clone(), ProviderKind::Claude);
            until("Claude tick", || {
                super::super::claude_idle_runtime::RELAY_TICKS.load(Ordering::Acquire) > 0
            })
            .await;
            assert!(!f.pane.path.exists());
            assert!(!logged(f.pane.channel.get(), &f.pane.path));
            std::fs::write(&f.pane.path, "{\"type\":\"mode\"}\n").unwrap();
            until("Claude source", || {
                dedupe::runtime_binding_for_tmux_session(&f.pane.tmux)
                    .is_some_and(|b| std::path::Path::new(&b.output_path) == f.pane.path)
                    && logged(f.pane.channel.get(), &f.pane.path)
            })
            .await;
            let baseline = std::fs::metadata(&f.pane.path).unwrap().len();
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&f.pane.tmux)
                    .unwrap()
                    .last_offset,
                baseline
            );
            // A discovered source supplies the existing host's initial boundary.
            f.initialize(baseline);
            f.gate.acquired();
            let ready = Arc::new(Readiness::default());
            let hosts = host::start(ShadowProvider::Claude, true, || HostParts {
                io: f.io.clone(),
                runtime_root: Some(f.root.clone()),
                gate: f.gate.clone(),
                readiness: ready.clone(),
            });
            append(
                &f.pane.path,
                &record(
                    &f.pane.a,
                    "late-body",
                    "2026-10-01T03:00:00Z",
                    "O_LATE_BODY",
                    false,
                ),
            );
            until("O POST", || f.posts() == ["O_LATE_BODY"]).await;
            assert!(ready.is_ready(f.pane.channel.get()));

            let legacy = Pane::new(&f.root, f.pane.channel.get() + 1);
            f.pane
                .shared
                .tmux_watchers
                .restore_owner_channel_for_tmux_session(&legacy.tmux, legacy.channel);
            let mock = discord_mock::DiscordMockState::new();
            mock.allow_channel(legacy.channel.get());
            mock.park_first_placeholder.store(false, Ordering::SeqCst);
            let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
            let _server = AbortOnDrop(Some(server));
            let ctx = discord_mock::serenity_context(proxy, gateway).await;
            registry
                .set_utility_bot_http_for_tests(
                    crate::services::discord::bot_role::UtilityBotRole::Notify,
                    ctx.http.clone(),
                )
                .await;
            assert!(f.pane.shared.http.cached_serenity_ctx.set(ctx).is_ok());
            registry
                .register(
                    ProviderKind::Claude.as_str().to_string(),
                    f.pane.shared.clone(),
                )
                .await;
            let owner_http = crate::services::discord::session_notice::provider_http(
                registry.as_ref(),
                &ProviderKind::Claude,
                legacy.channel,
            )
            .await
            .expect("Legacy fixture provider HTTP");
            assert!(Arc::ptr_eq(
                &owner_http,
                &f.pane.shared.serenity_http_or_token_fallback().unwrap(),
            ));
            probe(&legacy.tmux, false);
            append(
                &legacy.path,
                &format!(
                    "{}\n",
                    serde_json::json!({"type":"user", "uuid":"legacy-prompt", "sessionId":legacy.a, "message":{"role":"user", "content":"legacy external prompt"}})
                ),
            );
            append(
                &legacy.path,
                &record(
                    &legacy.a,
                    "legacy-body",
                    "2026-10-01T03:00:00Z",
                    "LEGACY_STILL_RELAYS",
                    false,
                ),
            );
            until("Legacy POST", || {
                mock.messages
                    .lock()
                    .unwrap()
                    .values()
                    .any(|(_, text)| text.contains("LEGACY_STILL_RELAYS"))
            })
            .await;
            let terminal = serde_json::json!({"type":"result", "result":"LEGACY_STILL_RELAYS", "session_id":legacy.a});
            append(&legacy.path, &format!("{terminal}\n"));
            until("Legacy tail", || {
                !super::super::claude_idle_tail_running(&legacy.tmux)
            })
            .await;
            let before = counts(&f.pane.tmux).0;
            until("source ticks", || {
                counts(&f.pane.tmux).0 >= before + 2 && counts(&legacy.tmux).0 >= 2
            })
            .await;
            assert_eq!(
                counts(&f.pane.tmux).1,
                0,
                "O never entered Legacy recovery/scan/cursor code"
            );
            assert!(counts(&legacy.tmux).1 > 0);
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(&f.pane.tmux)
                    .unwrap()
                    .last_offset,
                baseline
            );
            assert_eq!(
                super::super::claude_idle_runtime::RELAY_SPAWNS.load(Ordering::Acquire),
                1
            );
            assert_eq!(f.posts(), ["O_LATE_BODY"]);
            assert_eq!(
                mock.messages
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|(_, text)| text.contains("LEGACY_STILL_RELAYS"))
                    .count(),
                1
            );
            assert!(mock.unhandled.lock().unwrap().is_empty());
            for task in hosts {
                task.abort();
                let _ = task.await;
            }
        },
        true,
    );
}

#[test]
fn codex_reconciles_a_late_marker_while_legacy_recovery_is_busy() {
    if isolated("codex_reconciles_a_late_marker_while_legacy_recovery_is_busy") {
        return;
    }
    run(async |f| {
        let tmux = "phase-split-codex";
        let channel = ChannelId::new(f.pane.channel.get() + 2);
        let _confirmed = TestConfirmation::new(channel.get());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
        shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(tmux, channel);
        let rollout = f.root.join("late-rollout.jsonl");
        dedupe::register_tmux_runtime_binding(
            tmux,
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: f
                    .root
                    .join("not-written-rollout.jsonl")
                    .display()
                    .to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        dedupe::register_tmux_channel(tmux, channel.get());
        probe(tmux, true);
        super::super::spawn_tui_prompt_relay(shared.clone(), ProviderKind::Codex);
        until("Codex tick", || {
            super::super::codex_idle_rollout::RELAY_TICKS.load(Ordering::Acquire) > 0
        })
        .await;
        let _busy = crate::services::discord::live_bridge::try_recovery(
            &ProviderKind::Codex,
            channel.get(),
        )
        .unwrap();
        std::fs::write(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"phase-codex\"}}\n",
        )
        .unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            tmux,
            &rollout,
            Some("phase-codex"),
        )
        .unwrap();
        until("Codex source", || {
            dedupe::runtime_binding_for_tmux_session(tmux)
                .is_some_and(|b| std::path::Path::new(&b.output_path) == rollout)
        })
        .await;
        let before = counts(tmux).0;
        until("Codex ticks", || counts(tmux).0 >= before + 2).await;
        assert_eq!(counts(tmux).1, 0);
        assert_eq!(
            super::super::codex_idle_rollout::RELAY_SPAWNS.load(Ordering::Acquire),
            1
        );
    });
}
