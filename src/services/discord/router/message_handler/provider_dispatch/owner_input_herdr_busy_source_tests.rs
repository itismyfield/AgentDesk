//! A busy Codex front on a Bound Herdr row reads the rollout its attach proved while its
//! inflight row still names none; a Pending row, a tmux pane and an unproved source do not.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedObservation;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::codex_tui::busy_inject::{TurnVerdict, read_turn};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::tui_prompt_dedupe::{
    TuiRuntimeBinding, clear_tmux_runtime_binding, register_tmux_runtime_binding,
};
use crate::services::turn_host::HerdrTurnPlan;

/// A rollout whose model turn `turn` still runs.
fn rollout(dir: &std::path::Path, native: &str, turn: &str) -> std::path::PathBuf {
    let path = dir.join(format!("rollout-2026-11-26T09-13-34-{native}.jsonl"));
    let event = |payload: Value| format!("{}\n", json!({"type":"event_msg","payload":payload}));
    let context = json!({"type":"turn_context","payload":{"turn_id":turn,
        "sandbox_policy":{"type":"danger-full-access"}}});
    let body = event(json!({"type":"task_started","turn_id":turn,"root_turn_id":turn}))
        + &format!("{context}\n")
        + &event(json!({"type":"item_completed","turn_id":turn,"item":{"type":"UserMessage"}}));
    std::fs::write(&path, body).unwrap();
    path
}

fn bind(logical: &str, kind: RuntimeHandoffKind, path: &std::path::Path, native: Option<&str>) {
    let binding = TuiRuntimeBinding {
        runtime_kind: kind,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: native.map(str::to_owned),
        last_offset: 0,
        relay_last_offset: None,
    };
    register_tmux_runtime_binding(logical, binding);
}

/// The row a turn on `logical` holds as intake seeded it; on Herdr it names no path.
fn row(logical: &str, output: Option<String>) -> InflightTurnState {
    let text = "turn a".to_string();
    let native = Some("row-native".to_string());
    let (name, tmux) = (Some("adk-cdx".to_string()), Some(logical.to_string()));
    InflightTurnState::new(
        ProviderKind::Codex,
        8_426,
        name,
        7,
        1,
        0,
        text,
        native,
        tmux,
        output,
        None,
        0,
    )
}

fn host(owner: &HerdrOwner, provider: &str, state: HostedState) -> TurnHost {
    let row = Some(HostedObservation::known_fixture(
        owner.record(provider, state),
    ));
    TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint: owner.endpoint(),
        row,
        bootstrap: false,
    }))
}

fn source(
    provider: ProviderKind,
    host: &TurnHost,
    row: &InflightTurnState,
) -> Option<(String, Option<String>)> {
    let session = row.tmux_session_name.clone().unwrap_or_default();
    super::super::busy_source::live_source(provider, host, &session, row)
}

