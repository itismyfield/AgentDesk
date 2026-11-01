use super::*;
use crate::services::tui_input::human_frame::{self, history};
use history::append::{self, Admission};
use std::io::Write;
use std::path::Path;

fn frame(body: &str) -> String {
    human_frame::plain_inject("discord", "7504", "ann", "ignored", body)
}

fn register_append(
    path: &Path,
    tmux: &str,
    nonce: &str,
    target: &str,
    prompt: &str,
    separator: bool,
    message: Option<u64>,
) {
    append::register(
        (tmux, nonce, "7504"),
        path,
        std::fs::metadata(path).unwrap().len(),
        prompt,
        target,
        message,
        separator,
    )
    .unwrap();
    append::mark(nonce, path, Admission::Admitted).unwrap();
}

fn bind(path: &Path, tmux: &str, prompt: &str, row: &str, turn: &str, image: bool) {
    let mut content = vec![serde_json::json!({"type":"text", "text":prompt, "text_elements":[]})];
    if image {
        let image_at = prompt.find("[Image #1]").unwrap();
        content[0]["text_elements"] = serde_json::json!([
            {"byte_range":{"start":image_at,"end":image_at + 10},"placeholder":"[Image #1]"}
        ]);
        content.insert(
            0,
            serde_json::json!({"type":"local_image", "path":path.with_extension("png")}),
        );
    }
    let record = serde_json::json!({"type":"event_msg","payload":{
        "type":"item_completed","turn_id":turn,
        "item":{"type":"UserMessage","id":row,"content":content}
    }});
    let start = std::fs::metadata(path).unwrap().len();
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
    assert!(append::bind_record(tmux, path, start, &record).is_some());
}

fn lifecycle(path: &Path, kind: &str, turn: &str) {
    let record = serde_json::json!({"type":"event_msg","payload":{
        "type":kind,"turn_id":turn,"root_turn_id":turn
    }});
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
}

#[test]
fn distinct_plain_rows_each_hand_off_the_same_body_inside_the_recent_window() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let tmux = format!("handed-identity-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    lifecycle(&path, "task_started", "t0");
    let prompt = frame("same words");
    let mut rx = subscribe_observed_prompts();
    let at = Utc::now();
    for (nonce, target, turn, row, message) in [
        ("aaaa7504", "t0", "t1", "row-a", 751),
        ("bbbb7504", "t1", "t2", "row-b", 752),
    ] {
        assert!(register_injected_steer(
            "codex",
            &tmux,
            nonce,
            target,
            Some(message)
        ));
        register_append(&path, &tmux, nonce, target, &prompt, false, Some(message));
        withdraw_injected_steer("codex", &tmux, nonce);
        lifecycle(&path, "task_complete", target);
        assert_eq!(
            crate::services::codex_tui::busy_inject::read_turn(&path),
            Some(crate::services::codex_tui::busy_inject::TurnVerdict::NotBusy)
        );
        lifecycle(&path, "task_started", turn);
        bind(&path, &tmux, &prompt, row, turn, false);
        assert_eq!(
            observe_codex_prompt_in_turn_at(&tmux, &prompt, Some(row), Some(turn), at),
            PromptObservation::PublishedSshDirect,
            "a new native row's first Handed must not merge with an earlier attempt's body"
        );
        let published: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event.tmux_session_name == tmux)
            .collect();
        assert_eq!(published.len(), 1, "{row} must publish exactly once");
        assert_eq!(published[0].source_event_id.as_deref(), Some(row));
        assert_eq!(published[0].native_turn_id.as_deref(), Some(turn));
        assert!(native_turn_is_open("codex", &tmux, turn));
        adopt_native_turn(&published[0]);
        Instant::advance(Duration::from_secs(1));
    }
    for (row, turn) in [("row-a", "t1"), ("row-b", "t2")] {
        assert_eq!(
            observe_codex_prompt_in_turn_at(&tmux, &prompt, Some(row), Some(turn), at),
            PromptObservation::SuppressedRecentDuplicate
        );
    }
    assert_eq!(
        observe_codex_prompt_in_turn_at(&tmux, &prompt, Some("late-response-a"), Some("t1"), at),
        PromptObservation::InjectedDeferred
    );
    assert!(
        rx.try_recv().is_err(),
        "re-observation must publish nothing"
    );
}

