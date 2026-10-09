//! The admitted-kind restart terminal: which rows it takes, what each kind shows, and that each
//! refused or unconfirmed step keeps the row.
use super::*;
use crate::services::discord::InflightRestartMode;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;

const CHANNEL: u64 = 1_479_671_301_387_170_000;
const NONCE: &str = "nonce-restart-kind";

/// A change applied to a row.
type Edit = fn(&mut inflight::InflightTurnState);

/// A prior process's admitted Aborted on a Herdr pane, saved under a fresh test root.
struct Fixture {
    root: tempfile::TempDir,
    _env: crate::config::test_env::TestEnvVarGuard,
    provider: ProviderKind,
    row: inflight::InflightTurnState,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = ProviderKind::Claude;
        let name = provider.build_tmux_session_name(&format!("restart-kind-{tag}"));
        let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        let mut row = inflight::InflightTurnState::new(
            provider.clone(),
            CHANNEL,
            None,
            1,
            2,
            3,
            "restart kind".to_string(),
            None,
            Some(name),
            None,
            None,
            0,
        );
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        row.born_generation = 0;
        row.turn_nonce = Some(NONCE.to_string());
        row.tui_terminal_kind = Some(NativeTerminalKind::Aborted);
        row.full_response = "stored answer".to_string();
        Self {
            root,
            _env: env,
            provider,
            row,
        }
    }

    /// Saves `row` and returns it as the loader reads it back.
    fn persist(&self, row: &inflight::InflightTurnState) -> inflight::InflightTurnState {
        inflight::save_inflight_state(row).unwrap();
        inflight::load_inflight_state(&self.provider, CHANNEL).unwrap()
    }

    fn path(&self) -> std::path::PathBuf {
        inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &self.provider,
            CHANNEL,
        )
    }

    /// Rewrites the durable row in place, keeping its save generation.
    fn overwrite(&self, row: &inflight::InflightTurnState) {
        std::fs::write(self.path(), serde_json::to_vec(row).unwrap()).unwrap();
    }

    fn durable(&self) -> Option<inflight::InflightTurnState> {
        inflight::load_inflight_state(&self.provider, CHANNEL)
    }

    fn terminal(&self, row: &inflight::InflightTurnState) -> AdmittedRestartTerminal {
        admitted(&self.provider, row).expect("an admitted restart terminal")
    }
}

fn dead_http() -> Arc<serenity::Http> {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    )
}

#[test]
fn only_a_prior_process_admitted_herdr_row_is_a_restart_terminal() {
    let fixture = Fixture::new("table");
    let provider = &fixture.provider;
    let base = fixture.row.clone();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(&generation, "7").unwrap();
    assert_eq!(
        crate::services::discord::runtime_store::process_generation(),
        7
    );
    let edit = |change: Edit| {
        let mut row = base.clone();
        change(&mut row);
        row
    };
    let table: [(&str, inflight::InflightTurnState, bool); 7] = [
        ("admitted", base.clone(), true),
        (
            "prior generation",
            edit(|row| row.born_generation = 6),
            true,
        ),
        (
            "current generation",
            edit(|row| row.born_generation = 7),
            false,
        ),
        (
            "unadmitted",
            edit(|row| row.tui_terminal_kind = None),
            false,
        ),
        ("no nonce", edit(|row| row.turn_nonce = None), false),
        (
            "empty nonce",
            edit(|row| row.turn_nonce = Some(String::new())),
            false,
        ),
        (
            "not Herdr",
            edit(|row| row.tmux_session_name = Some("plain".into())),
            false,
        ),
    ];
    for (case, row, expected) in table {
        assert_eq!(admitted(provider, &row).is_some(), expected, "{case}");
    }
    // Off control: production's hard-false settlement leaves every row to the existing path.
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let off = admitted(provider, &base).is_none();
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    assert!(off, "settlement off must not consume an admitted kind");
}

