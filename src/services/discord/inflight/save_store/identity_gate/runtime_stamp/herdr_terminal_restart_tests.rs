//! Restart recovery keeps durable Herdr evidence without inventing a provider terminal, and settles
//! an admitted terminal from its persisted kind.
use super::*;
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, seed, shared_on,
};
use crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE;
use crate::services::discord::recovery_engine::herdr_admitted_restart::test_hooks::STOP_AFTER_ACK;
use crate::services::discord::recovery_engine::o_cut_recorder::DiscordRecorder;
use crate::services::provider::cancel_token_claude_interrupt::{
    HERDR_SETTLEMENT_OVERRIDE, HerdrSubmission,
};
use crate::services::session_host::test_support::{InjectedLivenessGuard, InjectedPresenceGuard};
use crate::services::session_host::{HostLiveness, HostPresence, HostSessionRef};
use serenity::all::ChannelId;
use std::sync::Arc;
use std::sync::atomic::Ordering;

const FILTER: &str = "a_restart_keeps_a_held_herdr_turn_and_its_admitted_kind_pg";
const ACK_FILTER: &str = "an_admitted_ack_outlives_a_stop_before_cleanup_pg";
const BASE: u64 = 1_479_671_301_387_160_000;
const ACK_BASE: u64 = 1_479_671_301_387_180_000;

/// Case `n`'s transcript result, distinct per case so a delivery names its row.
fn body(n: usize) -> String {
    format!("restart-kind-body-{n}-end")
}

/// Case `n`'s stored answer, distinct from its transcript so a completion shows which it read.
fn stored(n: usize) -> String {
    format!("restart-kind-stored-{n}-end")
}