#[test]
fn text_and_image_hooks_defer_without_owning_or_aliasing_before_completion() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for image in [false, true] {
        let tmux = format!("hook-append-{image}-{}", uuid::Uuid::new_v4());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(&path, "").unwrap();
        let framed = frame("new message  ");
        let prefix = if image {
            " \n[Image #1] \ndraft note"
        } else {
            " \ndraft quoting [User: bob (ID: 1)]\nmy note"
        };
        let mixed = format!("{prefix}\n{framed}");
        register_append(&path, &tmux, "cccc7504", "target", &framed, true, Some(751));
        record_prompt_anchor("codex", &tmux, 7504, 99);
        let anchor = prompt_anchor_for_response("codex", &tmux, 7504);
        let mut rx = subscribe_observed_prompts();
        let seen = hosted_source::observe_hook_at(
            "codex",
            &tmux,
            &mixed,
            (None, Some("target")),
            Utc::now(),
            &serde_json::json!({}),
            None,
        );
        assert_eq!(seen, PromptObservation::InjectedDeferred);
        assert_eq!(
            observe_codex_prompt_in_turn_at(
                &tmux,
                &mixed,
                Some("response-before-completion"),
                Some("target"),
                Utc::now()
            ),
            PromptObservation::InjectedDeferred
        );
        assert_eq!(history::receipt_row("cccc7504", &path), None);
        assert_eq!(prompt_anchor_for_response("codex", &tmux, 7504), anchor);
        assert!(!native_turn_is_open("codex", &tmux, "target"));
        assert!(!external_input_relay_lease_present("codex", &tmux, 7504));
        assert!(rx.try_recv().is_err());
        bind(&path, &tmux, &mixed, "native-row", "target", image);
        assert_eq!(
            observe_codex_prompt_in_turn_at(
                &tmux,
                &mixed,
                Some("native-row"),
                Some("target"),
                Utc::now()
            ),
            PromptObservation::InjectedSteer
        );
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn a_bound_admission_restores_without_competing_with_eight_uncertain_reservations() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let tmux = format!("restore-slots-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let prompt = frame("admitted before restart");
    register_append(&path, &tmux, "dddd7504", "target", &prompt, false, None);
    bind(&path, &tmux, &prompt, "late-row", "target", false);
    for n in 0..8 {
        let nonce = format!("uncertain-{n}");
        assert!(register_injected_steer(
            "codex", &tmux, &nonce, "target", None
        ));
        append::register(
            (&tmux, &nonce, "7504"),
            &path,
            std::fs::metadata(&path).unwrap().len(),
            &frame(&format!("uncertain {n}")),
            "target",
            None,
            false,
        )
        .unwrap();
        append::mark(&nonce, &path, Admission::Uncertain).unwrap();
    }
    assert_eq!(
        observe_codex_prompt_in_turn_at(
            &tmux,
            &prompt,
            Some("late-row"),
            Some("target"),
            Utc::now()
        ),
        PromptObservation::InjectedSteer
    );
    assert!(!register_injected_steer(
        "codex", &tmux, "ninth", "target", None
    ));
    assert_eq!(
        append::projection("codex", &tmux, "dddd7504")
            .unwrap()
            .decision,
        Some(append::Decision::Joined)
    );
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let ledger = state
            .injected_steer_by_tmux
            .get_mut(&PromptKey::new("codex", &tmux))
            .unwrap();
        ledger.retain(|entry| entry.value.nonce != "dddd7504");
    }
    assert_eq!(
        observe_codex_prompt_in_turn_at(&tmux, &prompt, Some("late-row"), None, Utc::now()),
        PromptObservation::InjectedSteer,
        "the durable Joined decision is reused after its memory identity is gone"
    );
    assert!(!register_injected_steer(
        "codex",
        &tmux,
        "still-ninth",
        "target",
        None
    ));
}

