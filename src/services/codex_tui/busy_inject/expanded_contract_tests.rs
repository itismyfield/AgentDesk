use super::*;

fn composer(fake: &Fake, draft: &str, queued: bool, images: usize) {
    let state = serde_json::json!({"text": draft, "cursor": draft.chars().count(),
        "queued": queued, "images": images, "accepted": []});
    fs::write(fake.dir.path().join("composer.json"), state.to_string()).unwrap();
    let base = fake.dir.path().join("tmux-base");
    fs::rename(fake.dir.path().join("tmux"), &base).unwrap();
    let program = format!(
        r#"#!/bin/sh
d='{}'
"$d/tmux-base" "$@" || exit $?
python3 - "$d" "$2" "$@" <<'PY'
import json, pathlib, sys
d = pathlib.Path(sys.argv[1])
p = d / 'composer.json'
s = json.loads(p.read_text())
if sys.argv[2] == 'paste-buffer':
    text = (d / 'buffer').read_text()
    at = s['cursor']
    if s.get('selection'):
        at, end = s['selection']
        s['text'] = s['text'][:at] + s['text'][end:]
        s['selection'] = None
    s['text'] = s['text'][:at] + text + s['text'][at:]
    s['cursor'] = at + len(text)
elif sys.argv[2] == 'send-keys' and sys.argv[-1].endswith('Enter'):
    s['accepted'].append({{'text': s['text'], 'images': s['images'], 'queued': s['queued']}})
    s['text'] = ''
    s['cursor'] = 0
    s['images'] = 0
p.write_text(json.dumps(s))
PY
"#,
        fake.dir.path().display()
    );
    let path = fake.dir.path().join("tmux");
    fs::write(&path, program).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn accepted(fake: &Fake) -> Vec<serde_json::Value> {
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fake.dir.path().join("composer.json")).unwrap()).unwrap();
    state["accepted"].as_array().unwrap().clone()
}

#[test]
fn an_open_turn_nine_megabytes_back_is_injectable() {
    let padding = item(T, "AgentMessage", &"x".repeat(1024));
    let rollout = model(T) + &padding.repeat(9 * 1024 * 1024 / padding.len() + 1);
    assert_eq!(verdict(&rollout), steerable(T));
}

#[test]
fn unproven_draft_preserves_text_and_images_for_the_existing_queue() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for images in [0, 1] {
        let fake = Fake::new();
        let draft = "쓰다 만 초안\n두 번째 줄 ";
        let shown = format!(
            "\n› {}\n\n  90% context left\n",
            draft.replace('\n', "\n  ")
        );
        let shown = if images > 0 {
            with_image(&shown)
        } else {
            shown
        };
        fake.answers("cap", &[&shown]);
        composer(&fake, draft, false, images);
        assert_eq!(
            fake.run(&FakeLedger::default()),
            Outcome::NotSent(Veto::DraftNavigationUnproven)
        );
        assert!(accepted(&fake).is_empty());
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(fake.dir.path().join("composer.json")).unwrap())
                .unwrap();
        assert_eq!(state["text"], draft);
        assert_eq!(state["images"], images);
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (0, 0));
    }
}

#[test]
fn native_queue_acceptance_succeeds_once_without_a_user_receipt() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    fake.caps(&["busy_queued.ansi"]);
    composer(&fake, "", true, 0);
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert_eq!(
        accepted(&fake),
        vec![serde_json::json!({
        "text": frame_text(), "images": 0, "queued": true})]
    );
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
    assert_eq!(rollout::submitted(&fake.rollout, 0, NONCE), None);
}

#[test]
fn a_manual_compact_turn_accepts_input_without_a_user_message() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    let rollout =
        records("compact_turn.jsonl", 6).replace("01a11dcc-3da7-7a23-9f47-5067834fa221", T);
    fs::write(&fake.rollout, rollout).unwrap();
    fake.caps(&["busy_empty.ansi"]);
    composer(&fake, "", true, 0);
    assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
    assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
    assert_eq!(accepted(&fake).len(), 1);
}

