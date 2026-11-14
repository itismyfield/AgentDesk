use super::native::{Disposition, Episode, Outcome, Ticket};
use super::*;

pub(crate) struct Fixture {
    pub(crate) history: Registry,
    pub(crate) ticket: Ticket,
    pub(crate) input: Value,
}

impl Fixture {
    pub(crate) fn at(path: PathBuf, channel: u64, primary: bool) -> Self {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let transcript = path.with_extension("jsonl");
        std::fs::write(&transcript, "native input\n").unwrap();
        let key = json!({"identity":{"primary":channel+1,"sources":[{"message_id":channel+1,"queued_generation":1}]},
            "attempt":{"nonce":format!("attempt-{channel}"),"provider":"claude",
                "target":{"host":"herdr","owner":"fixture-pane","execution_nonce":"incarnation-1"},
                "source":{"kind":"existing","session_id":"native","transcript":transcript,
                    "file":file_id(&std::fs::metadata(&transcript).unwrap()),"offset":0}}});
        let input = json!({"identity":key["identity"],"attempt":key["attempt"],
            "original":{"author_id":7},"payload":{"frame":"한국어 원본 입력"}});
        let history = Registry::at(path);
        history.prepare("native-fixture", &input).unwrap();
        let ticket = Ticket {
            key: key.clone(),
            channel_id: channel,
            source: Some(history.source("native-fixture", &key).unwrap()),
            extra: Default::default(),
            owning_episode: primary.then(|| Episode {
                user_message_id: channel + 1,
                turn_nonce: format!("episode-{channel}"),
                generation: 1,
                extra: Default::default(),
            }),
        };
        history.prepare_native("native-fixture", &ticket).unwrap();
        Self {
            history,
            ticket,
            input,
        }
    }
    pub(crate) fn outcome(disposition: Disposition) -> Outcome {
        Outcome {
            disposition,
            range: Some([0, 1]),
            extra: Default::default(),
        }
    }
}

#[test]
fn native_8148_v3_reader_keeps_follower_results_through_projection_and_compaction() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    for disposition in [Disposition::Executed, Disposition::WithdrawnNotExecuted] {
        let dir = test_dir(&runtime);
        let f = Fixture::at(dir.path().join("frames.json"), 8148001, false);
        let outcome = Fixture::outcome(disposition.clone());
        assert!(
            f.history
                .record_native_outcome("native-fixture", &f.ticket, &outcome)
                .unwrap()
        );
        assert!(
            !f.history
                .record_native_outcome("native-fixture", &f.ticket, &outcome)
                .unwrap()
        );
        let withdrawn = disposition == Disposition::WithdrawnNotExecuted;
        assert_eq!(
            f.history
                .ack_native_withdrawal_notice("native-fixture", &f.ticket)
                .unwrap(),
            withdrawn
        );
        assert!(
            !f.history
                .ack_native_withdrawal_notice("native-fixture", &f.ticket)
                .unwrap()
        );
        let mut future = f.history.json().unwrap();
        future["live"][0]["registration"]["native_settlement"]["future_proof"] =
            json!({"native_id":"run-1"});
        future["live"][0]["registration"]["native_settlement"]["warned"] = json!(true);
        std::fs::write(dir.path().join("frames.json"), future.to_string()).unwrap();
        f.history
            .project(&json!({"namespace":"native-fixture","head_injection":null,
            "retained_inputs":[{"input":f.input}],"terminal_identities":[]}))
            .unwrap();
        f.history
            .project(&json!({"namespace":"native-fixture","head_injection":null,
            "retained_inputs":[],"terminal_identities":[{"identity":f.ticket.key["identity"]}]}))
            .unwrap();
        let reopened = Registry::at(dir.path().join("frames.json"));
        let value = reopened.json().unwrap();
        assert_eq!(value["version"], 3);
        assert_eq!(value["passive"].as_array().unwrap().len(), 1);
        let settlement = &value["passive"][0]["registration"]["native_settlement"];
        assert_eq!(
            settlement["outcome"],
            serde_json::to_value(outcome).unwrap()
        );
        assert_eq!(settlement["withdrawal_notice"], withdrawn);
        assert_eq!(settlement["future_proof"], json!({"native_id":"run-1"}));
        assert_eq!(settlement["warned"], true);
        assert!(settlement["ticket"]["owning_episode"].is_null());
        let boot = native::boot_records(&dir.path().join("frames.json"), "claude").unwrap();
        assert_eq!(boot.len(), 1);
        assert_eq!(boot[0]["settlement"], *settlement);
        assert!(super::super::compat::read_legacy(&dir.path().join("frames.json")).is_err());
    }
}

