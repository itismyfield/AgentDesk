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

/// The clear removes the row only while the locked file is still the turn the pass read, from a
/// prior process, with no kind, receipt or hot swap; a receipt written after the pass chose the
/// row keeps it for receipt settlement.
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