/// Clear, first turn, busy input on Herdr: the binding a clear left (A) is never read while the
/// row is Pending, and the Bound row's front registers the new rollout B.
#[test]
fn a_bound_codex_herdr_front_reads_the_rollout_its_attach_proved() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 8_426_101);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let _entered = owner.runtime.enter();
    let (old_native, new_native) = ("aaaa-8426", "bbbb-8426");
    let old = rollout(owner._dir.path(), old_native, "turn-a");
    let new = rollout(owner._dir.path(), new_native, "turn-b");
    let inflight = row(&owner.logical, None);
    let kind = ("codex", InputOrigin::Discord, true);
    // Contract GREEN before and after: a Pending row is Absent, so the front is never handed in.
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &old,
        Some(old_native),
    );
    let pending = host(&owner, "codex", HostedState::Pending);
    assert_eq!(source(ProviderKind::Codex, &pending, &inflight), None);
    let at = (&pending, owner.logical.as_str(), None);
    let registered = Registration::observed(owner.mailbox.clone(), kind, at, &owner.original, None);
    assert_eq!(registered.unwrap().phase, target::Phase::Absent);
    // RED before the fix: Codex took only the empty row path, so the transcript was "" and the
    // busy route answered NotSent("busy_turn: transcript unavailable").
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &new,
        Some(new_native),
    );
    let bound = host(&owner, "codex", HostedState::Bound);
    let chosen = source(ProviderKind::Codex, &bound, &inflight);
    assert_eq!(
        chosen,
        Some((new.display().to_string(), Some(new_native.to_string())))
    );
    let live = chosen
        .as_ref()
        .map(|(path, native)| (path.as_str(), native.as_deref()));
    let at = (&bound, owner.logical.as_str(), live);
    let registered = Registration::observed(owner.mailbox.clone(), kind, at, &owner.original, None);
    let registered = registered.unwrap();
    assert_eq!(registered.phase, target::Phase::Busy);
    assert_eq!(registered.transcript, new, "CodexInput.rollout is B");
    assert_eq!(registered.attempt["source"]["session_id"], new_native);
    let turn = Some(TurnVerdict::KnownModelTurn("turn-b".into()));
    assert_eq!(read_turn(&registered.transcript), turn);
    clear_tmux_runtime_binding(&owner.logical);
}

/// Contract GREEN before and after: tmux has no attach proof, so its row path stands and a
/// pathless row (the clear window) stays without a source.
#[test]
fn a_tmux_codex_front_keeps_its_row_path() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let logical = "AgentDesk-codex-busy-source-tmux-8426";
    bind(
        logical,
        RuntimeHandoffKind::CodexTui,
        &rollout(dir.path(), "cccc-8426", "turn-c"),
        Some("cccc-8426"),
    );
    assert_eq!(
        source(ProviderKind::Codex, &TurnHost::Tmux, &row(logical, None)),
        None
    );
    let path = dir.path().join("row.jsonl").display().to_string();
    let seeded = row(logical, Some(path.clone()));
    let kept = Some((path, Some("row-native".to_string())));
    assert_eq!(source(ProviderKind::Codex, &TurnHost::Tmux, &seeded), kept);
    clear_tmux_runtime_binding(logical);
}

/// Contract GREEN before and after: without a proved CodexTui source the Bound front reads no
/// turn (NotSent "busy_turn: transcript unavailable"), and a Claude front never takes it.
#[test]
fn a_bound_herdr_front_without_a_proved_codex_source_reads_no_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 8_426_103);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let _entered = owner.runtime.enter();
    let path = rollout(owner._dir.path(), "dddd-8426", "turn-d");
    let inflight = row(&owner.logical, None);
    let bound = host(&owner, "codex", HostedState::Bound);
    clear_tmux_runtime_binding(&owner.logical);
    assert_eq!(
        source(ProviderKind::Codex, &bound, &inflight),
        None,
        "no binding"
    );
    let unproved = [
        (RuntimeHandoffKind::CodexTui, None),
        (RuntimeHandoffKind::ClaudeTui, Some("dddd-8426")),
    ];
    for (kind, native) in unproved {
        bind(&owner.logical, kind, &path, native);
        assert_eq!(
            source(ProviderKind::Codex, &bound, &inflight),
            None,
            "{kind:?} {native:?}"
        );
    }
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &path,
        Some("dddd-8426"),
    );
    let claude = host(&owner, "claude", HostedState::Bound);
    assert_eq!(
        source(ProviderKind::Claude, &claude, &inflight),
        None,
        "Claude never reads it"
    );
    let at = (&bound, owner.logical.as_str(), None);
    let kind = ("codex", InputOrigin::Discord, true);
    let registered = Registration::observed(owner.mailbox.clone(), kind, at, &owner.original, None);
    assert_eq!(read_turn(&registered.unwrap().transcript), None);
    clear_tmux_runtime_binding(&owner.logical);
}