#[test]
fn first_handed_row_bypasses_body_pending_while_an_existing_native_owner_still_joins() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for owned in [false, true] {
        let tmux = format!("handed-pending-{owned}-{}", uuid::Uuid::new_v4());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(&path, "").unwrap();
        let prompt = frame("same body pending elsewhere");
        register_append(
            &path,
            &tmux,
            "ffff7504",
            "old-target",
            &prompt,
            false,
            Some(751),
        );
        bind(&path, &tmux, &prompt, "native-row", "current-turn", false);
        record_discord_originated_prompt("codex", &tmux, &prompt);
        if owned {
            open_native_turn(
                "codex",
                &tmux,
                "current-turn",
                (
                    SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
                    NativeTurnOwner::Answer,
                ),
            );
        }
        let mut rx = subscribe_observed_prompts();
        let (expected, decision) = if owned {
            (PromptObservation::InjectedSteer, append::Decision::Joined)
        } else {
            (
                PromptObservation::PublishedSshDirect,
                append::Decision::HandedPending,
            )
        };
        assert_eq!(
            observe_codex_prompt_in_turn_at(
                &tmux,
                &prompt,
                Some("native-row"),
                Some("current-turn"),
                Utc::now()
            ),
            expected
        );
        let published = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event.tmux_session_name == tmux)
            .count();
        assert_eq!(published, usize::from(!owned));
        assert_eq!(
            append::projection("codex", &tmux, "ffff7504")
                .unwrap()
                .decision,
            Some(decision)
        );
        assert!(take_matching_pending_prompt("codex", &tmux, &prompt));
    }
}

#[test]
fn cold_handed_pending_does_not_claim_external_completion_or_republish() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let tmux = format!("cold-handoff-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let prompt = frame("a late queued input");
    register_append(
        &path,
        &tmux,
        "eeee7504",
        "old-target",
        &prompt,
        false,
        Some(751),
    );
    bind(&path, &tmux, &prompt, "handoff-row", "new-turn", false);
    let mut rx = subscribe_observed_prompts();
    assert_eq!(
        observe_codex_prompt_in_turn_at(
            &tmux,
            &prompt,
            Some("handoff-row"),
            Some("new-turn"),
            Utc::now()
        ),
        PromptObservation::PublishedSshDirect
    );
    assert!(rx.try_recv().is_ok());
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let key = PromptKey::new("codex", &tmux);
        state.injected_steer_by_tmux.remove(&key);
        state.native_turn_by_tmux.remove(&key);
        state.relayed_entry_ids_by_tmux.remove(&key);
        state.recent_observed_by_tmux.remove(&key);
    }
    assert_eq!(
        observe_codex_prompt_in_turn_at(
            &tmux,
            &prompt,
            Some("handoff-row"),
            Some("new-turn"),
            Utc::now()
        ),
        PromptObservation::InjectedDeferred
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(
        append::projection("codex", &tmux, "eeee7504")
            .unwrap()
            .decision,
        Some(append::Decision::HandedPending)
    );
}

fn bind_response_receipt(
    path: &Path,
    tmux: &str,
    prompt: &str,
    turn: Option<&str>,
) -> append::Receipt {
    let record = serde_json::json!({"type":"event_msg","payload":{
        "type":"item_completed","turn_id":turn,
        "item":{"type":"UserMessage","id":"response-row","content":[{"type":"text","text":prompt}]}
    }});
    let start = std::fs::metadata(path).unwrap().len();
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
    let receipt = append::bind_record(tmux, path, start, &record).unwrap();
    assert_eq!(receipt.native_turn.as_deref(), turn);
    receipt
}