#[test]
fn command_transform_and_selection_drafts_never_touch_the_payload() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for (draft, reason) in [
        ("/status\n메모", Veto::DraftCommandUnproven),
        ("/compact\n메모", Veto::DraftCommandUnproven),
        ("> 인용문", Veto::DraftPasteTransformUnproven),
        (
            "[Pasted text #1 +100 lines]",
            Veto::DraftPlaceholderUnproven,
        ),
        ("\x1b[7m \x1b[0m", Veto::DraftSelectionUnproven),
        ("\x1b[7m\n \x1b[0m", Veto::DraftSelectionUnproven),
        (
            "known text but unknown custom/Vim bindings",
            Veto::DraftNavigationUnproven,
        ),
    ] {
        let fake = Fake::new();
        let capture = format!(
            "\n› {}\n\n  90% context left\n",
            draft.replace('\n', "\n  ")
        );
        fake.answers("cap", &[&capture]);
        composer(&fake, draft, false, 0);
        assert_eq!(
            fake.run(&FakeLedger::default()),
            Outcome::NotSent(reason),
            "{draft:?}"
        );
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (0, 0));
        assert!(accepted(&fake).is_empty());
    }
}

#[test]
fn physical_composer_rows_have_explicit_39_40_41_boundaries() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for rows in [39, 40, 41] {
        let fake = Fake::new();
        fs::write(fake.dir.path().join("meta.json"), r#"{"height":50}"#).unwrap();
        let draft = vec!["row"; rows].join("\n");
        let capture = format!(
            "\n› {}\n\n  90% context left\n",
            draft.replace('\n', "\n  ")
        );
        fake.answers("cap", &[&capture]);
        let expected = if rows > 40 {
            Veto::DraftNotFullyVisible
        } else {
            Veto::DraftNavigationUnproven
        };
        assert_eq!(fake.run(&FakeLedger::default()), Outcome::NotSent(expected));
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (0, 0));
    }
}

#[test]
fn completion_or_pane_replacement_during_preparation_cannot_open_a_new_turn() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for replaced in [false, true] {
        let fake = Fake::new();
        if replaced {
            fake.flag("replace_pane");
        } else {
            fs::write(
                fake.dir.path().join("on_load"),
                ended("task_complete", Some(T)),
            )
            .unwrap();
        }
        let ledger = FakeLedger::default();
        let expected = if replaced {
            Veto::PaneUnavailable
        } else {
            Veto::NotBusy
        };
        assert_eq!(fake.run(&ledger), Outcome::NotSent(expected));
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (0, 0));
        assert_eq!(ledger.withdrawn.get(), 1);
    }
}

#[test]
fn native_command_and_quote_snapshots_use_the_production_refusal_path() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for (capture, reason) in [
        ("native_draft_status.ansi", Veto::DraftCommandUnproven),
        ("native_draft_compact.ansi", Veto::DraftCommandUnproven),
        ("native_draft_quote.ansi", Veto::DraftPasteTransformUnproven),
    ] {
        let fake = Fake::new();
        fake.caps(&[capture]);
        assert_eq!(fake.run(&FakeLedger::default()), Outcome::NotSent(reason));
        assert_eq!(
            (
                fake.calls("load-buffer"),
                fake.calls("paste-buffer"),
                fake.enters()
            ),
            (0, 0, 0)
        );
    }
    let capture = "Messages to be submitted after next tool call (press esc to interrupt and send immediately)\n  ↳ frame\n\n› \n\n  90% context left\n";
    assert!(screen::read(capture).queued);
}

