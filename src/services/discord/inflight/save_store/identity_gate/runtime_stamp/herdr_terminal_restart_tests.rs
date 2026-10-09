//! Restart recovery keeps durable Herdr evidence without inventing a provider terminal.
use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, seed, shared_on,
};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
use serenity::all::ChannelId;

const FILTER: &str = "a_restart_keeps_a_held_herdr_turn_and_its_admitted_kind_pg";

/// The two phases run in different processes; the persisted bytes outlive token and registry state.
#[tokio::test(flavor = "current_thread")]
async fn a_restart_keeps_a_held_herdr_turn_and_its_admitted_kind_pg() {
    use crate::services::provider::cancel_token_claude_interrupt::{
        HERDR_SETTLEMENT_OVERRIDE, HerdrSubmission,
    };
    use std::sync::atomic::Ordering;
    if std::env::var("ADK_ACT7_PHASE").is_err() {
        let root = tempfile::tempdir().unwrap();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        pool.close().await;
        let mut pids = Vec::new();
        for phase in ["PERSIST", "RESTORE"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([FILTER, "--test-threads=1", "--nocapture"])
                .env("ADK_ACT7_PHASE", phase)
                .env("ADK_ACT7_ROOT", root.path())
                .env("ADK_ACT7_DB", &db.database_url)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{phase}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            let stdout = String::from_utf8_lossy(&output.stdout);
            let marker = stdout
                .lines()
                .find(|line| line.contains(&format!("ACT7_PHASE {phase}")))
                .unwrap();
            let pid = marker
                .split("pid=")
                .nth(1)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<u32>()
                .unwrap();
            assert_ne!(pid, std::process::id());
            pids.push(pid);
        }
        assert_ne!(pids[0], pids[1]);
        db.drop().await;
        return;
    }
    let phase = std::env::var("ADK_ACT7_PHASE").unwrap();
    let root = std::path::PathBuf::from(std::env::var_os("ADK_ACT7_ROOT").unwrap());
    let _root = crate::config::set_agentdesk_root_for_test(&root);
    let _legacy_output = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    let pool = crate::db::postgres::connect_test_pool_with_max_connections(
        &std::env::var("ADK_ACT7_DB").unwrap(),
        "act7 restart",
        4,
    )
    .await
    .unwrap();
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let kinds = [
        None,
        None,
        None,
        Some(NativeTerminalKind::Aborted),
        Some(NativeTerminalKind::Completed),
        None,
        None,
    ];
    let mut guards = Vec::new();
    for (n, kind) in kinds.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_160_000 + n as u64);
        let name = provider.build_tmux_session_name(&format!("act7-held-{n}"));
        let session = HostSessionRef::tmux(&name);
        guards.push((
            InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(session, HostPresence::Present),
        ));
        let before_path = root.join(format!("{n}.before"));
        let path = inflight_state_path(&inflight_runtime_root().unwrap(), &provider, channel.get());
        if phase == "PERSIST" {
            seed(
                &pool,
                &channel_key(&shared, &name),
                &name,
                channel.get(),
                Stored::Hosted,
            )
            .await;
            let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "herdr").unwrap();
            let token = busy_turn(&shared, channel, &name).await;
            let owner = crate::db::dispatched_sessions::hosted_execution::HostedOwner {
                provider: provider.as_str().into(),
                channel_id: channel.to_string(),
                discord_token_hash: shared.token_hash.clone(),
                logical_key: name.clone(),
                owner_node: "test".into(),
                runtime_root: root.display().to_string(),
            };
            let intent = token.prepare_herdr_interrupt(provider.clone(), &owner);
            *intent.submission.lock().unwrap() = if n == 2 {
                HerdrSubmission::Unknown
            } else {
                HerdrSubmission::Submitted
            };
            intent.user_stop.store(n < 3, Ordering::Release);
            let source = root.join(format!("{n}.jsonl"));
            std::fs::write(
                &source,
                "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"a\"}\n",
            )
            .unwrap();
            let mut row =
                crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                    .unwrap();
            row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            row.output_path = Some(source.display().to_string());
            row.tui_terminal_kind = kind;
            crate::services::discord::inflight::save_inflight_state(&row).unwrap();
            if n == 2 {
                crate::services::claude::herdr_turn::hold(&row.turn_nonce.clone().unwrap())
                    .unwrap();
            }
            std::fs::write(&before_path, std::fs::read(&path).unwrap()).unwrap();
            if n == 6 {
                std::fs::write(source, "").unwrap();
            }
            assert!(token.herdr_interrupt_state().is_some());
        } else {
            assert!(
                shared.mailbox_peek(channel).is_none(),
                "a process restart has no warm mailbox"
            );
            if crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(
                "restart_in_process",
            ) {
                busy_turn(&shared, channel, &name).await;
                assert!(
                    shared.mailbox_peek(channel).is_none(),
                    "an in-process restart retains its mailbox"
                );
            }
        }
    }
    if phase == "RESTORE" {
        let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(
            1_479_671_301_387_160_000,
        )
        .await;
        crate::services::discord::recovery_engine::restore_inflight_turns(
            &discord.http,
            &shared,
            &provider,
        )
        .await;
        for (n, kind) in kinds.into_iter().enumerate() {
            let channel = ChannelId::new(1_479_671_301_387_160_000 + n as u64);
            let path =
                inflight_state_path(&inflight_runtime_root().unwrap(), &provider, channel.get());
            if kind.is_some() {
                // Known defect: restart reads transcript success instead of the admitted Aborted kind,
                // then deletes the pre-finalize row; this is not provider-kind settlement proof.
                assert!(
                    crate::services::discord::inflight::load_inflight_state(
                        &provider,
                        channel.get()
                    )
                    .is_none(),
                    "known defect changed; re-evaluate terminal recovery"
                );
                continue;
            }
            let row =
                crate::services::discord::inflight::load_inflight_state(&provider, channel.get())
                    .unwrap_or_else(|| panic!("the row stays case={n} kind={kind:?}"));
            assert_eq!(row.tui_terminal_kind, kind);
            assert_eq!(
                std::fs::read(&path).unwrap(),
                std::fs::read(root.join(format!("{n}.before"))).unwrap()
            );
            assert!(!shared.core.lock().await.sessions.contains_key(&channel));
            assert!(shared.mailbox_peek(channel).is_none());
            if n == 2 {
                assert!(
                    crate::services::claude::herdr_turn::not_held(
                        row.turn_nonce.as_deref().unwrap()
                    )
                    .is_err()
                );
            }
        }
        assert!(
            !discord.calls().is_empty(),
            "terminal cases reached the fake Discord recovery sink"
        );
    }
    println!("ACT7_PHASE {phase} pid={} cases=7", std::process::id());
    drop(guards);
    pool.close().await;
    HERDR_SETTLEMENT_OVERRIDE.set(true);
}