fn observe_response_record(
    tmux: &str,
    prompt: &str,
    own_turn: Option<&str>,
    inferred_turn: Option<&str>,
) -> PromptObservation {
    let record = serde_json::json!({"type":"response_item","payload":{
        "type":"message","role":"user","id":"response-row",
        "content":[{"type":"input_text","text":prompt}],
        "internal_chat_message_metadata_passthrough":{
            "turn_id":own_turn,"create_time":Utc::now().timestamp_millis() as f64 / 1000.0
        }
    }});
    let (extracted, row, evidence) =
        extract_codex_rollout_user_prompt_with_evidence(&record).unwrap();
    assert_eq!(extracted, prompt);
    assert_eq!(row.as_deref(), Some("response-row"));
    assert_eq!(evidence.turn_id.as_deref(), own_turn);
    observe_codex_record_prompt_at(
        tmux,
        &extracted,
        row.as_deref(),
        inferred_turn,
        &evidence,
        Utc::now(),
    )
}

fn assert_response_still_unowned(
    tmux: &str,
    nonce: &str,
    rx: &mut tokio::sync::broadcast::Receiver<ObservedTuiPrompt>,
) {
    assert_eq!(
        append::projection("codex", tmux, nonce).unwrap().decision,
        None
    );
    let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    assert!(
        !state
            .native_turn_by_tmux
            .contains_key(&PromptKey::new("codex", tmux))
    );
    drop(state);
    assert!(!external_input_relay_lease_present("codex", tmux, 7504));
    assert_eq!(
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event.tmux_session_name == tmux)
            .count(),
        0
    );
}

#[test]
fn response_inferred_turn_cannot_settle_a_turnless_append_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for inferred in ["target", "other-turn"] {
        let tmux = format!("receipt-no-turn-{}", uuid::Uuid::new_v4());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(&path, "").unwrap();
        let prompt = frame("response cannot supply missing receipt turn");
        register_append(
            &path,
            &tmux,
            "repair-no-turn",
            "target",
            &prompt,
            false,
            Some(751),
        );
        let receipt = bind_response_receipt(&path, &tmux, &prompt, None);
        let mut rx = subscribe_observed_prompts();
        assert_eq!(
            observe_codex_prompt_in_turn_at(
                &tmux,
                &receipt.prompt,
                Some(&receipt.row),
                receipt.native_turn.as_deref(),
                Utc::now(),
            ),
            PromptObservation::InjectedDeferred
        );
        assert_response_still_unowned(&tmux, "repair-no-turn", &mut rx);
        assert_eq!(
            observe_response_record(&tmux, &prompt, None, Some(inferred)),
            PromptObservation::InjectedDeferred,
            "inferred {inferred} is not receipt evidence"
        );
        assert_response_still_unowned(&tmux, "repair-no-turn", &mut rx);
    }
}

#[test]
fn response_native_turn_must_match_the_receipt_before_first_settlement() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for target in ["t1", "old-target"] {
        let tmux = format!("receipt-turn-match-{}", uuid::Uuid::new_v4());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native.jsonl");
        std::fs::write(&path, "").unwrap();
        let prompt = frame("receipt owns the native turn");
        register_append(
            &path,
            &tmux,
            "repair-match",
            target,
            &prompt,
            false,
            Some(751),
        );
        bind_response_receipt(&path, &tmux, &prompt, Some("t1"));
        let mut rx = subscribe_observed_prompts();
        assert_eq!(
            observe_response_record(&tmux, &prompt, Some("t2"), Some("t1")),
            PromptObservation::InjectedDeferred
        );
        assert_response_still_unowned(&tmux, "repair-match", &mut rx);
        let (expected, decision) = if target == "t1" {
            (PromptObservation::InjectedSteer, append::Decision::Joined)
        } else {
            (
                PromptObservation::PublishedSshDirect,
                append::Decision::HandedPending,
            )
        };
        assert_eq!(
            observe_response_record(&tmux, &prompt, Some("t1"), Some("t2")),
            expected
        );
        assert_eq!(
            append::projection("codex", &tmux, "repair-match")
                .unwrap()
                .decision,
            Some(decision)
        );
        let published = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event.tmux_session_name == tmux)
            .count();
        assert_eq!(published, usize::from(target != "t1"));
        if target == "t1" {
            STATE
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .injected_steer_by_tmux
                .remove(&PromptKey::new("codex", &tmux));
            assert_eq!(
                observe_response_record(&tmux, &prompt, None, None),
                PromptObservation::InjectedSteer,
                "a settled Joined row reuses its durable decision"
            );
            assert!(rx.try_recv().is_err());
        }
    }
}