/// Runs this binary's test `filter` as `phase` in a new process; returns the pid it printed on the
/// line holding `marker`.
fn run_phase(filter: &str, phase: &str, marker: &str, env: &[(&str, &std::ffi::OsStr)]) -> u32 {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args([filter, "--test-threads=1", "--nocapture"]);
    command.env("ADK_ACT7_PHASE", phase);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{phase}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = stdout
        .lines()
        .find(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("{phase} printed no {marker}: {stdout}"));
    let pid = line
        .split("pid=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_ne!(pid, std::process::id());
    pid
}

/// One child phase's runtime: the shared root and database the parent made.
struct Phase {
    name: String,
    root: std::path::PathBuf,
    _root: crate::config::test_env::TestEnvVarGuard,
    _legacy_output: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    pool: sqlx::PgPool,
    shared: Arc<crate::services::discord::SharedData>,
    provider: ProviderKind,
}

impl Phase {
    async fn open() -> Option<Self> {
        let name = std::env::var("ADK_ACT7_PHASE").ok()?;
        let root = std::path::PathBuf::from(std::env::var_os("ADK_ACT7_ROOT").unwrap());
        let root_guard = crate::config::set_agentdesk_root_for_test(&root);
        let legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        let pool = crate::db::postgres::connect_test_pool_with_max_connections(
            &std::env::var("ADK_ACT7_DB").unwrap(),
            "act7 restart",
            4,
        )
        .await
        .unwrap();
        let shared = shared_on(&pool).await;
        Some(Self {
            name,
            root,
            _root: root_guard,
            _legacy_output: legacy,
            pool,
            shared,
            provider: ProviderKind::Claude,
        })
    }

    fn path(&self, channel: ChannelId) -> std::path::PathBuf {
        inflight_state_path(
            &inflight_runtime_root().unwrap(),
            &self.provider,
            channel.get(),
        )
    }

    fn durable(&self, channel: ChannelId) -> Option<InflightTurnState> {
        crate::services::discord::inflight::load_inflight_state(&self.provider, channel.get())
    }

    /// Seeds case `n` as a prior process left it: a busy Herdr turn whose row `edit` shapes.
    async fn persist(
        &self,
        n: usize,
        channel: ChannelId,
        name: &str,
        edit: impl FnOnce(&mut InflightTurnState),
    ) -> Arc<crate::services::provider::CancelToken> {
        let shared = &self.shared;
        let key = channel_key(shared, name);
        seed(&self.pool, &key, name, channel.get(), Stored::Hosted).await;
        let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let token = busy_turn(shared, channel, name).await;
        let source = self.root.join(format!("{}.jsonl", channel.get()));
        std::fs::write(
            &source,
            format!(
                "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"{}\"}}\n",
                body(n)
            ),
        )
        .unwrap();
        let mut row = self.durable(channel).unwrap();
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.output_path = Some(source.display().to_string());
        edit(&mut row);
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        token
    }

    /// Runs restart recovery against a fresh fake Discord.
    async fn restore(&self) -> DiscordRecorder {
        let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(BASE).await;
        crate::services::discord::recovery_engine::restore_inflight_turns(
            &discord.http,
            &self.shared,
            &self.provider,
        )
        .await;
        discord
    }

    async fn close(self, cases: usize) {
        println!(
            "ACT7_PHASE {} pid={} cases={cases}",
            self.name,
            std::process::id()
        );
        self.pool.close().await;
        HERDR_SETTLEMENT_OVERRIDE.set(true);
    }
}

/// What `discord` was asked to write to `channel`.
fn written(discord: &DiscordRecorder, channel: ChannelId) -> Vec<String> {
    let path = format!("/channels/{}/messages", channel.get());
    discord
        .calls()
        .into_iter()
        .filter(|call| {
            (call.route.starts_with("POST ") || call.route.starts_with("PATCH "))
                && call.route.contains(&path)
        })
        .map(|call| call.content.unwrap_or_default())
        .collect()
}

/// What restart must leave for each admitted case.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expect {
    /// Unadmitted: the held row stays byte-for-byte.
    Held,
    /// Shows `text` on its channel, never the transcript body, and its row is cleared.
    Settled(&'static str),
    /// Its delivery was already recorded: nothing written, row cleared.
    CleanupOnly,
    /// Unsettleable here: nothing written, row byte-for-byte.
    Kept,
}

/// The two phases run in different processes; the persisted bytes outlive token and registry state.
#[tokio::test(flavor = "current_thread")]
async fn a_restart_keeps_a_held_herdr_turn_and_its_admitted_kind_pg() {
    use NativeTerminalKind::{Aborted, Completed};
    if std::env::var("ADK_ACT7_PHASE").is_err() {
        let root = tempfile::tempdir().unwrap();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        pool.close().await;
        let env = [
            ("ADK_ACT7_ROOT", root.path().as_os_str()),
            ("ADK_ACT7_DB", std::ffi::OsStr::new(&db.database_url)),
        ];
        let persist = run_phase(FILTER, "PERSIST", "ACT7_PHASE PERSIST", &env);
        let restore = run_phase(FILTER, "RESTORE", "ACT7_PHASE RESTORE", &env);
        assert_ne!(persist, restore);
        db.drop().await;
        return;
    }
    let phase = Phase::open().await.unwrap();
    let stored_body = |n| -> &'static str { Box::leak(stored(n).into_boxed_str()) };
    // (kind, expectation): 3 Aborted, 4 Completed with a stored body, 7 a recorded delivery,
    // 8 a completion with no stored body, 9 a planned restart, 10 an un-anchored Aborted.
    let cases = [
        (None, Expect::Held),
        (None, Expect::Held),
        (None, Expect::Held),
        (Some(Aborted), Expect::Settled(ADMITTED_ABORT_NOTICE)),
        (Some(Completed), Expect::Settled(stored_body(4))),
        (None, Expect::Held),
        (None, Expect::Held),
        (Some(Completed), Expect::CleanupOnly),
        (Some(Completed), Expect::Kept),
        (Some(Aborted), Expect::Kept),
        (Some(Aborted), Expect::Settled(ADMITTED_ABORT_NOTICE)),
    ];
    let mut guards = Vec::new();
    for (n, (kind, _)) in cases.into_iter().enumerate() {
        let channel = ChannelId::new(BASE + n as u64);
        let name = phase
            .provider
            .build_tmux_session_name(&format!("act7-held-{n}"));
        let session = HostSessionRef::tmux(&name);
        guards.push((
            InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent),
            InjectedPresenceGuard::set(session, HostPresence::Present),
        ));
        let before_path = phase.root.join(format!("{n}.before"));
        if phase.name == "PERSIST" {
            let token = phase
                .persist(n, channel, &name, |row| {
                    row.tui_terminal_kind = kind;
                    if kind.is_some() && n != 8 {
                        row.full_response = stored(n);
                    }
                    // Case 7 stopped after its delivery was recorded, before its row was cleared.
                    row.terminal_delivery_committed = n == 7;
                    if n == 9 {
                        row.set_restart_mode(
                            crate::services::discord::InflightRestartMode::DrainRestart,
                        );
                    }
                    if n == 10 {
                        row.current_msg_id = 0;
                    }
                })
                .await;
            let owner = crate::db::dispatched_sessions::hosted_execution::HostedOwner {
                provider: phase.provider.as_str().into(),
                channel_id: channel.to_string(),
                discord_token_hash: phase.shared.token_hash.clone(),
                logical_key: name.clone(),
                owner_node: "test".into(),
                runtime_root: phase.root.display().to_string(),
            };
            let intent = token.prepare_herdr_interrupt(phase.provider.clone(), &owner);
            *intent.submission.lock().unwrap() = if n == 2 {
                HerdrSubmission::Unknown
            } else {
                HerdrSubmission::Submitted
            };
            intent.user_stop.store(n < 3, Ordering::Release);
            let row = phase.durable(channel).unwrap();
            if n == 2 {
                crate::services::claude::herdr_turn::hold(&row.turn_nonce.clone().unwrap())
                    .unwrap();
            }
            std::fs::write(&before_path, std::fs::read(phase.path(channel)).unwrap()).unwrap();
            if n == 6 {
                std::fs::write(phase.root.join(format!("{}.jsonl", channel.get())), "").unwrap();
            }
            assert!(token.herdr_interrupt_state().is_some());
        } else {
            assert!(
                phase.shared.mailbox_peek(channel).is_none(),
                "a process restart has no warm mailbox"
            );
            if crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant(
                "restart_in_process",
            ) {
                phase
                    .shared
                    .mailbox(channel)
                    .restore_active_turn(
                        Arc::new(crate::services::provider::CancelToken::new()),
                        serenity::all::UserId::new(7),
                        serenity::all::MessageId::new(channel.get() + 1),
                    )
                    .await;
                assert!(
                    phase.shared.mailbox_peek(channel).is_none(),
                    "an in-process restart retains its mailbox"
                );
            }
        }
    }
    if phase.name == "RESTORE" {
        let discord = phase.restore().await;
        for (n, (kind, expect)) in cases.into_iter().enumerate() {
            let channel = ChannelId::new(BASE + n as u64);
            let shown = written(&discord, channel);
            let before = std::fs::read(phase.root.join(format!("{n}.before"))).unwrap();
            let case = format!("case={n} kind={kind:?} expect={expect:?} shown={shown:?}");
            assert!(
                shown.iter().all(|text| !text.contains(&body(n))),
                "{case}: a transcript result is never the restart terminal"
            );
            match expect {
                Expect::Settled(text) => {
                    assert!(shown.iter().any(|c| c.contains(text)), "{case}");
                    assert!(phase.durable(channel).is_none(), "{case}");
                }
                Expect::CleanupOnly => {
                    assert!(shown.is_empty(), "{case}");
                    assert!(phase.durable(channel).is_none(), "{case}");
                }
                Expect::Kept | Expect::Held => {
                    assert!(shown.is_empty(), "{case}");
                    let row = phase
                        .durable(channel)
                        .unwrap_or_else(|| panic!("the row stays {case}"));
                    assert_eq!(row.tui_terminal_kind, kind);
                    assert_eq!(
                        std::fs::read(phase.path(channel)).unwrap(),
                        before,
                        "{case}"
                    );
                }
            }
            if expect != Expect::Held {
                continue;
            }
            assert!(
                !phase
                    .shared
                    .core
                    .lock()
                    .await
                    .sessions
                    .contains_key(&channel)
            );
            assert!(phase.shared.mailbox_peek(channel).is_none());
            if n == 2 {
                let row = phase.durable(channel).unwrap();
                assert!(
                    crate::services::claude::herdr_turn::not_held(
                        row.turn_nonce.as_deref().unwrap()
                    )
                    .is_err()
                );
            }
        }
    }
    drop(guards);
    phase.close(cases.len()).await;
}

