//! #8426: a busy Codex front on a Bound Herdr row reads the rollout its attach proved while its
//! inflight row still names none; before that attach it registers no source, a tmux pane keeps
//! its row path, and a front without a proved source still reads no turn.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedObservation;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::codex_tui::busy_inject::{TurnVerdict, read_turn};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::tui_prompt_dedupe::{
    TuiRuntimeBinding, clear_tmux_runtime_binding, register_tmux_runtime_binding,
    runtime_binding_for_tmux_session,
};
use crate::services::turn_host::HerdrTurnPlan;

fn event(payload: Value) -> String {
    format!("{}\n", json!({"type":"event_msg","payload":payload}))
}

/// A rollout whose last turn ended (`open` None) or whose model turn `open` still runs.
fn rollout(dir: &std::path::Path, native: &str, open: Option<&str>) -> std::path::PathBuf {
    let path = dir.join(format!("rollout-2026-11-26T09-13-34-{native}.jsonl"));
    let body = match open {
        Some(turn) => {
            event(json!({"type":"task_started","turn_id":turn,"root_turn_id":turn}))
                + &format!(
                    "{}\n",
                    json!({"type":"turn_context","payload":{"turn_id":turn,
                        "sandbox_policy":{"type":"danger-full-access"}}})
                )
                + &event(json!({"type":"item_completed","turn_id":turn,
                    "item":{"type":"UserMessage"}}))
        }
        None => {
            event(json!({"type":"task_started","turn_id":"done","root_turn_id":"done"}))
                + &event(json!({"type":"task_complete","turn_id":"done"}))
        }
    };
    std::fs::write(&path, body).unwrap();
    path
}

fn bind(logical: &str, kind: RuntimeHandoffKind, path: &std::path::Path, native: Option<&str>) {
    register_tmux_runtime_binding(
        logical,
        TuiRuntimeBinding {
            runtime_kind: kind,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: native.map(str::to_owned),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    assert_eq!(
        runtime_binding_for_tmux_session(logical).map(|binding| binding.output_path),
        Some(path.display().to_string()),
        "the fixture binding is readable"
    );
}

/// The inflight row a turn on `logical` holds as intake seeded it: a Herdr Codex row names no
/// path (`output` None) because no tmux pane lives under the logical key.
fn running_row(logical: &str, channel: u64, output: Option<String>) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        Some("adk-cdx".to_string()),
        7,
        8_426_000_000_000_001,
        0,
        "turn a".to_string(),
        Some("row-native".to_string()),
        Some(logical.to_string()),
        output,
        None,
        0,
    )
}

fn herdr_host(owner: &HerdrOwner, provider: &str, state: HostedState) -> TurnHost {
    TurnHost::Herdr(Box::new(HerdrTurnPlan {
        endpoint: owner.endpoint(),
        row: Some(HostedObservation::known_fixture(
            owner.record(provider, state),
        )),
        bootstrap: false,
    }))
}

fn live_source(
    provider: ProviderKind,
    host: &TurnHost,
    session: &str,
    row: &InflightTurnState,
) -> Option<(String, Option<String>)> {
    super::super::busy_source::live_source(provider, host, session, row)
}

/// Clear, first turn, busy input on Herdr: the binding a clear left behind is never read before
/// the new execution's attach, and the Bound row's front reads the new rollout B.
#[test]
fn a_bound_codex_herdr_front_reads_the_rollout_its_attach_proved() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 8_426_101);
    let _hosts =
        crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let _entered = owner.runtime.enter();
    let dir = owner._dir.path();
    let old = rollout(dir, "aaaaaaaa-0000-4000-8000-000000008426", None);
    let new_native = "bbbbbbbb-0000-4000-8000-000000008426";
    let new = rollout(dir, new_native, Some("turn-b"));
    assert_eq!(
        read_turn(&new),
        Some(TurnVerdict::KnownModelTurn("turn-b".into()))
    );
    let row = running_row(&owner.logical, owner.channel, None);
    let kind = ("codex", InputOrigin::Discord, true);

    // Contract GREEN before and after: the first turn's row is Pending until its attach, so the
    // binding a clear left behind (A) registers no source and the front is not handed in.
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &old,
        Some("aaaaaaaa-0000-4000-8000-000000008426"),
    );
    let pending = herdr_host(&owner, "codex", HostedState::Pending);
    let source = live_source(ProviderKind::Codex, &pending, &owner.logical, &row);
    assert_eq!(source, None, "a Pending row never reads the binding");
    let registration = Registration::observed(
        owner.mailbox.clone(),
        kind,
        (&pending, owner.logical.as_str(), None),
        &owner.original,
        None,
    )
    .unwrap();
    assert_eq!(registration.phase, target::Phase::Absent);

    // RED before the fix: hand_busy took only the row path for Codex, so the transcript was empty
    // and the busy route answered NotSent("busy_turn: transcript unavailable").
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &new,
        Some(new_native),
    );
    let bound = herdr_host(&owner, "codex", HostedState::Bound);
    let source = live_source(ProviderKind::Codex, &bound, &owner.logical, &row);
    assert_eq!(
        source,
        Some((new.display().to_string(), Some(new_native.to_string())))
    );
    let live = source
        .as_ref()
        .map(|(path, native)| (path.as_str(), native.as_deref()));
    let registration = Registration::observed(
        owner.mailbox.clone(),
        kind,
        (&bound, owner.logical.as_str(), live),
        &owner.original,
        None,
    )
    .unwrap();
    assert_eq!(registration.phase, target::Phase::Busy);
    assert_eq!(registration.transcript, new, "CodexInput.rollout is B");
    assert_eq!(registration.attempt["source"]["kind"], "existing");
    assert_eq!(registration.attempt["source"]["session_id"], new_native);
    assert_eq!(
        read_turn(&registration.transcript),
        Some(TurnVerdict::KnownModelTurn("turn-b".into())),
        "the busy route reads the running turn instead of refusing"
    );
    clear_tmux_runtime_binding(&owner.logical);
}