/// What the real observer and the append readers make of one V1 append state, in the V1 store or
/// after the history migrated to V2, before and after the observer's memory restarts.
fn migrated_append_world(v2: bool, state: &str, separator: bool) -> (Vec<String>, usize) {
    let ledger = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let _ = std::fs::remove_file(&ledger);
    let tmux = format!("migrated-{state}-{v2}-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    lifecycle(&path, "task_started", "t0");
    let framed = frame("migrated body  ");
    let prompt = if separator {
        format!("draft note\n{framed}")
    } else {
        framed.clone()
    };
    let nonce = "mmmm7504";
    let mut rx = subscribe_observed_prompts();
    assert!(register_injected_steer(
        "codex",
        &tmux,
        nonce,
        "t0",
        Some(761)
    ));
    register_append(&path, &tmux, nonce, "t0", &framed, separator, Some(761));
    let turn = if state == "handed" { "t1" } else { "t0" };
    if state == "handed" {
        lifecycle(&path, "task_complete", "t0");
        lifecycle(&path, "task_started", "t1");
    }
    if state != "admitted" {
        bind(&path, &tmux, &prompt, "row-m", turn, false);
    }
    match state {
        "joined" => assert!(append::settle(
            nonce,
            &tmux,
            "row-m",
            append::Decision::Joined
        )),
        "handed" => assert!(append::settle(
            nonce,
            &tmux,
            "row-m",
            append::Decision::HandedPending
        )),
        _ => {}
    }
    let restart = || {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state
            .injected_steer_by_tmux
            .remove(&PromptKey::new("codex", &tmux));
        state
            .native_turn_by_tmux
            .remove(&PromptKey::new("codex", &tmux));
    };
    restart();
    if v2 {
        let empty = serde_json::json!({"namespace":"migration","head_injection":null,
            "retained_inputs":[],"terminal_identities":[]});
        history::v2::Registry::runtime()
            .unwrap()
            .project(&empty)
            .unwrap();
        let file = std::fs::read_to_string(&ledger).unwrap();
        assert!(file.starts_with("{\"version\":2,"), "{file}");
    }
    let mut out = Vec::new();
    let read = |out: &mut Vec<String>| {
        let p = append::projection("codex", &tmux, nonce);
        out.push(format!(
            "projection={:?}",
            p.map(|p| (
                p.target_turn,
                p.admitted,
                p.bound,
                p.row,
                p.native_turn,
                p.decision
            ))
        ));
        out.push(format!(
            "replay_start={:?}",
            append::replay_start(&tmux, &path)
        ));
        out.push(format!(
            "pending={}",
            append::pending_candidate("codex", &tmux, std::slice::from_ref(&prompt), None)
        ));
    };
    read(&mut out);
    if state == "admitted" {
        // The native observer's bind of the completion row, as the rollout reader calls it.
        let record = serde_json::json!({"type":"event_msg","payload":{
            "type":"item_completed","turn_id":turn,
            "item":{"type":"UserMessage","id":"row-m","content":[{"type":"text","text":prompt,"text_elements":[]}]}
        }});
        let start = std::fs::metadata(&path).unwrap().len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
        let bound = append::bind_record_at(&path, start, &record);
        out.push(format!(
            "bind={:?}",
            bound.map(|r| (r.attempt.id, r.row, r.native_turn))
        ));
    }
    let at = Utc::now();
    for _ in 0..2 {
        out.push(format!(
            "observed={:?}",
            observe_codex_prompt_in_turn_at(&tmux, &prompt, Some("row-m"), Some(turn), at)
        ));
        read(&mut out);
        restart();
    }
    let published = std::iter::from_fn(|| rx.try_recv().ok())
        .filter(|event| event.tmux_session_name == tmux)
        .count();
    (out, published)
}

/// A V1 append migrated into V2 binds, settles, replays and is observed as in V1 for every stored
/// state, with or without a draft before the frame, and is handed off at most once.
#[test]
fn a_migrated_v1_append_binds_settles_and_replays_as_in_v1() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for state in ["admitted", "bound", "joined", "handed"] {
        for separator in [false, true] {
            let (v1, v1_published) = migrated_append_world(false, state, separator);
            let (v2, v2_published) = migrated_append_world(true, state, separator);
            assert_eq!(v2, v1, "{state} separator={separator}");
            assert_eq!(v2_published, v1_published, "{state} separator={separator}");
            assert!(
                v2_published <= 1,
                "{state} separator={separator}: {v2_published}"
            );
            assert!(
                v1.iter()
                    .any(|line| line.contains("bound: true")
                        || line.contains(", true, Some(\"row-m\")")),
                "{state} separator={separator}: the append reaches its row: {v1:?}"
            );
        }
    }
}

