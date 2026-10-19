//! Which rows boot restore leaves to the end pass, and that the end pass's clear removes only the
//! turn it read; the end-to-end scenarios run in `rig`.
use std::sync::Mutex;

use super::*;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;

#[path = "herdr_row_end_rig_tests.rs"]
mod rig;

type Hook = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Hooks a scenario places at a named point of the end task.
static HOOKS: Mutex<Vec<(&'static str, Hook)>> = Mutex::new(Vec::new());

/// Runs every hook registered for `point`, in order.
pub(super) async fn hook(point: &'static str) {
    let hooks: Vec<Hook> = HOOKS
        .lock()
        .unwrap()
        .iter()
        .filter(|(at, _)| *at == point)
        .map(|(_, hook)| hook.clone())
        .collect();
    for run in hooks {
        run().await;
    }
}

fn on(
    point: &'static str,
    hook: impl Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) {
    HOOKS.lock().unwrap().push((point, Arc::new(hook)));
}

/// `hook` at `point` until the guard drops, for a scenario outside this module.
pub(in crate::services::discord) fn on_while(
    point: &'static str,
    hook: impl Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'static,
) -> Hooked {
    on(point, hook);
    Hooked(point)
}

pub(in crate::services::discord) struct Hooked(&'static str);

impl Drop for Hooked {
    fn drop(&mut self) {
        let point = self.0;
        HOOKS.lock().unwrap().retain(|(at, _)| *at != point);
    }
}

const CHANNEL: u64 = 5_340_871_001;
const NONCE: &str = "turn-row-end";
const PRIOR: u64 = 4;
const CURRENT: u64 = 5;

/// A fresh runtime root whose process generation is `CURRENT`.
struct Root {
    _dir: tempfile::TempDir,
    _env: crate::config::test_env::TestEnvVarGuard,
}

fn root() -> Root {
    let dir = tempfile::tempdir().unwrap();
    let env = crate::config::set_agentdesk_root_for_test(dir.path());
    let generation = super::super::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(&generation, CURRENT.to_string()).unwrap();
    Root {
        _dir: dir,
        _env: env,
    }
}

/// A session name; `herdr` writes its Herdr host marker.
fn session(provider: &ProviderKind, tag: &str, herdr: bool) -> String {
    let name = provider.build_tmux_session_name(&format!("row-end-{tag}"));
    if herdr {
        let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
    }
    name
}

/// A path-less turn row the prior process left on `name`.
fn row(provider: &ProviderKind, name: &str) -> inflight::InflightTurnState {
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        CHANNEL,
        None,
        1,
        2,
        3,
        "restarted turn".to_string(),
        None,
        Some(name.to_string()),
        None,
        None,
        0,
    );
    row.runtime_kind = Some(match provider {
        ProviderKind::Codex => crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        _ => crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    });
    row.born_generation = PRIOR;
    row.turn_nonce = Some(NONCE.to_string());
    row
}

type Edit = fn(&mut inflight::InflightTurnState);
type Change = dyn Fn(&mut serde_json::Value);

/// With settlement off, as in production, boot restore keeps its wrapper path for every row but a
/// prior process's path-less Herdr turn with no kind, receipt or planned hand-off.
#[test]
fn boot_restore_leaves_only_a_restarted_herdr_row_to_the_end_pass() {
    let _root = root();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let claude = ProviderKind::Claude;
    let herdr = row(&claude, &session(&claude, "herdr", true));
    let edit = |change: Edit| {
        let mut row = herdr.clone();
        change(&mut row);
        row
    };
    let table: [(&str, inflight::InflightTurnState, bool); 11] = [
        ("restarted Herdr turn", herdr.clone(), true),
        (
            "drain restart",
            edit(|row| row.restart_mode = Some(InflightRestartMode::DrainRestart)),
            true,
        ),
        (
            "tmux turn",
            row(&claude, &session(&claude, "tmux", false)),
            false,
        ),
        (
            "current generation",
            edit(|row| row.born_generation = CURRENT),
            false,
        ),
        (
            "unstamped generation",
            edit(|row| row.born_generation = 0),
            true,
        ),
        (
            "admitted kind",
            edit(|row| row.tui_terminal_kind = Some(NativeTerminalKind::Completed)),
            false,
        ),
        (
            "output path",
            edit(|row| row.output_path = Some("/tmp/out.jsonl".into())),
            false,
        ),
        (
            "hot swap",
            edit(|row| row.restart_mode = Some(InflightRestartMode::HotSwapHandoff)),
            false,
        ),
        ("rebind origin", edit(|row| row.rebind_origin = true), false),
        ("no nonce", edit(|row| row.turn_nonce = None), false),
        (
            "empty nonce",
            edit(|row| row.turn_nonce = Some(String::new())),
            false,
        ),
    ];
    for (case, state, expected) in &table {
        assert_eq!(held_at_restore(&claude, state), *expected, "{case}");
    }
    // Codex rows are Herdr turns only while the Codex switch is on; a tmux row never is.
    let codex = ProviderKind::Codex;
    let codex_herdr = row(&codex, &session(&codex, "herdr", true));
    let codex_tmux = row(&codex, &session(&codex, "tmux", false));
    let off = held_at_restore(&codex, &codex_herdr);
    let _on = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let on = (
        held_at_restore(&codex, &codex_herdr),
        held_at_restore(&codex, &codex_tmux),
    );
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    assert!(!off, "Codex with its switch off");
    assert_eq!(
        on,
        (true, false),
        "Codex Herdr and tmux rows with the switch on"
    );
}

/// The clear removes only the turn the pass read, from a prior process, with no kind, receipt or
/// hot swap; a receipt written after the pass chose the row keeps it.
#[test]
fn the_exact_clear_removes_only_the_turn_the_pass_read() {
    let _root = root();
    let claude = ProviderKind::Claude;
    let base = row(&claude, &session(&claude, "clear", true));
    let path = || {
        inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &claude,
            CHANNEL,
        )
    };
    let saved = || {
        let _ = std::fs::remove_file(path());
        inflight::save_inflight_state(&base).unwrap();
        inflight::load_inflight_state(&claude, CHANNEL).unwrap()
    };
    let rewrite = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path()).unwrap()).unwrap();
        change(&mut json);
        std::fs::write(path(), serde_json::to_vec(&json).unwrap()).unwrap();
    };
    let kept: [(&str, &Change); 6] = [
        ("successor turn", &|row| {
            row["turn_nonce"] = "successor".into()
        }),
        ("later save", &|row| {
            row["save_generation"] = (row["save_generation"].as_u64().unwrap() + 1).into()
        }),
        ("receipt", &|row| {
            row["herdr_before_start_stop"] = serde_json::json!({"protocol": 1})
        }),
        ("admitted kind", &|row| {
            row["tui_terminal_kind"] = serde_json::json!(NativeTerminalKind::Completed)
        }),
        ("hot swap", &|row| {
            row["restart_mode"] = "hot_swap_handoff".into()
        }),
        ("this process", &|row| {
            row["born_generation"] = CURRENT.into()
        }),
    ];
    for (case, change) in kept {
        let read = saved();
        rewrite(change);
        let outcome = read.clear_restarted_herdr_row(&claude, CURRENT);
        assert_ne!(outcome, inflight::GuardedClearOutcome::Cleared, "{case}");
        assert!(path().exists(), "{case} keeps the row");
    }
    let read = saved();
    rewrite(&|row| row["restart_mode"] = "drain_restart".into());
    let mut drained = read.clone();
    drained.restart_mode = Some(InflightRestartMode::DrainRestart);
    assert_eq!(
        drained.clear_restarted_herdr_row(&claude, CURRENT),
        inflight::GuardedClearOutcome::Cleared,
        "a drain-restart mark is not a hand-off"
    );
    assert!(!path().exists());
}