/// Contract GREEN before and after: a tmux Codex pane has no attach proof, so the binding never
/// replaces its row path; a pathless row (the clear window) stays without a source.
#[test]
fn a_tmux_codex_front_keeps_its_row_path_and_ignores_the_binding() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let logical = "AgentDesk-codex-busy-source-tmux-8426";
    let left = rollout(dir.path(), "cccccccc-0000-4000-8000-000000008426", Some("turn-c"));
    bind(
        logical,
        RuntimeHandoffKind::CodexTui,
        &left,
        Some("cccccccc-0000-4000-8000-000000008426"),
    );
    let pathless = running_row(logical, 8_426_102, None);
    assert_eq!(
        live_source(ProviderKind::Codex, &TurnHost::Tmux, logical, &pathless),
        None,
        "the clear window on tmux stays refused"
    );
    let row_path = dir.path().join("row.jsonl").display().to_string();
    let seeded = running_row(logical, 8_426_102, Some(row_path.clone()));
    assert_eq!(
        live_source(ProviderKind::Codex, &TurnHost::Tmux, logical, &seeded),
        Some((row_path, Some("row-native".to_string())))
    );
    clear_tmux_runtime_binding(logical);
}

/// Contract GREEN before and after: without a proved CodexTui source of the Bound execution the
/// front reads no turn and the busy route refuses with its reason; Claude never takes it.
#[test]
fn a_bound_herdr_front_without_a_proved_codex_source_reads_no_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("codex", 8_426_103);
    let _hosts =
        crate::config::session_hosts::force_for_test(Some(NODE), &[(owner.channel, NODE)]);
    let _registry = owner.rig.registry_on_this_thread();
    let _entered = owner.runtime.enter();
    let dir = owner._dir.path();
    let native = "dddddddd-0000-4000-8000-000000008426";
    let path = rollout(dir, native, Some("turn-d"));
    let row = running_row(&owner.logical, owner.channel, None);
    let bound = herdr_host(&owner, "codex", HostedState::Bound);

    clear_tmux_runtime_binding(&owner.logical);
    let source = live_source(ProviderKind::Codex, &bound, &owner.logical, &row);
    assert_eq!(source, None, "no binding: no source");
    let registration = Registration::observed(
        owner.mailbox.clone(),
        ("codex", InputOrigin::Discord, true),
        (&bound, owner.logical.as_str(), None),
        &owner.original,
        None,
    )
    .unwrap();
    assert_eq!(registration.phase, target::Phase::Busy);
    assert_eq!(
        read_turn(&registration.transcript),
        None,
        "the route answers NotSent(\"busy_turn: transcript unavailable\")"
    );

    bind(&owner.logical, RuntimeHandoffKind::CodexTui, &path, None);
    assert_eq!(
        live_source(ProviderKind::Codex, &bound, &owner.logical, &row),
        None,
        "a binding without its native session proves nothing"
    );
    bind(
        &owner.logical,
        RuntimeHandoffKind::ClaudeTui,
        &path,
        Some(native),
    );
    assert_eq!(
        live_source(ProviderKind::Codex, &bound, &owner.logical, &row),
        None,
        "a binding of another runtime proves nothing"
    );
    bind(
        &owner.logical,
        RuntimeHandoffKind::CodexTui,
        &path,
        Some(native),
    );
    let claude = herdr_host(&owner, "claude", HostedState::Bound);
    assert_eq!(
        live_source(ProviderKind::Claude, &claude, &owner.logical, &row),
        None,
        "a Claude front never reads the Codex binding"
    );
    clear_tmux_runtime_binding(&owner.logical);
}