/// A restart that stops right after its delivery ack leaves the ack durable; the next process
/// clears the row without writing to Discord again.
#[tokio::test(flavor = "current_thread")]
async fn an_admitted_ack_outlives_a_stop_before_cleanup_pg() {
    if std::env::var("ADK_ACT7_PHASE").is_err() {
        let root = tempfile::tempdir().unwrap();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        pool.close().await;
        let env = [
            ("ADK_ACT7_ROOT", root.path().as_os_str()),
            ("ADK_ACT7_DB", std::ffi::OsStr::new(&db.database_url)),
        ];
        let stop = [env[0], env[1], (STOP_AFTER_ACK, std::ffi::OsStr::new("1"))];
        let pids = [
            run_phase(ACK_FILTER, "PERSIST", "ACT7_PHASE PERSIST", &env),
            run_phase(
                ACK_FILTER,
                "RESTORE_AFTER_ACK_STOP",
                "RESTART_KIND_ACK_STOP",
                &stop,
            ),
            run_phase(
                ACK_FILTER,
                "RESTORE_CLEANUP",
                "ACT7_PHASE RESTORE_CLEANUP",
                &env,
            ),
        ];
        assert!(pids[0] != pids[1] && pids[1] != pids[2] && pids[0] != pids[2]);
        db.drop().await;
        return;
    }
    let phase = Phase::open().await.unwrap();
    let channel = ChannelId::new(ACK_BASE);
    let name = phase.provider.build_tmux_session_name("act7-ack-stop");
    let session = HostSessionRef::tmux(&name);
    let _guards = (
        InjectedLivenessGuard::set(session, HostLiveness::DeadOrAbsent),
        InjectedPresenceGuard::set(session, HostPresence::Present),
    );
    match phase.name.as_str() {
        "PERSIST" => {
            phase
                .persist(0, channel, &name, |row| {
                    row.tui_terminal_kind = Some(NativeTerminalKind::Aborted);
                })
                .await;
        }
        "RESTORE_AFTER_ACK_STOP" => {
            phase.restore().await;
            unreachable!("the stop hook ends this process after the ack");
        }
        _ => {
            let row = phase
                .durable(channel)
                .expect("the acked row outlived the stop");
            assert!(row.terminal_delivery_committed, "the ack is durable");
            assert_eq!(row.tui_terminal_kind, Some(NativeTerminalKind::Aborted));
            let discord = phase.restore().await;
            assert!(
                written(&discord, channel).is_empty(),
                "{:?}",
                discord.calls()
            );
            assert!(phase.durable(channel).is_none(), "cleanup clears the row");
        }
    }
    phase.close(1).await;
}
