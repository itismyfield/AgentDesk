//! The proof a head's transcript must hold: one exact enqueue of its frame under one nonce, then the
//! absorbed removal and the human prompt command naming the same ids, in one session.
use super::*;
use serde_json::{Value, json};
use std::io::Write;

const CHANNEL: u64 = 6_813_460;
const HEAD: u64 = 9_100_094_081_725_135_881;
const NONCE: &str = "d286a43b";
const SESSION: &str = "47d7dbe8";
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

fn prompt(nonce: &str, wrapped: bool) -> String {
    let source = format!("discord:{CHANNEL}:{HEAD}");
    let frame = busy_inject::frame(&source, "200", nonce, "status?");
    match wrapped {
        true => format!("<pasted_content id=\"b0ac\">\n{frame}\n</pasted_content id=\"b0ac\">"),
        false => frame,
    }
}

/// Claude's records of an absorbed queued paste: enqueue, removal and human prompt command.
fn chain(nonce: &str, stamp: &str) -> [Value; 3] {
    let content = prompt(nonce, true);
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

    fn bound(&self, session_id: Option<&str>) -> Bound {
        Bound {
            session: "AgentDesk-claude-test".into(),
            path: self.path.clone(),
            file: file_id(&std::fs::metadata(&self.path).unwrap()).unwrap(),
            session_id: session_id.map(str::to_string),
        }
    }

    fn want(&self, sources: &[u64], text: &str, recorded: Option<Recorded>) -> Option<Want> {
        let channel = ChannelId::new(CHANNEL);
        let bound = self.bound(None);
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
    let bound = transcript.bound(None);
    Recorded {
        nonce: nonce.into(),
        transcript: bound.path.display().to_string(),
        session_id: None,
        file: Some(bound.file),
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
    let bound = transcript.bound(None);
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
    let named = transcript.bound(Some("another-session"));
    let wrong = Want::of(
        channel,
        &identity(&[HEAD]),
        &original("status?", 200),
        None,
        &named,
    );
    assert!(
        wrong.unwrap().search().is_none(),
        "the binding names another native session"
    );
    let named = transcript.bound(Some(SESSION));
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
    replaced.file = Some((0, 0));
    assert!(
        transcript
            .want(&[HEAD], "status?", Some(replaced))
            .is_none()
    );
    let mut other_session = recorded(&transcript, NONCE, offset);
    other_session.session_id = Some("another-session".into());
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
