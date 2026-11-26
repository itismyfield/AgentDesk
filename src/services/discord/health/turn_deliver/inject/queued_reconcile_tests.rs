//! The proof a head's transcript must hold: one exact enqueue of its frame under one nonce, then the
//! absorbed removal and the human prompt command naming the same ids, in one session.

// Successful reconciliation needs the Unix device/inode identity of the bound transcript.
#![cfg(unix)]
use super::*;
use serde_json::{Value, json};
use std::io::Write;

const CHANNEL: u64 = 6_813_460;
const HEAD: u64 = 9_100_094_081_725_135_881;
const NONCE: &str = "d286a43b";
const SESSION: &str = "47d7dbe8-0c1a-4b2e-9f3d-6a5b4c3d2e1f";
const OTHER: &str = "7d3e9a10-2c4b-4f6e-8a1d-5b9c0e2f4a6d";
/// The source's enqueue clock: 2026-10-14T19:42:03Z.
const CLOCK_US: i64 = 1_792_006_923_000_000;
const AFTER: &str = "2026-10-14T20:31:14.363Z";
const BEFORE: &str = "2026-10-14T19:00:00.000Z";

fn original(text: &str, author: u64) -> PendingQueueItem {
    serde_json::from_value(json!({
        "author_id": author, "message_id": HEAD, "queued_generation": 290,
        "source_message_ids": [HEAD], "text": text, "channel_id": CHANNEL,
        "source_message_queued_generations": [
            {"message_id": HEAD, "queued_generation": 290, "enqueued_at_epoch_us": CLOCK_US}],
    }))
    .unwrap()
}

fn identity(sources: &[u64]) -> QueueIdentity {
    let sources: Vec<Value> = (sources.iter())
        .map(|id| json!({"message_id": id, "queued_generation": 290}))
        .collect();
    serde_json::from_value(json!({"primary": HEAD, "queued_generation": 290, "sources": sources}))
        .unwrap()
}

fn wrap(frame: String, wrapped: bool) -> String {
    match wrapped {
        true => format!("<pasted_content id=\"b0ac\">\n{frame}\n</pasted_content id=\"b0ac\">"),
        false => frame,
    }
}

fn prompt(nonce: &str, wrapped: bool) -> String {
    wrap(frame(nonce), wrapped)
}

fn frame(nonce: &str) -> String {
    human_frame::plain_inject("", "200", "ann", nonce, "status?")
}

/// Claude's records of an absorbed queued paste: enqueue, removal and human prompt command.
fn chain(nonce: &str, stamp: &str) -> [Value; 3] {
    chain_of(prompt(nonce, true), nonce, stamp)
}

fn chain_of(content: String, nonce: &str, stamp: &str) -> [Value; 3] {
    let ids = (format!("cmd-{nonce}"), format!("del-{nonce}"));
    [
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": stamp,
            "sessionId": SESSION, "content": content}),
        json!({"type": "queue-operation", "operation": "remove", "timestamp": stamp,
            "sessionId": SESSION, "content": content, "reason": "absorbed_mid_turn",
            "commandUuid": ids.0, "deliveryId": ids.1}),
        json!({"type": "attachment", "timestamp": stamp, "sessionId": SESSION,
            "attachment": {"type": "queued_command", "prompt": content, "source_uuid": ids.0,
            "delivery_id": ids.1, "commandMode": "prompt", "origin": {"kind": "human"},
            "humanTurn": true}}),
    ]
}

