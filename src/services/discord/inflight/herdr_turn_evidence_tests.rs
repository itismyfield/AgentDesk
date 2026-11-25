//! The executor's recorded start, its durable row and the watcher's fresh-idle signal after the
//! token is gone.
use super::herdr_turn_evidence::PersistedHerdrTurnStart;
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::turn_finalizer::CompletionSignal::{self, Done, PausedLive, Unknown};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;
use crate::services::session_host::{HostKind, HostedRuntimeLocator};
use serde_json::json;
use std::sync::Arc;

const LOGICAL: &str = "AgentDesk-herdr-evidence-unlisted";

fn env(root: &Path) -> crate::config::TestEnvVarGuard {
    crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root)
}

fn claude_turn(at: chrono::DateTime<chrono::Utc>) -> String {
    let at = at.to_rfc3339();
    [
        json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": "q"}}),
        json!({"type": "assistant", "timestamp": at, "message": {"role": "assistant",
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "답"}]}}),
        json!({"type": "system", "subtype": "turn_duration", "timestamp": at}),
    ]
    .map(|record| format!("{record}\n"))
    .concat()
}

fn codex_turn(turn: &str) -> String {
    [
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}}),
        json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn,
            "last_agent_message": "답"}}),
    ]
    .map(|record| format!("{record}\n"))
    .concat()
}

/// A saved row of `channel` on a Herdr host (or plain tmux) whose transcript holds `body`.
fn row(
    root: &Path,
    provider: &ProviderKind,
    channel: u64,
    body: &str,
    herdr: bool,
) -> (PathBuf, InflightTurnState) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join(format!("{channel}.jsonl"));
    std::fs::write(&path, body).unwrap();
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        1,
        77,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    row.hosted_execution_nonce = Some(format!("exec-{channel}"));
    row.host_locator = herdr.then(|| {
        host_locator::PersistedHostLocator::Known(HostedRuntimeLocator {
            execution_node: None,
            host_kind: HostKind::Herdr,
            host_session_id: "herdr".into(),
            pane: Some("p1".into()),
        })
    });
    save_inflight_state_in_root(root, &row).unwrap();
    (path, row)
}

fn signal(provider: &ProviderKind, channel: u64, whole_file: CompletionSignal) -> CompletionSignal {
    herdr_fresh_idle_signal(
        load_inflight_state(provider, channel).as_ref(),
        LOGICAL,
        whole_file,
    )
}

/// Each executor's recorded start reaches its row and survives a stale save; once the token is
/// gone the watcher holds the row on an earlier turn's terminal and ends it on its own terminal.
#[test]
fn same_herdr_row_restored_after_restart_finalizes_identically() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    install_turn_start_sink();
    let root = inflight_runtime_root().unwrap();
    let at = chrono::Utc::now();
    let cases = [
        (
            ProviderKind::Claude,
            claude_turn(at - chrono::Duration::seconds(60)),
            claude_turn(at + chrono::Duration::seconds(1)),
            Some(at),
        ),
        (
            ProviderKind::Codex,
            codex_turn("t0"),
            codex_turn("t1"),
            None,
        ),
    ];
    for (n, (provider, previous, own, submitted_at)) in cases.into_iter().enumerate() {
        let channel = 8_399_200 + n as u64;
        let (path, mut stale) = row(&root, &provider, channel, &previous, true);
        let token = Arc::new(CancelToken::new());
        stale.turn_nonce = token.turn_nonce().map(str::to_owned);
        save_inflight_state_in_root(&root, &stale).unwrap();
        let owner = HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: "hash".into(),
            channel_id: channel.to_string(),
            logical_key: format!("{LOGICAL}-{channel}"),
            owner_node: "node".into(),
            runtime_root: root.display().to_string(),
        };
        let nonce = stale.hosted_execution_nonce.clone().unwrap();
        let state = token.prepare_herdr_interrupt(provider.clone(), &owner);
        assert!(state.record_turn_start(HerdrTurnStart::at_end_of(&nonce, &path, submitted_at)));
        save_inflight_state_in_root(&root, &stale).unwrap();
        drop((state, token));
        assert_eq!(
            signal(&provider, channel, Done),
            PausedLive,
            "{provider:?}: an earlier turn's terminal"
        );
        std::fs::write(&path, previous + &own).unwrap();
        assert_eq!(
            signal(&provider, channel, PausedLive),
            Done,
            "{provider:?}: its own terminal"
        );
    }
}

/// Before input there is no start; a start of another execution or of the previous turn on the
/// same execution names no terminal of this row, and a successor's save drops it.
#[test]
fn terminal_before_input_or_from_previous_turn_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let root = inflight_runtime_root().unwrap();
    let provider = ProviderKind::Codex;
    let (path, mut row) = row(&root, &provider, 8_399_210, &codex_turn("t1"), true);
    assert_eq!(
        signal(&provider, row.channel_id, Done),
        PausedLive,
        "before input"
    );
    let start = |execution: &str| HerdrTurnStart {
        execution_nonce: execution.into(),
        source: path.clone(),
        file: None,
        offset: 0,
        submitted_at: None,
    };
    let own = row.hosted_execution_nonce.clone().unwrap();
    row.herdr_turn_start = Some(PersistedHerdrTurnStart {
        turn_nonce: row.turn_nonce.clone().unwrap(),
        start: start("exec-earlier"),
    });
    save_inflight_state_in_root(&root, &row).unwrap();
    assert_eq!(
        signal(&provider, row.channel_id, Done),
        PausedLive,
        "another execution's start"
    );
    row.herdr_turn_start = Some(PersistedHerdrTurnStart {
        turn_nonce: "previous-turn".into(),
        start: start(&own),
    });
    assert_eq!(
        herdr_fresh_idle_signal(Some(&row), LOGICAL, Done),
        PausedLive,
        "the previous turn's start"
    );
    save_inflight_state_in_root(&root, &row).unwrap();
    assert_eq!(
        load_inflight_state(&provider, row.channel_id)
            .unwrap()
            .herdr_turn_start,
        None
    );
}

/// A row on no Herdr host keeps every signal as before and needs no Herdr start.
#[test]
fn native_tmux_completion_with_verified_pin_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let _env = env(temp.path());
    let root = inflight_runtime_root().unwrap();
    let provider = ProviderKind::Claude;
    let (_, row) = row(&root, &provider, 8_399_220, "", false);
    for whole_file in [Done, PausedLive, Unknown] {
        assert_eq!(signal(&provider, row.channel_id, whole_file), whole_file);
        assert_eq!(
            herdr_fresh_idle_signal(None, LOGICAL, whole_file),
            whole_file
        );
    }
}
