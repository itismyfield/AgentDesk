//! Native output fixtures carry the identities the real O deriver requires.

use super::*;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use dedupe::binding_context::BindingContext;
use std::io::Write;

fn rows(turn: &str, text: &str) -> [serde_json::Value; 4] {
    let message = format!("{turn}-{:x}", Sha256::digest(text.as_bytes()));
    [
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn}}),
        json!({"type":"response_item","payload":{"type":"message","role":"assistant",
            "id":message,"content":[{"type":"output_text","text":text}]}}),
        json!({"type":"event_msg","payload":{"type":"composer_ready","turn_id":turn}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":turn,"last_agent_message":text}}),
    ]
}

pub(super) fn terminal(path: &Path, turn: &str, text: &str) {
    let rows = rows(turn, text);
    let message = format!("{turn}-{:x}", Sha256::digest(text.as_bytes()));
    let facts: Vec<_> = rows
        .iter()
        .flat_map(|row| classify(ShadowProvider::Codex, row))
        .collect();
    assert!(
        !facts
            .iter()
            .any(|fact| matches!(fact, RecordFact::Blocked(_)))
    );
    assert!(
        facts
            .iter()
            .any(|fact| matches!(fact, RecordFact::TurnStart(Some(id)) if id == turn))
    );
    assert!(
        facts
            .iter()
            .any(|fact| matches!(fact, RecordFact::Unit(id, _, _) if id == &message))
    );
    for fact in &facts {
        if let RecordFact::Unit(_, _, content) = fact {
            let plan = crate::services::tui_o::shadow::unit_plan::plan(content).unwrap();
            assert!(
                matches!(plan, crate::services::tui_o::shadow::unit_plan::UnitPlan::Pieces(ref pieces) if !pieces.is_empty())
            );
        }
    }
    let mut output = fs::OpenOptions::new().append(true).open(path).unwrap();
    for row in rows {
        writeln!(output, "{row}").unwrap();
    }
    output.flush().unwrap();
    assert_eq!(
        crate::services::codex_tui::rollout_tail::observe_rollout_turn_state(path),
        crate::services::tui_turn_state::TuiTurnState::Idle,
    );
}

struct WarmProbe {
    spy: crate::services::codex_tui::host_input::spy::SpyGuard,
    calls: Arc<Mutex<Vec<String>>>,
}

impl Drop for WarmProbe {
    fn drop(&mut self) {
        *self.calls.lock().unwrap() = self.spy.calls();
        crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY
            .with_borrow_mut(|slot| *slot = None);
    }
}

pub(super) fn warm_probe(
    context: &BindingContext,
    accepted: Arc<Mutex<Vec<String>>>,
    calls: Arc<Mutex<Vec<String>>>,
) -> (
    crate::services::discord::codex_absence::test_setup::ResetGuard,
    mpsc::Receiver<String>,
    mpsc::Sender<()>,
    mpsc::Receiver<String>,
) {
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NEW_NATIVE_ID}.jsonl"));
    let (entered, wait_entered) = mpsc::channel();
    let (release, wait_release) = mpsc::channel();
    let (boundary, boundary_events) = mpsc::channel();
    let setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |prompt| {
            use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};
            assert!(prompt.contains(b1::Q2) && !prompt.contains(b1::Q1));
            let prompt = prompt.to_owned();
            entered.send(prompt.clone()).unwrap();
            let ready = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
            let draft = b1::warm_draft(&prompt);
            crate::services::codex_tui::warm_followup::ACTUAL_BOUNDARY.with_borrow_mut(|slot| {
                *slot = Some(Box::new(move |stage| {
                    let _ = boundary.send(stage.to_owned());
                }));
            });
            let spy = SpyGuard::install(SpyState {
                pane_size: Some((200, 80)),
                captures: [ready.to_owned(), draft, ready.to_owned(), ready.to_owned()]
                    .into_iter()
                    .map(Some)
                    .collect(),
                on_enter: Some(Box::new(move || {
                    accepted.lock().unwrap().push(prompt.clone());
                    terminal(&path, "b1-q2", SECOND_REPLY);
                })),
                ..SpyState::default()
            });
            let _ = wait_release.recv_timeout(Duration::from_secs(30));
            Box::new(WarmProbe { spy, calls })
        }),
    );
    (setup, wait_entered, release, boundary_events)
}

#[test]
fn native_terminal_fixture_has_a_postable_stable_unit_and_idle_boundary() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("native.jsonl");
    fs::write(&path, "").unwrap();
    terminal(&path, "fixture-turn", "fixture output");
    assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 4);
}