struct Transcript {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Transcript {
    fn new(records: &[Value]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{SESSION}.jsonl"));
        let transcript = Self { _dir: dir, path };
        transcript.write("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
        for record in records {
            transcript.write(&format!("{record}\n"));
        }
        transcript
    }

    fn write(&self, text: &str) {
        let mut options = std::fs::OpenOptions::new();
        let mut file = options.create(true).append(true).open(&self.path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn bound(&self, session_id: &str) -> Bound {
        Bound {
            session: "AgentDesk-claude-test".into(),
            path: self.path.clone(),
            file: file_id(&std::fs::metadata(&self.path).unwrap()).unwrap(),
            session_id: session_id.to_string(),
        }
    }

    fn want(&self, sources: &[u64], text: &str, recorded: Option<Recorded>) -> Option<Want> {
        let bound = self.bound(SESSION);
        Want::of(&identity(sources), &original(text, 200), recorded, &bound)
    }

    /// The nonce the head's paste, registered and recorded from the file's start, is proven taken
    /// under, if any.
    fn proven(&self) -> Option<String> {
        let paste = frame(NONCE);
        if !human_frame::history::registered(NONCE, &self.path, &paste) {
            let who = ("claude", "AgentDesk-claude-test", NONCE, "200");
            human_frame::history::register(who, &self.path, 0, &paste).unwrap();
        }
        let recorded = Some(recorded(self, NONCE, 0));
        Some(self.want(&[HEAD], "status?", recorded)?.search()?.nonce)
    }
}

fn recorded(transcript: &Transcript, nonce: &str, offset: u64) -> Recorded {
    let bound = transcript.bound(SESSION);
    Recorded {
        nonce: nonce.into(),
        transcript: bound.path.display().to_string(),
        session_id: SESSION.into(),
        file: bound.file,
        offset,
    }
}

fn edit(mut record: Value, path: &[&str], value: Option<Value>) -> Value {
    let (last, parents) = path.split_last().unwrap();
    let mut at = &mut record;
    for key in parents {
        at = &mut at[*key];
    }
    match value {
        Some(value) => at[*last] = value,
        None => drop(at.as_object_mut().unwrap().remove(*last)),
    }
    record
}

#[test]
fn a_recorded_head_needs_the_whole_absorbed_chain() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let [enqueue, remove, command] = chain(NONCE, AFTER);
    let ok = |records: Vec<Value>| Transcript::new(&records).proven();
    assert_eq!(
        ok(vec![enqueue.clone(), remove.clone(), command.clone()]).as_deref(),
        Some(NONCE)
    );
    let bare =
        |record: &Value, key: &[&str]| edit(record.clone(), key, Some(json!(prompt(NONCE, false))));
    let bare_chain = vec![
        bare(&enqueue, &["content"]),
        bare(&remove, &["content"]),
        bare(&command, &["attachment", "prompt"]),
    ];
    assert_eq!(
        ok(bare_chain).as_deref(),
        Some(NONCE),
        "an unwrapped frame proves it too"
    );
    let older = chain("0f0f0f0f", BEFORE);
    let earlier_paste = [
        older.to_vec(),
        vec![enqueue.clone(), remove.clone(), command.clone()],
    ];
    assert_eq!(
        ok(earlier_paste.concat()).as_deref(),
        Some(NONCE),
        "another attempt's paste is not a rival"
    );

    let field =
        |record: &Value, path: &[&str], value: Option<Value>| edit(record.clone(), path, value);
    let assistant = json!({"type": "assistant", "sessionId": SESSION,
        "message": {"content": prompt(NONCE, true)}});
    let cases: Vec<(&str, Vec<Value>)> = vec![
        ("enqueue only", vec![enqueue.clone()]),
        ("no human command", vec![enqueue.clone(), remove.clone()]),
        (
            "no absorbed removal",
            vec![enqueue.clone(), command.clone()],
        ),
        (
            "removal before the enqueue",
            vec![remove.clone(), enqueue.clone(), command.clone()],
        ),
        (
            "enqueued twice",
            vec![
                enqueue.clone(),
                enqueue.clone(),
                remove.clone(),
                command.clone(),
            ],
        ),
        (
            "another reason",
            vec![
                enqueue.clone(),
                field(&remove, &["reason"], Some(json!("cancelled"))),
                command.clone(),
            ],
        ),
        (
            "not a human turn",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(&command, &["attachment", "humanTurn"], Some(json!(false))),
            ],
        ),
        (
            "human turn unstated",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(&command, &["attachment", "humanTurn"], None),
            ],
        ),
        (
            "not human",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(
                    &command,
                    &["attachment", "origin", "kind"],
                    Some(json!("system")),
                ),
            ],
        ),
        (
            "mode unstated",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(&command, &["attachment", "commandMode"], None),
            ],
        ),
        (
            "bash mode",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(
                    &command,
                    &["attachment", "commandMode"],
                    Some(json!("bash")),
                ),
            ],
        ),
        (
            "other command id",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(&command, &["attachment", "source_uuid"], Some(json!("x"))),
            ],
        ),
        (
            "other delivery id",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(&command, &["attachment", "delivery_id"], Some(json!("x"))),
            ],
        ),
        (
            "other session",
            vec![
                enqueue.clone(),
                remove.clone(),
                field(&command, &["sessionId"], Some(json!("other"))),
            ],
        ),
        (
            "no session",
            vec![
                field(&enqueue, &["sessionId"], None),
                remove.clone(),
                command.clone(),
            ],
        ),
        (
            "a suffix in the block",
            vec![
                field(
                    &enqueue,
                    &["content"],
                    Some(json!(prompt(NONCE, true).replace("status?", "status?\nX"))),
                ),
                remove.clone(),
                command.clone(),
            ],
        ),
        (
            "a non-text prompt",
            vec![
                field(
                    &enqueue,
                    &["content"],
                    Some(json!([{"type": "text", "text": prompt(NONCE, true)}])),
                ),
                remove.clone(),
                command.clone(),
            ],
        ),
        (
            "quoted by the assistant",
            vec![assistant, remove.clone(), command.clone()],
        ),
    ];
    for (case, records) in cases {
        assert_eq!(ok(records), None, "{case}");
    }
}