#[test]
fn native_8148_full_ticket_cas_rejects_foreign_episode_source_and_missing_registration() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let dir = test_dir(&runtime);
    let f = Fixture::at(dir.path().join("frames.json"), 8148002, false);
    let before = std::fs::read(dir.path().join("frames.json")).unwrap();
    for axis in 0..4 {
        let mut wrong = f.ticket.clone();
        match axis {
            0 => {
                wrong.owning_episode = Some(Episode {
                    user_message_id: 8148003,
                    turn_nonce: "foreign".into(),
                    generation: 2,
                    extra: Default::default(),
                })
            }
            1 => wrong.source.as_mut().unwrap().target["execution_nonce"] = json!("incarnation-2"),
            2 => wrong.key["attempt"]["nonce"] = json!("different-attempt"),
            _ => wrong.channel_id += 1,
        }
        assert!(
            f.history
                .record_native_outcome(
                    "native-fixture",
                    &wrong,
                    &Fixture::outcome(Disposition::Executed)
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join("frames.json")).unwrap(),
            before
        );
    }
    assert!(
        f.history
            .record_native_outcome(
                "other-namespace",
                &f.ticket,
                &Fixture::outcome(Disposition::Executed)
            )
            .is_err()
    );
}

#[test]
fn native_8148_ordinary_registration_remains_v2_and_v3_is_not_a_legacy_writer() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let dir = test_dir(&runtime);
    let f = Fixture::at(dir.path().join("frames.json"), 8148003, true);
    let before = std::fs::read(dir.path().join("frames.json")).unwrap();
    assert!(super::super::compat::v2_prefix(&dir.path().join("frames.json")).unwrap());
    assert!(super::super::compat::read_legacy(&dir.path().join("frames.json")).is_err());
    assert_eq!(
        std::fs::read(dir.path().join("frames.json")).unwrap(),
        before
    );
    let ordinary = Registry::at(dir.path().join("ordinary.json"));
    ordinary.prepare("native-fixture", &f.input).unwrap();
    let value = ordinary.json().unwrap();
    assert_eq!(value["version"], 2);
    assert!(
        value["live"][0]["registration"]
            .get("native_settlement")
            .is_none()
    );
    assert!(
        native::boot_records(&dir.path().join("ordinary.json"), "claude")
            .unwrap()
            .is_empty()
    );
    assert!(
        native::boot_records(&dir.path().join("frames.json"), "codex")
            .unwrap()
            .is_empty()
    );
}

fn test_dir(runtime: &crate::config::TestRuntimeRootGuard) -> tempfile::TempDir {
    let root = runtime.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

#[test]
fn native_8148_boot_reader_exempts_only_decoded_historical_envelopes() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let dir = test_dir(&runtime);
    let path = dir.path().join("frames.json");
    for raw in [
        r#"{"entries":[]}"#,
        r#"{"version":2,"live":[],"passive":[]}"#,
    ] {
        std::fs::write(&path, raw).unwrap();
        assert!(native::boot_records(&path, "claude").unwrap().is_empty());
    }
    for raw in [r#"{"version":2,"live":7,"passive":[]}"#, "{broken"] {
        std::fs::write(&path, raw).unwrap();
        assert!(native::boot_records(&path, "claude").is_err());
    }
    std::fs::remove_file(&path).unwrap();
    let fixture = Fixture::at(path.clone(), 8148500, true);
    let mut value = fixture.history.json().unwrap();
    value["version"] = json!(2);
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(
        native::boot_records(&path, "claude").is_err(),
        "a native ticket cannot use the historical exception"
    );
}