fn preparation_composer_change(case: &str) {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    fake.caps(&["busy_empty.ansi"]);
    composer(&fake, "", false, 0);
    let mut changed =
        serde_json::json!({"text":"", "cursor":0,"queued":false,"images":0,"accepted":[]});
    let (capture, veto) = match case {
        "text" => {
            changed["text"] = "사람이 준비 중 쓴 초안".into();
            (
                "\n› 사람이 준비 중 쓴 초안\n\n  90% context left\n".to_string(),
                Veto::DraftNavigationUnproven,
            )
        }
        "spaces" => {
            changed["text"] = "  ".into();
            (
                "\n›   \n\n  90% context left\n".to_string(),
                Veto::DraftNavigationUnproven,
            )
        }
        "selected" => {
            changed["text"] = "선택한 초안".into();
            changed["selection"] = serde_json::json!([0, 6]);
            (
                "\n› \x1b[7m선택한 초안\x1b[0m\n\n  90% context left\n".to_string(),
                Veto::DraftSelectionUnproven,
            )
        }
        "command" => {
            changed["text"] = "/compact\n메모".into();
            (
                "\n› /compact\n  메모\n\n  90% context left\n".to_string(),
                Veto::DraftCommandUnproven,
            )
        }
        "image" => {
            changed["images"] = 1.into();
            (
                with_image(&fixture("busy_empty.ansi")),
                Veto::DraftImagePositionUnproven,
            )
        }
        "modal" => (fixture("overlay.ansi"), Veto::Modal),
        "resize" => {
            fs::write(fake.dir.path().join("meta.after.json"), r#"{"width":81}"#).unwrap();
            (fixture("busy_empty.ansi"), Veto::ComposerUnread)
        }
        _ => unreachable!(),
    };
    fs::write(fake.dir.path().join("cap.after"), &capture).unwrap();
    fs::write(
        fake.dir.path().join("composer.after.json"),
        changed.to_string(),
    )
    .unwrap();
    let ledger = FakeLedger::default();
    assert_eq!(fake.run(&ledger), Outcome::NotSent(veto), "{case}");
    assert_eq!(
        (fake.calls("paste-buffer"), fake.enters()),
        (0, 0),
        "{case}"
    );
    assert_eq!(
        (ledger.registered.get(), ledger.withdrawn.get()),
        (1, 1),
        "{case}"
    );
    assert_eq!(fake.calls("delete-buffer"), 1);
    let preserved: serde_json::Value =
        serde_json::from_slice(&fs::read(fake.dir.path().join("composer.json")).unwrap()).unwrap();
    assert_eq!(preserved, changed, "{case}");
    assert!(
        crate::services::tui_input::human_frame::history::append::projection(
            "codex",
            &fake.session(),
            NONCE
        )
        .is_none(),
        "reserved entry must be withdrawn"
    );
}

#[test]
fn final_composer_rechecks_text_changed_during_preparation() {
    preparation_composer_change("text");
}
#[test]
fn final_composer_rechecks_selection_changed_during_preparation() {
    preparation_composer_change("selected");
}
#[test]
fn final_composer_rechecks_command_changed_during_preparation() {
    preparation_composer_change("command");
}
#[test]
fn final_composer_rechecks_image_changed_during_preparation() {
    preparation_composer_change("image");
}
#[test]
fn final_composer_rechecks_modal_changed_during_preparation() {
    preparation_composer_change("modal");
}
#[test]
fn final_composer_rechecks_resize_changed_during_preparation() {
    preparation_composer_change("resize");
}

#[test]
fn final_composer_allows_transcript_activity_and_native_queue_without_extra_enter() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for queued in [false, true] {
        let fake = Fake::new();
        let before = fixture(if queued {
            "busy_queued.ansi"
        } else {
            "busy_empty.ansi"
        });
        fake.answers("cap", &[&before]);
        fs::write(
            fake.dir.path().join("cap.after"),
            before.replace("Working", "\x1b[7mFurther model output\x1b[0m\nWorking"),
        )
        .unwrap();
        composer(&fake, "", queued, 0);
        assert_eq!(fake.run(&FakeLedger::default()), Outcome::Injected);
        assert_eq!((fake.calls("paste-buffer"), fake.enters()), (1, 1));
        assert_eq!(accepted(&fake).len(), 1);
    }
}