#[test]
fn a_recorded_head_is_proven_only_for_its_own_text_author_and_session() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = Transcript::new(&chain(NONCE, AFTER));
    assert_eq!(transcript.proven().as_deref(), Some(NONCE));
    let at_start = || Some(recorded(&transcript, NONCE, 0));
    let other_text = transcript.want(&[HEAD], "status!", at_start()).unwrap();
    assert!(other_text.search().is_none(), "another body");
    let bound = transcript.bound(SESSION);
    let other_author = Want::of(
        &identity(&[HEAD]),
        &original("status?", 201),
        at_start(),
        &bound,
    );
    assert!(other_author.unwrap().search().is_none(), "another author");
    let named = transcript.bound(OTHER);
    let wrong = Want::of(
        &identity(&[HEAD]),
        &original("status?", 200),
        at_start(),
        &named,
    );
    assert!(
        wrong.is_none(),
        "the transcript is bound for another native session"
    );
}

#[test]
fn a_recorded_paste_is_proven_by_its_own_nonce_after_its_offset_in_its_own_file() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = Transcript::new(&[]);
    let offset = std::fs::metadata(&transcript.path).unwrap().len();
    let who = ("claude", "AgentDesk-claude-test", NONCE, "200");
    human_frame::history::register(who, &transcript.path, offset, &frame(NONCE)).unwrap();
    // The fake pane's clock is older than the queue's; a recorded nonce needs no clock.
    for record in chain(NONCE, BEFORE) {
        transcript.write(&format!("{record}\n"));
    }
    let proven = |recorded| {
        transcript
            .want(&[HEAD], "status?", Some(recorded))?
            .search()
    };
    let found = proven(recorded(&transcript, NONCE, offset)).map(|proof| proof.nonce);
    assert_eq!(found.as_deref(), Some(NONCE));
    assert!(
        proven(recorded(&transcript, "0f0f0f0f", offset)).is_none(),
        "another nonce"
    );
    let len = std::fs::metadata(&transcript.path).unwrap().len();
    assert!(
        proven(recorded(&transcript, NONCE, len)).is_none(),
        "records before the paste"
    );
    let mut elsewhere = recorded(&transcript, NONCE, offset);
    elsewhere.transcript = "/elsewhere.jsonl".into();
    assert!(
        transcript
            .want(&[HEAD], "status?", Some(elsewhere))
            .is_none()
    );
    let mut replaced = recorded(&transcript, NONCE, offset);
    replaced.file = (0, 0);
    assert!(
        transcript
            .want(&[HEAD], "status?", Some(replaced))
            .is_none()
    );
    let mut other_session = recorded(&transcript, NONCE, offset);
    other_session.session_id = OTHER.into();
    assert!(
        transcript
            .want(&[HEAD], "status?", Some(other_session))
            .is_none()
    );
}

