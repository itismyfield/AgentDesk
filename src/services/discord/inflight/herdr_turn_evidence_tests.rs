//! A recorded Herdr start, its durable row and the watcher's fresh-idle signal after token loss.
use super::herdr_turn_evidence::PersistedHerdrTurnStart;
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::turn_finalizer::CompletionSignal::{Done, PausedLive, Unknown};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;
use crate::services::session_host::{HostKind, HostedRuntimeLocator};
use serde_json::json;
use std::sync::Arc;

const LOGICAL: &str = "AgentDesk-herdr-evidence-unlisted";

fn setup() -> (tempfile::TempDir, crate::config::TestEnvVarGuard, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    (temp, env, inflight_runtime_root().unwrap())
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
    let record =
        |kind: &str| json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}});
    format!("{}\n{}\n", record("task_started"), record("task_complete"))
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

/// Each executor's recorded start reaches its row and survives a stale save; once the token is
/// gone the watcher holds the row on an earlier turn's terminal and ends it on its own terminal.
#[test]
fn same_herdr_row_restored_after_restart_finalizes_identically() {
    let (_temp, _env, root) = setup();
    install_turn_start_sink();
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
        let restored = || load_inflight_state(&provider, channel);
        let held = herdr_fresh_idle_signal(restored().as_ref(), LOGICAL, Done);
        assert_eq!(held, PausedLive, "{provider:?}: an earlier turn's terminal");
        std::fs::write(&path, previous + &own).unwrap();
        let ended = herdr_fresh_idle_signal(restored().as_ref(), LOGICAL, PausedLive);
        assert_eq!(ended, Done, "{provider:?}: its own terminal");
    }
}

/// Before input there is no start; a start of another execution or of the previous turn on the
/// same execution names no terminal of this row, and a successor's save drops it.
#[test]
fn terminal_before_input_or_from_previous_turn_is_held() {
    let (_temp, _env, root) = setup();
    let provider = ProviderKind::Codex;
    let (path, mut row) = row(&root, &provider, 8_399_210, &codex_turn("t1"), true);
    let own = row.hosted_execution_nonce.clone().unwrap();
    let starts = [
        (None, "before input"),
        (
            Some((row.turn_nonce.clone().unwrap(), "exec-earlier".to_string())),
            "another execution",
        ),
        (Some(("previous-turn".into(), own)), "the previous turn"),
    ];
    for (start, case) in starts {
        row.herdr_turn_start = start.map(|(turn_nonce, execution_nonce)| PersistedHerdrTurnStart {
            turn_nonce,
            start: HerdrTurnStart {
                execution_nonce,
                source: path.clone(),
                file: None,
                offset: 0,
                submitted_at: None,
            },
        });
        assert_eq!(
            herdr_fresh_idle_signal(Some(&row), LOGICAL, Done),
            PausedLive,
            "{case}"
        );
    }
    save_inflight_state_in_root(&root, &row).unwrap();
    let saved = load_inflight_state(&provider, row.channel_id).unwrap();
    assert_eq!(saved.herdr_turn_start, None);
}

/// A row on no Herdr host keeps every signal as before and needs no Herdr start.
#[test]
fn native_tmux_completion_with_verified_pin_unchanged() {
    let (_temp, _env, root) = setup();
    let (_, row) = row(&root, &ProviderKind::Claude, 8_399_220, "", false);
    for whole_file in [Done, PausedLive, Unknown] {
        assert_eq!(
            herdr_fresh_idle_signal(Some(&row), LOGICAL, whole_file),
            whole_file
        );
    }
}