#[test]
fn final_composer_checks_native_viewport_cursor_samples() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for name in [
        "native_queue_empty_viewport.json",
        "native_steer_empty_viewport.json",
    ] {
        let sample: serde_json::Value = serde_json::from_str(&fixture(name)).unwrap();
        let fake = Fake::new();
        fake.answers("cap", &[sample["capture"].as_str().unwrap()]);
        fs::write(
            fake.dir.path().join("meta.json"),
            sample["pane"].to_string(),
        )
        .unwrap();
        composer(&fake, "", true, 0);
        assert_eq!(
            fake.run(&FakeLedger::default()),
            Outcome::Injected,
            "{name}"
        );
        assert_eq!(
            (
                fake.calls("capture-pane"),
                fake.calls("paste-buffer"),
                fake.enters()
            ),
            (2, 1, 1)
        );
        assert_eq!(accepted(&fake).len(), 1);
        let log = fs::read_to_string(fake.dir.path().join("log")).unwrap();
        assert!(
            !log.lines()
                .filter(|line| line.contains("capture-pane"))
                .any(|line| line.contains(" -S "))
        );
    }
}

#[test]
fn unproven_empty_cursor_mode_or_geometry_never_reaches_the_buffer() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for meta in [
        serde_json::json!({"x":3}),
        serde_json::json!({"y":0}),
        serde_json::json!({"mode":1}),
        serde_json::json!({"width":0}),
        serde_json::json!({"height":0}),
        serde_json::json!({"pid":0}),
    ] {
        let fake = Fake::new();
        fake.caps(&["busy_empty.ansi"]);
        fs::write(fake.dir.path().join("meta.json"), meta.to_string()).unwrap();
        let ledger = FakeLedger::default();
        assert_eq!(
            fake.run(&ledger),
            Outcome::NotSent(Veto::ComposerUnread),
            "{meta}"
        );
        assert_eq!(
            (
                fake.calls("load-buffer"),
                fake.calls("paste-buffer"),
                fake.enters(),
                ledger.registered.get()
            ),
            (0, 0, 0, 0)
        );
    }
}

#[test]
fn final_composer_rechecks_pane_incarnation_during_preparation() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let fake = Fake::new();
    fake.caps(&["busy_empty.ansi"]);
    fs::write(fake.dir.path().join("meta.after.json"), r#"{"pid":7505}"#).unwrap();
    let ledger = FakeLedger::default();
    assert_eq!(fake.run(&ledger), Outcome::NotSent(Veto::PaneUnavailable));
    assert_eq!(
        (
            fake.calls("paste-buffer"),
            fake.enters(),
            ledger.withdrawn.get()
        ),
        (0, 0, 1)
    );
    assert!(
        crate::services::tui_input::human_frame::history::append::projection(
            "codex",
            &fake.session(),
            NONCE
        )
        .is_none()
    );
}

#[test]
fn whitespace_at_the_start_is_still_a_draft_before_each_composer_proof() {
    {
        let _runtime = crate::config::TestRuntimeRootGuard::new();
        let fake = Fake::new();
        fake.answers("cap", &["\n›   \n\n  90% context left\n"]);
        composer(&fake, "  ", false, 0);
        let path = fake.dir.path().join("composer.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state["cursor"] = 0.into();
        fs::write(&path, state.to_string()).unwrap();
        let ledger = FakeLedger::default();
        assert_eq!(
            fake.run(&ledger),
            Outcome::NotSent(Veto::DraftNavigationUnproven)
        );
        assert_eq!(
            (
                fake.calls("paste-buffer"),
                fake.enters(),
                ledger.registered.get()
            ),
            (0, 0, 0)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap(),
            state
        );
        assert!(accepted(&fake).is_empty());
    }
    preparation_composer_change("spaces");
}
