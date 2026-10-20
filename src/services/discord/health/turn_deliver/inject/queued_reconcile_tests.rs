//! The proof a head's transcript must hold: one exact enqueue of its frame under one nonce, then the
//! absorbed removal and the human prompt command naming the same ids, in one session.
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
    let source = format!("discord:{CHANNEL}:{HEAD}");
    wrap(
        human_frame::inject(&source, "200", "ann", nonce, "status?"),
        wrapped,
    )
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
        let channel = ChannelId::new(CHANNEL);
        let bound = self.bound(SESSION);
        Want::of(
            channel,
            &identity(sources),
            &original(text, 200),
            recorded,
            &bound,
        )
    }

    /// The nonce a head recorded before attempts were is proven taken under, if any.
    fn legacy(&self) -> Option<String> {
        Some(self.want(&[HEAD], "status?", None)?.search()?.nonce)
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
fn a_head_recorded_before_attempts_needs_the_whole_absorbed_chain_after_its_clock() {
    let [enqueue, remove, command] = chain(NONCE, AFTER);
    let ok = |records: Vec<Value>| Transcript::new(&records).legacy();
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
        "a paste before the clock is not a rival"
    );

    let field =
        |record: &Value, path: &[&str], value: Option<Value>| edit(record.clone(), path, value);
    let assistant = json!({"type": "assistant", "sessionId": SESSION,
        "message": {"content": prompt(NONCE, true)}});
    let rival = chain("1e1e1e1e", AFTER);
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
        ("before the clock", chain(NONCE, BEFORE).to_vec()),
        (
            "no stamp",
            vec![
                field(&enqueue, &["timestamp"], None),
                remove.clone(),
                command.clone(),
            ],
        ),
        (
            "two pastes after the clock",
            [
                rival.to_vec(),
                vec![enqueue.clone(), remove.clone(), command.clone()],
            ]
            .concat(),
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
fn a_legacy_head_is_proven_only_for_one_source_its_own_frame_and_its_clock() {
    let transcript = Transcript::new(&chain(NONCE, AFTER));
    assert!(
        transcript
            .want(&[HEAD, HEAD + 1], "status?", None)
            .is_none(),
        "a merged head"
    );
    let other_text = transcript.want(&[HEAD], "status!", None).unwrap();
    assert!(other_text.search().is_none(), "another body");
    let channel = ChannelId::new(CHANNEL);
    let bound = transcript.bound(SESSION);
    let other_author = Want::of(
        channel,
        &identity(&[HEAD]),
        &original("status?", 201),
        None,
        &bound,
    );
    assert!(other_author.unwrap().search().is_none(), "another author");
    let mut unclocked = original("status?", 200);
    unclocked.source_message_queued_generations[0].enqueued_at_epoch_us = None;
    assert!(Want::of(channel, &identity(&[HEAD]), &unclocked, None, &bound).is_none());
    let named = transcript.bound(OTHER);
    let wrong = Want::of(
        channel,
        &identity(&[HEAD]),
        &original("status?", 200),
        None,
        &named,
    );
    assert!(
        wrong.unwrap().search().is_none(),
        "the transcript is bound for another native session"
    );
    let named = transcript.bound(SESSION);
    let right = Want::of(
        channel,
        &identity(&[HEAD]),
        &original("status?", 200),
        None,
        &named,
    );
    assert_eq!(
        right.unwrap().search().map(|proof| proof.nonce).as_deref(),
        Some(NONCE)
    );
}

#[test]
fn a_recorded_paste_is_proven_by_its_own_nonce_after_its_offset_in_its_own_file() {
    let transcript = Transcript::new(&[]);
    let offset = std::fs::metadata(&transcript.path).unwrap().len();
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
    let [enqueue, remove, command] = chain(NONCE, AFTER);
    let transcript = Transcript::new(&[enqueue, remove]);
    assert_eq!(transcript.legacy(), None);
    let line = command.to_string();
    transcript.write(&line);
    assert_eq!(
        transcript.legacy(),
        None,
        "the command line has no newline yet"
    );
    transcript.write("\n");
    assert_eq!(
        transcript.legacy().as_deref(),
        Some(NONCE),
        "found once it is complete"
    );

    let replaced = transcript.want(&[HEAD], "status?", None).unwrap();
    let missing = transcript.want(&[HEAD], "status?", None).unwrap();
    let copy = transcript.path.with_extension("copy");
    std::fs::copy(&transcript.path, &copy).unwrap();
    std::fs::rename(&copy, &transcript.path).unwrap();
    assert!(replaced.search().is_none(), "another file at the same path");
    std::fs::remove_file(&transcript.path).unwrap();
    assert!(missing.search().is_none(), "no file");
}

#[test]
fn a_command_counts_only_after_its_removal_in_file_order() {
    let [enqueue, remove, command] = chain(NONCE, AFTER);
    let ok = |records: Vec<Value>| Transcript::new(&records).legacy();
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
        Transcript::new(&other).legacy(),
        None,
        "records of another session in this session's file"
    );
}

/// Heads pasted before v1 are still proven by their old frame, recorded or not; a v1 frame proves
/// its head under the name it shows, never under another author or source.
#[test]
fn an_old_frame_still_proves_its_head_and_a_v1_frame_needs_its_own_author_and_source() {
    let source = format!("discord:{CHANNEL}:{HEAD}");
    let legacy = |nonce: &str| {
        let frame = crate::services::tui_input::human_frame::legacy_inject;
        wrap(frame(&source, "200", nonce, "status?"), true)
    };
    let unrecorded = Transcript::new(&chain_of(legacy(NONCE), NONCE, AFTER));
    assert_eq!(unrecorded.legacy().as_deref(), Some(NONCE));
    let transcript = Transcript::new(&[]);
    let offset = std::fs::metadata(&transcript.path).unwrap().len();
    for record in chain_of(legacy(NONCE), NONCE, BEFORE) {
        transcript.write(&format!("{record}\n"));
    }
    let want = transcript.want(
        &[HEAD],
        "status?",
        Some(recorded(&transcript, NONCE, offset)),
    );
    let found = want.unwrap().search().map(|proof| proof.nonce);
    assert_eq!(found.as_deref(), Some(NONCE));

    let v1 = |author: &str, name: &str, source: &str| {
        wrap(
            human_frame::inject(source, author, name, NONCE, "status?"),
            true,
        )
    };
    let proven = |content: String| Transcript::new(&chain_of(content, NONCE, AFTER)).legacy();
    assert_eq!(
        proven(v1("200", "renamed (ops)", &source)).as_deref(),
        Some(NONCE)
    );
    assert_eq!(proven(v1("201", "ann", &source)), None, "another author");
    let other = format!("discord:{CHANNEL}:{}", HEAD - 1);
    assert_eq!(proven(v1("200", "ann", &other)), None, "another source");
    let fresh = v1("200", "ann", &source).replace("mode=inject", "mode=new");
    assert_eq!(proven(fresh), None, "a new turn's header");
}

#[test]
fn a_recorded_plain_frame_is_proven_only_with_its_durable_identity() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for wrapped in [false, true] {
        let transcript = Transcript::new(&[]);
        let offset = std::fs::metadata(&transcript.path).unwrap().len();
        let frame = human_frame::idle("ann", "200", "status?");
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
            .search()
            .expect("plain recorded frame must be scanned without a visible source");
        assert_eq!(proof.nonce, NONCE);
        assert_eq!(proof.offset, offset);
        assert!(
            transcript.legacy().is_none(),
            "an unnamed head cannot claim a plain frame"
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

#[test]
fn identical_plain_history_cannot_use_an_earlier_attempts_delayed_removal_chain() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let transcript = Transcript::new(&[]);
    let offset = std::fs::metadata(&transcript.path).unwrap().len();
    let frame = human_frame::idle("ann", "200", "status?");
    let first = chain_of(frame.clone(), NONCE, AFTER);
    human_frame::history::register(
        ("claude", "AgentDesk-claude-test", NONCE, "200"),
        &transcript.path,
        offset,
        &frame,
    )
    .unwrap();
    transcript.write(&format!("{}\n", first[0]));
    assert_eq!(
        human_frame::history::receipt(
            NONCE,
            &transcript.path,
            offset,
            "enqueue",
            Some("first-enqueue"),
            &frame,
        ),
        Some(true)
    );
    let second = std::fs::metadata(&transcript.path).unwrap().len();
    let second_nonce = "1e1e1e1e";
    human_frame::history::register(
        ("claude", "AgentDesk-claude-test", second_nonce, "200"),
        &transcript.path,
        second,
        &frame,
    )
    .unwrap();
    let second_chain = chain_of(frame, second_nonce, AFTER);
    for record in [&second_chain[0], &first[1], &first[2]] {
        transcript.write(&format!("{record}\n"));
    }
    assert!(
        transcript
            .want(
                &[HEAD],
                "status?",
                Some(recorded(&transcript, second_nonce, second)),
            )
            .unwrap()
            .search()
            .is_none(),
        "the second enqueue plus the first removal and command do not prove the second input"
    );
}