/// A bound append whose row landed in a new turn, migrated before any decision, is handed off
/// exactly once by the first observer; a cold re-observation publishes nothing more.
#[test]
fn a_migrated_undecided_append_in_a_new_turn_hands_off_exactly_once() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let ledger = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let _ = std::fs::remove_file(&ledger);
    let tmux = format!("migrated-handoff-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    let prompt = frame("a migrated late input");
    register_append(&path, &tmux, "hhhh7504", "t0", &prompt, false, Some(764));
    bind(&path, &tmux, &prompt, "row-h", "t1", false);
    let empty = serde_json::json!({"namespace":"migration","head_injection":null,
        "retained_inputs":[],"terminal_identities":[]});
    history::v2::Registry::runtime()
        .unwrap()
        .project(&empty)
        .unwrap();
    assert!(
        std::fs::read_to_string(&ledger)
            .unwrap()
            .starts_with("{\"version\":2,")
    );
    let mut rx = subscribe_observed_prompts();
    let mut published = || {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| event.tmux_session_name == tmux)
            .count()
    };
    let observe =
        || observe_codex_prompt_in_turn_at(&tmux, &prompt, Some("row-h"), Some("t1"), Utc::now());
    assert_eq!(observe(), PromptObservation::PublishedSshDirect);
    assert_eq!(published(), 1);
    {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        let key = PromptKey::new("codex", &tmux);
        state.injected_steer_by_tmux.remove(&key);
        state.native_turn_by_tmux.remove(&key);
        state.relayed_entry_ids_by_tmux.remove(&key);
        state.recent_observed_by_tmux.remove(&key);
    }
    assert_eq!(observe(), PromptObservation::InjectedDeferred);
    assert_eq!(published(), 0);
    assert_eq!(
        append::projection("codex", &tmux, "hhhh7504")
            .unwrap()
            .decision,
        Some(append::Decision::HandedPending)
    );
}