/// An Aborted shows only the stop notice; a Completed shows its stored body, never a later answer
/// in the transcript, and without one it shows nothing.
#[test]
fn each_kind_shows_only_its_own_terminal() {
    let fixture = Fixture::new("delivery");
    let provider = &fixture.provider;
    let transcript = fixture.root.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"successor answer\"}\n",
    )
    .unwrap();
    let mut row = fixture.row.clone();
    row.output_path = Some(transcript.display().to_string());
    let aborted = fixture.terminal(&row);
    assert_eq!(
        aborted.delivery(provider, &row).as_deref(),
        Some(ADMITTED_ABORT_NOTICE)
    );
    row.tui_terminal_kind = Some(NativeTerminalKind::Completed);
    let completed = fixture.terminal(&row);
    let shown = completed.delivery(provider, &row).unwrap();
    assert!(
        shown.contains("stored answer") && !shown.contains("successor"),
        "{shown}"
    );
    row.full_response.clear();
    assert_eq!(completed.delivery(provider, &row), None);
}

/// A refused delivery is not a settlement; the next boot's delivery settles the same row.
#[tokio::test(flavor = "current_thread")]
async fn an_undelivered_terminal_stays_until_a_later_boot_delivers_it() {
    let fixture = Fixture::new("retry");
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let row = fixture.persist(&fixture.row);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let outcome = fixture
        .terminal(&row)
        .settle(&dead_http(), &shared, &row)
        .await;
    assert_eq!(outcome, AdmittedRestart::Retained);
    let kept = fixture.durable().expect("the row stays");
    assert!(!kept.terminal_delivery_committed);
    assert_eq!(kept.tui_terminal_kind, Some(NativeTerminalKind::Aborted));
    assert_eq!(
        kept.recovery_relay_attempts,
        row.recovery_relay_attempts + 1
    );

    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture
        .terminal(&kept)
        .settle(&discord.http, &shared, &kept)
        .await;
    assert_eq!(outcome, AdmittedRestart::Settled);
    assert!(fixture.durable().is_none());
    assert!(
        discord
            .contents()
            .iter()
            .any(|c| c.contains(ADMITTED_ABORT_NOTICE))
    );
}

/// An un-anchored row's notice is sent new; the anchor it records is carried into the ack, so the
/// same run clears the row.
#[tokio::test(flavor = "current_thread")]
async fn a_send_new_terminal_acks_and_clears_in_one_run() {
    let fixture = Fixture::new("send-new");
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let mut row = fixture.row.clone();
    row.current_msg_id = 0;
    let row = fixture.persist(&row);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture
        .terminal(&row)
        .settle(&discord.http, &shared, &row)
        .await;
    assert_eq!(outcome, AdmittedRestart::Settled, "{:?}", discord.calls());
    assert!(fixture.durable().is_none());
    let posted = discord.calls().into_iter().any(|call| {
        call.route.starts_with("POST ")
            && call
                .route
                .contains(&format!("/channels/{CHANNEL}/messages"))
            && call
                .content
                .is_some_and(|c| c.contains(ADMITTED_ABORT_NOTICE))
    });
    assert!(posted, "{:?}", discord.calls());
}

/// O's marker is not this terminal: on an O-owned destination nothing is sent or acked.
#[tokio::test(flavor = "current_thread")]
async fn an_o_owned_destination_keeps_the_row() {
    let fixture = Fixture::new("o-owned");
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        CHANNEL,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    let row = fixture.persist(&fixture.row);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture
        .terminal(&row)
        .settle(&discord.http, &shared, &row)
        .await;
    assert_eq!(outcome, AdmittedRestart::Retained);
    let kept = fixture.durable().expect("the row stays");
    assert!(!kept.terminal_delivery_committed);
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
}