#[test]
fn a_record_still_being_written_or_a_replaced_file_proves_nothing_yet() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let [enqueue, remove, command] = chain(NONCE, AFTER);
    let transcript = Transcript::new(&[enqueue, remove]);
    assert_eq!(transcript.proven(), None);
    let line = command.to_string();
    transcript.write(&line);
    assert_eq!(
        transcript.proven(),
        None,
        "the command line has no newline yet"
    );
    transcript.write("\n");
    assert_eq!(
        transcript.proven().as_deref(),
        Some(NONCE),
        "found once it is complete"
    );

    let at_start = || Some(recorded(&transcript, NONCE, 0));
    let replaced = transcript.want(&[HEAD], "status?", at_start()).unwrap();
    let missing = transcript.want(&[HEAD], "status?", at_start()).unwrap();
    let copy = transcript.path.with_extension("copy");
    std::fs::copy(&transcript.path, &copy).unwrap();
    std::fs::rename(&copy, &transcript.path).unwrap();
    assert!(replaced.search().is_none(), "another file at the same path");
    std::fs::remove_file(&transcript.path).unwrap();
    assert!(missing.search().is_none(), "no file");
}

#[test]
fn a_command_counts_only_after_its_removal_in_file_order() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let [enqueue, remove, command] = chain(NONCE, AFTER);
    let ok = |records: Vec<Value>| Transcript::new(&records).proven();
    let id = |key: &str| {
        edit(
            command.clone(),
            &["attachment", key],
            Some(json!("other-id")),
        )
    };
    assert_eq!(
        ok(vec![enqueue.clone(), command.clone(), remove.clone()]),
        None,
        "the command came before its removal"
    );
    assert_eq!(
        ok(vec![enqueue.clone(), remove.clone(), command.clone()]).as_deref(),
        Some(NONCE)
    );
    let stray_first = vec![
        enqueue.clone(),
        command.clone(),
        remove.clone(),
        command.clone(),
    ];
    assert_eq!(
        ok(stray_first).as_deref(),
        Some(NONCE),
        "a later removal and command still prove it"
    );
    for key in ["source_uuid", "delivery_id"] {
        let records = vec![enqueue.clone(), remove.clone(), id(key)];
        assert_eq!(ok(records), None, "the command names another {key}");
    }
}

#[test]
fn the_proof_session_is_the_one_the_transcript_file_is_named_for() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let named = |name: &str, bound: Option<&str>| {
        transcript_session(&std::path::Path::new("/p").join(name), bound)
    };
    let file = format!("{SESSION}.jsonl");
    assert_eq!(named(&file, None).as_deref(), Some(SESSION));
    assert_eq!(named(&file, Some(SESSION)).as_deref(), Some(SESSION));
    assert_eq!(named(&file, Some(OTHER)), None, "the binding names another");
    assert_eq!(
        named("transcript.jsonl", None),
        None,
        "not named for a session"
    );
    assert_eq!(
        named(&format!("{SESSION}.json"), None),
        None,
        "not a transcript"
    );

    let other: Vec<Value> = (chain(NONCE, AFTER).into_iter())
        .map(|record| edit(record, &["sessionId"], Some(json!(OTHER))))
        .collect();
    assert_eq!(
        Transcript::new(&other).proven(),
        None,
        "records of another session in this session's file"
    );
}

#[test]
fn a_recorded_plain_frame_is_proven_only_with_its_durable_identity() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tagged = human_frame::plain_inject("discord", "200", "ann", NONCE, "status?");
    let untagged = human_frame::idle("ann", "200", "status?");
    for (wrapped, frame) in [false, true]
        .into_iter()
        .flat_map(|w| [(w, &tagged), (w, &untagged)])
    {
        let proves = *frame == tagged;
        let transcript = Transcript::new(&[]);
        let offset = std::fs::metadata(&transcript.path).unwrap().len();
        let frame = frame.clone();
        assert!(!frame.contains(&format!("discord:{CHANNEL}:{HEAD}")));
        human_frame::history::register(
            ("claude", "AgentDesk-claude-test", NONCE, "200"),
            &transcript.path,
            offset,
            &frame,
        )
        .unwrap();
        for record in chain_of(wrap(frame, wrapped), NONCE, BEFORE) {
            transcript.write(&format!("{record}\n"));
        }
        let proof = transcript
            .want(
                &[HEAD],
                "status?",
                Some(recorded(&transcript, NONCE, offset)),
            )
            .unwrap()
            .search();
        // An attempt pastes its tagged frame; an untagged one is no attempt's paste.
        assert_eq!(
            proof.map(|p| (p.nonce, p.offset)),
            proves.then(|| (NONCE.into(), offset))
        );
        assert!(
            transcript
                .want(
                    &[HEAD],
                    "status?",
                    Some(recorded(&transcript, "0f0f0f0f", offset)),
                )
                .unwrap()
                .search()
                .is_none(),
            "a different hidden identity cannot claim the same bytes"
        );
    }
}

