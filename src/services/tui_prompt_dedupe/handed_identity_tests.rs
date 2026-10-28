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