/// A completed turn's body is its explicit result, else its streamed text in order with one break
/// at each tool use; an aborted or mixed turn has none, and an open one is not idle.
#[test]
fn the_turn_body_is_its_result_or_its_streamed_text() {
    use serde_json::json;
    use std::io::Write;
    let at = "2026-10-19T00:00:01Z";
    let input = chrono::DateTime::parse_from_rfc3339(at).unwrap().to_utc();
    let user = |text: &str| json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": text}});
    let text = |text: &str| json!({"type": "assistant", "timestamp": at, "message": {"content": [{"type": "text", "text": text}]}});
    let tool = json!({"type": "assistant", "timestamp": at, "message": {"content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]}});
    let output = json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}});
    let stop = json!({"type": "system", "subtype": "stop_hook_summary", "timestamp": at});
    let took =
        json!({"type": "system", "subtype": "turn_duration", "timestamp": at, "durationMs": 900});
    let result = json!({"type": "result", "timestamp": at, "subtype": "success", "result": "The whole answer."});
    let interrupt = json!({"type": "user", "timestamp": at, "message": {"role": "user",
        "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
    let table = [
        (
            "one reply",
            vec![user("q"), text("The answer."), stop.clone(), took.clone()],
            Some(Some("The answer.")),
        ),
        (
            "text between tool uses",
            vec![
                user("q"),
                text("Reading the files."),
                tool.clone(),
                output.clone(),
                text("Both match."),
                tool,
                output,
                text("Done."),
                stop.clone(),
                took.clone(),
            ],
            Some(Some("Reading the files.\n\nBoth match.\n\nDone.")),
        ),
        (
            "explicit result",
            vec![user("q"), text("Reading."), result],
            Some(Some("The whole answer.")),
        ),
        (
            "aborted",
            vec![user("q"), text("Reading."), interrupt],
            Some(None),
        ),
        (
            "previous tail in the same second",
            vec![
                text("previous turn"),
                user("q"),
                text("The answer."),
                stop,
                took,
            ],
            Some(None),
        ),
        ("still open", vec![user("q"), text("Reading.")], None),
    ];
    for (case, records, expected) in table {
        let mut file = tempfile::tempfile().unwrap();
        for record in &records {
            writeln!(file, "{record}").unwrap();
        }
        let len = file.metadata().unwrap().len();
        let read = crate::services::claude::herdr_turn::closed_turn(&mut file, Some(input), len);
        let body = read.map(turn_body);
        assert_eq!(body.as_ref().map(Option::as_deref), expected, "{case}");
    }
}

/// Once the writer is gone a final record without its newline counts only when it parses whole, in
/// either provider's source; only a source cut after its length was read stays unread.
#[test]
fn an_ended_sources_torn_tail_counts_only_when_it_parses_whole() {
    use crate::services::claude::herdr_turn::{EndedTurn, ended_turn};
    use serde_json::json;
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let input = chrono::Utc::now();
    let at = (input + chrono::Duration::seconds(1)).to_rfc3339();
    let user =
        json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": "q"}});
    let text = json!({"type": "assistant", "timestamp": at, "message": {"role": "assistant",
        "content": [{"type": "text", "text": "부분"}]}});
    let stop = json!({"type": "assistant", "timestamp": at, "message": {"role": "assistant",
        "stop_reason": "end_turn", "content": [{"type": "text", "text": "답"}]}});
    let took = json!({"type": "system", "subtype": "turn_duration", "timestamp": at}).to_string();
    let started = [
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t1"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
            "id": "msg_t1", "content": [{"type": "output_text", "text": "부분"}]}}),
    ];
    let complete = json!({"type": "event_msg", "payload": {"type": "task_complete",
        "turn_id": "t1", "last_agent_message": "부분"}})
    .to_string();
    let cases = [
        (
            "claude torn",
            vec![user.clone(), text.clone()],
            &took[..took.len() / 2],
        ),
        (
            "claude whole terminal",
            vec![user.clone(), text.clone(), stop.clone()],
            &took[..],
        ),
        (
            "codex torn",
            started.to_vec(),
            &complete[..complete.len() / 2],
        ),
        ("codex whole terminal", started.to_vec(), &complete[..]),
        ("claude cut", vec![user, text, stop], &took[..]),
        ("codex cut", started.to_vec(), &complete[..]),
    ];
    let actor = Arc::new(crate::services::provider::CancelToken::new());
    let ended =
        crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::replay_ended_herdr_turn;
    let mut read = Vec::new();
    for (case, lines, tail) in cases {
        let mut file = tempfile::tempfile_in(dir.path()).unwrap();
        for line in &lines {
            writeln!(file, "{line}").unwrap();
        }
        write!(file, "{tail}").unwrap();
        let len = file.metadata().unwrap().len();
        // The source is cut on the same descriptor after its length was read.
        if case.ends_with("cut") {
            file.set_len(len / 2).unwrap();
        }
        read.push(match case.starts_with("claude") {
            true => (case, ended_turn(&mut file, input, len).ok()),
            false => (case, ended(&mut file, 0, len, &actor)),
        });
    }
    assert_eq!(
        read,
        [
            ("claude torn", Some(EndedTurn::NoTerminal)),
            ("claude whole terminal", Some(EndedTurn::Terminal)),
            ("codex torn", Some(EndedTurn::NoTerminal)),
            ("codex whole terminal", Some(EndedTurn::Terminal)),
            ("claude cut", None),
            ("codex cut", None),
        ]
    );
}