/// One migrated V1 append A beside a V2 attempt B on the same transcript; returns A's bind and
/// B's receipt for the row both could own, in the case's order, then again after a restart.
#[cfg(unix)]
fn migrated_rival(case: &str) -> [(Option<String>, Option<bool>, bool); 2] {
    use std::os::unix::fs::MetadataExt;
    let ledger = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let _ = std::fs::remove_file(&ledger);
    let tmux = format!("rival-{case}-{}", uuid::Uuid::new_v4());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    std::fs::write(&path, "").unwrap();
    lifecycle(&path, "task_started", "t0");
    let framed = frame("rival body");
    let nonce = "rrrr7504";
    assert!(register_injected_steer(
        "codex",
        &tmux,
        nonce,
        "t0",
        Some(762)
    ));
    register_append(&path, &tmux, nonce, "t0", &framed, false, Some(762));
    let record = serde_json::json!({"type":"event_msg","payload":{"type":"item_completed",
        "turn_id":"t0","item":{"type":"UserMessage","id":"row-r",
        "content":[{"type":"text","text":framed,"text_elements":[]}]}}});
    let start = std::fs::metadata(&path).unwrap().len();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{record}").unwrap();
    let bind = || append::bind_record_at(&path, start, &record).map(|r| r.attempt.id);
    if case == "owned" {
        assert_eq!(
            bind().as_deref(),
            Some(nonce),
            "A owns the row before B exists"
        );
    }
    let empty = serde_json::json!({"namespace":"migration","head_injection":null,
        "retained_inputs":[],"terminal_identities":[]});
    history::v2::Registry::runtime()
        .unwrap()
        .project(&empty)
        .unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let attempt = serde_json::json!({"nonce":"rival-b","provider":"codex",
        "target":{"host":"tmux","owner":tmux,"root":"/tmp","execution_nonce":"spawn"},
        "source":{"kind":"existing","session_id":"native","transcript":path,
            "file":[meta.dev(), meta.ino()],"offset":0}});
    let identity = serde_json::json!({"primary":763,"queued_generation":1,
        "sources":[{"message_id":763,"queued_generation":1}]});
    let b_frame = if case == "other_frame" {
        frame("other body")
    } else {
        framed.clone()
    };
    let mut input = serde_json::json!({"identity":identity,"attempt":attempt,
        "original":{"author_id":7},"payload":{"frame":b_frame}});
    history::v2::Registry::runtime()
        .unwrap()
        .prepare("rival", &input)
        .unwrap();
    if case == "abandoned" {
        input["phase"] = "reserved".into();
        let not_sent = serde_json::json!({"namespace":"rival","head_injection":null,
            "retained_inputs":[{"input":input,"disposition":"known_not_sent"}],
            "terminal_identities":[]});
        history::v2::Registry::runtime()
            .unwrap()
            .project(&not_sent)
            .unwrap();
    }
    let key = serde_json::json!({"identity":identity,"attempt":attempt});
    let receipt = || {
        history::v2::Registry::runtime().unwrap().receipt(
            "rival",
            &key,
            (&path, start, "codex", Some("row-r"), &b_frame),
        )
    };
    let round = || {
        let (a, b) = if case == "receipt_first" {
            let b = receipt();
            (bind(), b)
        } else {
            (bind(), receipt())
        };
        let known = matches!(
            history::observed_attempt("codex", &tmux, std::slice::from_ref(&framed), Some("row-r")),
            history::ObservedAttempt::Known(_)
        );
        (a, b, known)
    };
    let first = round();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .injected_steer_by_tmux
        .remove(&PromptKey::new("codex", &tmux));
    state
        .native_turn_by_tmux
        .remove(&PromptKey::new("codex", &tmux));
    drop(state);
    [first, round()]
}

/// A migrated append and a same-frame V2 attempt leave their row unowned in either order and after
/// a restart; another frame, an owner's re-read or an abandoned rival still bind.
#[cfg(unix)]
#[test]
fn a_migrated_append_and_a_same_frame_v2_attempt_leave_their_row_unowned() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let a = Some("rrrr7504".to_string());
    for case in ["receipt_first", "bind_first"] {
        let unowned = (None, None, false);
        assert_eq!(migrated_rival(case), [unowned.clone(), unowned], "{case}");
    }
    for case in ["other_frame", "owned", "abandoned"] {
        assert_eq!(
            migrated_rival(case),
            [(a.clone(), None, true), (a.clone(), None, true)],
            "{case}"
        );
    }
}