#[test]
fn native_8148_boot_adopts_full_registration_without_reopening_a_live_slot() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let path = runtime.path().join("runtime/boot-adopt.json");
    let _fixture = Fixture::at(path.clone(), 81489951, true);
    let mut record = super::native::boot_records(&path, "claude")
        .unwrap()
        .remove(0);
    for location in ["settlement", "registration"] {
        let settlement = if location == "settlement" {
            &mut record[location]
        } else {
            &mut record[location]["native_settlement"]
        };
        settlement["ticket"]["opaque_ticket"] = json!({"preserve": [1,2,3]});
        settlement["opaque_settlement"] = json!({"preserve": "proof"});
        settlement["warned"] = json!(true);
    }
    let record = super::native::RestoredRecord::decode(record).unwrap();
    std::fs::remove_file(&path).unwrap();
    let restored = Registry::at(path.clone());
    assert_eq!(
        restored
            .adopt_native_records(std::slice::from_ref(&record))
            .unwrap()
            .len(),
        1
    );
    let expected = serde_json::to_value(&record).unwrap();
    let state = restored.json().unwrap();
    assert_eq!(
        state["passive"][0]["registration"],
        expected["registration"]
    );
    assert_eq!(state["passive"][0]["state"], "retained");
    assert!(state["planned"].as_array().unwrap().is_empty());
    assert_eq!(state["version"], 3);
    assert!(state["live"].as_array().unwrap().is_empty());
    let bytes = std::fs::read(&path).unwrap();
    restored
        .adopt_native_records(std::slice::from_ref(&record))
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    for mutation in ["namespace", "frame", "frame_author", "outcome", "extra"] {
        let mut bad = serde_json::to_value(&record).unwrap();
        match mutation {
            "namespace" => bad["namespace"] = json!("another-namespace"),
            "frame" => bad["registration"]["frame"]["class_hash"] = json!("another-frame"),
            "frame_author" => bad["registration"]["frame"]["author"] = json!("forged-author"),
            "outcome" => {
                bad["settlement"]["cleanup_complete"] = json!(true);
                bad["registration"]["native_settlement"]["cleanup_complete"] = json!(true);
            }
            _ => {
                bad["settlement"]["opaque_settlement"] = json!(false);
                bad["registration"]["native_settlement"]["opaque_settlement"] = json!(false);
            }
        }
        let rejected = super::native::RestoredRecord::decode(bad)
            .and_then(|bad| restored.adopt_native_records(&[bad]).map(drop));
        assert!(rejected.is_err(), "{mutation}");
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{mutation}");
    }
}

#[test]
fn native_8148_boot_stale_custody_cannot_roll_back_a_durable_result_or_ack() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let path = runtime.path().join("runtime/boot-monotonic.json");
    let f = Fixture::at(path.clone(), 81489952, false);
    let old = super::native::RestoredRecord::decode(
        super::native::boot_records(&path, "claude")
            .unwrap()
            .remove(0),
    )
    .unwrap();
    let outcome = Fixture::outcome(Disposition::Executed);
    f.history
        .record_native_outcome("native-fixture", &f.ticket, &outcome)
        .unwrap();
    f.history
        .ack_native_cleanup("native-fixture", &f.ticket, &outcome)
        .unwrap();
    f.history.project(&json!({"namespace":"native-fixture","head_injection":null,"retained_inputs":[],"terminal_identities":[{"identity":f.ticket.key["identity"]}]})).unwrap();
    let before = f
        .history
        .native_settlement("native-fixture", &f.ticket)
        .unwrap();
    let merged = f
        .history
        .adopt_native_records(std::slice::from_ref(&old))
        .unwrap();
    assert_eq!(merged[0].settlement, before);
    assert_eq!(
        f.history
            .native_settlement("native-fixture", &f.ticket)
            .unwrap(),
        before
    );
    let mut conflicting = serde_json::to_value(&merged[0]).unwrap();
    for place in ["settlement", "registration"] {
        let settlement = if place == "settlement" {
            &mut conflicting[place]
        } else {
            &mut conflicting[place]["native_settlement"]
        };
        settlement["outcome"]["disposition"] = json!("withdrawn_not_executed");
    }
    let conflict = super::native::RestoredRecord::decode(conflicting).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(f.history.adopt_native_records(&[conflict]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