/// A V2 attempt input for this head's owner, sourced in `transcript` or not yet.
fn v2_input(nonce: &str, frame: &str, transcript: Option<&Transcript>) -> Value {
    let source = transcript.map_or(json!({"kind": "pre_source"}), |t| {
        json!({"kind": "existing", "session_id": SESSION, "transcript": t.path,
            "file": t.bound(SESSION).file, "offset": 0})
    });
    json!({"identity": {"primary": HEAD, "queued_generation": 7,
            "sources": [{"message_id": HEAD, "queued_generation": 7}]},
        "attempt": {"nonce": nonce, "provider": "claude", "source": source,
            "target": {"owner": "AgentDesk-claude-test", "execution_nonce": "spawn"}},
        "original": {"author_id": 200}, "payload": {"frame": frame}})
}

/// A recorded attempt's proof survives its history becoming V2 or V3, live or passive; abandoned
/// evidence, another body or a second attempt of the same bytes proves nothing.
#[test]
fn a_recorded_proof_survives_the_history_moving_to_v2() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = Transcript::new(&chain(NONCE, AFTER));
    assert_eq!(transcript.proven().as_deref(), Some(NONCE));
    let proof = |t: &Transcript, text: &str| {
        let want = t.want(&[HEAD], text, Some(recorded(t, NONCE, 0)))?;
        want.search().map(|proof| (proof.nonce, proof.offset))
    };
    // The V1 proof names the enqueue line, which the fixture writes after its opening user row.
    let proven = proof(&transcript, "status?");
    let written = std::fs::read_to_string(&transcript.path).unwrap();
    let enqueue_at = written
        .find(&format!("{}\n", chain(NONCE, AFTER)[0]))
        .unwrap() as u64;
    assert_ne!(enqueue_at, 0);
    assert_eq!(proven, Some((NONCE.to_string(), enqueue_at)));
    let history = human_frame::history::v2::Registry::runtime().unwrap();
    // Another input's V2 owner moves the V1 registration into passive evidence.
    history
        .prepare("reconcile-test", &v2_input("0f0f0f0f", "other input", None))
        .unwrap();
    let file = crate::config::runtime_root()
        .unwrap()
        .join("runtime/injected_frames.json");
    let v2 = std::fs::read_to_string(&file).unwrap();
    assert!(v2.starts_with("{\"version\":2,") && v2.contains("\"state\":\"retained\""));
    assert_eq!(proof(&transcript, "status?"), proven, "passive evidence");
    assert_eq!(proof(&transcript, "status!"), None, "another body");
    std::fs::write(&file, v2.replacen("{\"version\":2,", "{\"version\":3,", 1)).unwrap();
    assert_eq!(proof(&transcript, "status?"), proven, "V3");
    let abandoned = v2.replace("\"state\":\"retained\"", "\"state\":\"abandoned\"");
    std::fs::write(&file, abandoned).unwrap();
    assert_eq!(proof(&transcript, "status?"), None, "abandoned evidence");
    std::fs::write(&file, &v2).unwrap();
    let twin = v2_input("0e0e0e0e", &frame(NONCE), Some(&transcript));
    history.prepare("reconcile-test", &twin).unwrap();
    assert_eq!(
        proof(&transcript, "status?"),
        None,
        "a second attempt of the bytes"
    );
    let live = Transcript::new(&chain(NONCE, AFTER));
    let own = v2_input(NONCE, &frame(NONCE), Some(&live));
    history.prepare("reconcile-test", &own).unwrap();
    assert_eq!(proof(&live, "status?"), proven, "a live V2 registration");
}