/// A planned restart owns its row; restart keeps it untouched and sends nothing.
#[tokio::test(flavor = "current_thread")]
async fn a_planned_restart_row_is_kept_untouched() {
    let fixture = Fixture::new("planned");
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let mut row = fixture.row.clone();
    row.restart_mode = Some(InflightRestartMode::DrainRestart);
    let row = fixture.persist(&row);
    let before = std::fs::read(fixture.path()).unwrap();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    let outcome = fixture
        .terminal(&row)
        .settle(&discord.http, &shared, &row)
        .await;
    assert_eq!(outcome, AdmittedRestart::Retained);
    assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
}

/// Cleanup re-reads the row under its lock: a changed nonce or a current-generation row is kept.
#[tokio::test(flavor = "current_thread")]
async fn cleanup_keeps_a_row_that_is_no_longer_the_delivered_episode() {
    let fixture = Fixture::new("strict");
    let mut committed = fixture.row.clone();
    committed.terminal_delivery_committed = true;
    let committed = fixture.persist(&committed);
    let provider = &fixture.provider;
    let changes: [(&str, Edit); 4] = [
        ("no nonce", |row| row.turn_nonce = None),
        ("empty nonce", |row| row.turn_nonce = Some(String::new())),
        ("other nonce", |row| row.turn_nonce = Some("other".into())),
        ("current generation", |row| row.born_generation = 9),
    ];
    for (case, change) in changes {
        let mut fresh = committed.clone();
        change(&mut fresh);
        fixture.overwrite(&fresh);
        let outcome = inflight::clear_admitted_restart_terminal(provider, &committed, NONCE, 9);
        assert_ne!(outcome, inflight::GuardedClearOutcome::Cleared, "{case}");
        assert!(fixture.durable().is_some(), "{case}");
    }
    // The settle path, too: a nonce dropped after the snapshot keeps the row.
    fixture.overwrite(&committed);
    let terminal = fixture.terminal(&committed);
    let mut dropped = committed.clone();
    dropped.turn_nonce = None;
    let path = fixture.path();
    test_hooks::set_before_cleanup(Box::new(move || {
        Box::pin(async move {
            std::fs::write(path, serde_json::to_vec(&dropped).unwrap()).unwrap();
        })
    }));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let outcome = terminal.settle(&dead_http(), &shared, &committed).await;
    assert_eq!(outcome, AdmittedRestart::Retained);
    assert!(
        fixture.durable().is_some(),
        "a nonce-less row is not this episode"
    );
    fixture.overwrite(&committed);
    let outcome = inflight::clear_admitted_restart_terminal(provider, &committed, NONCE, 9);
    assert_eq!(outcome, inflight::GuardedClearOutcome::Cleared);
}

/// An actor that arrives before cleanup is another turn's: it keeps its token and the row stays.
#[tokio::test(flavor = "current_thread")]
async fn an_actor_registered_before_cleanup_is_left_alone() {
    let fixture = Fixture::new("barrier");
    let mut committed = fixture.row.clone();
    committed.terminal_delivery_committed = true;
    let committed = fixture.persist(&committed);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let token = Arc::new(CancelToken::new());
    let (hook_shared, hook_token) = (shared.clone(), token.clone());
    test_hooks::set_before_cleanup(Box::new(move || {
        Box::pin(async move {
            hook_shared
                .mailbox(ChannelId::new(CHANNEL))
                .restore_active_turn(
                    hook_token,
                    serenity::UserId::new(7),
                    serenity::MessageId::new(CHANNEL + 1),
                )
                .await;
        })
    }));
    let terminal = fixture.terminal(&committed);
    let outcome = terminal.settle(&dead_http(), &shared, &committed).await;
    assert_eq!(outcome, AdmittedRestart::Retained);
    let owner = super::mailbox_snapshot(&shared, ChannelId::new(CHANNEL)).await;
    assert!(
        owner
            .cancel_token
            .is_some_and(|current| Arc::ptr_eq(&current, &token))
    );
    assert!(fixture.durable().is_some());
}
