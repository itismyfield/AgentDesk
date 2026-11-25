use super::*;
use crate::services::stream_json_cli::codec::AgyCodec;
use crate::services::tui_o::store::agy_spool::{
    AgyReservation, AgySpool, CHECKPOINT, ImessageSource, SEGMENT,
};
use crate::services::tui_o::store::fault;
use serde_json::{Value, json};
use std::fs;
use std::num::NonZeroU64;
use uuid::Uuid;

const ID: &str = "c1c2e570-1f0e-466a-aa13-6aa3dbf604cb";

#[test]
fn same_imessage_origin_cannot_reserve_two_execution_ids() {
    let root = tempfile::tempdir().unwrap();
    let source = ImessageSource::from_guid("p:0/Assistant-GUID").unwrap();
    assert_eq!(
        source.digest(),
        "d144ec4da71c5dd246697895b3bfef87c15aefd68de658b59cbcc63900cd0969"
    );
    let reserve = |destination, message| {
        AgySpool::reserve_imessage(
            root.path(),
            AgyReservation {
                destination,
                logical_channel: 42,
                message,
                nonce: Uuid::from_u128(message.into()),
                session_key: "discord:41",
                writer_generation: NonZeroU64::new(45).unwrap(),
                resume: None,
            },
            &source,
        )
    };
    assert!(reserve(41, 43).is_ok());
    assert_eq!(
        reserve(41, 44)
            .err()
            .expect("same source reserved twice")
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(reserve(42, 44).is_ok());
    let slot = root
        .path()
        .join(format!("o_store/41/agy/v1/imsg-{}", source.digest()));
    let open: Value = serde_json::from_slice(&fs::read(slot.join(SEGMENT)).unwrap()).unwrap();
    assert_eq!(open["turn_key"]["id"], 43);
    assert_eq!(open["data"]["ingress"], "imessage_http");
    assert_eq!(open["data"]["original_input_sha256"], source.digest());
}

struct Fixture {
    root: tempfile::TempDir,
    dir: PathBuf,
    spool: AgySpool,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("o_store/41/agy/v1/43");
        let spool = AgySpool::reserve(
            root.path(),
            AgyReservation {
                destination: 41,
                logical_channel: 42,
                message: 43,
                nonce: Uuid::from_u128(44),
                session_key: "discord:41",
                writer_generation: NonZeroU64::new(45).unwrap(),
                resume: None,
            },
        )
        .unwrap();
        Self { root, dir, spool }
    }

    fn run(&mut self, lines: &[Value], sender: Sender<StreamMessage>) -> Result<(), String> {
        let command = shell(self.root.path(), lines);
        run_prepared_recorded(command, sender, Duration::ZERO, None, &mut self.spool)
    }

    fn checkpoint(&self) -> Value {
        serde_json::from_slice(&fs::read(self.dir.join(CHECKPOINT)).unwrap()).unwrap()
    }

    fn records(&self) -> Vec<Value> {
        let checkpoint = self.checkpoint();
        let segment = fs::read(self.dir.join(SEGMENT)).unwrap();
        let committed = &segment[..checkpoint["committed_bytes"].as_u64().unwrap() as usize];
        use sha2::{Digest, Sha256};
        assert_eq!(
            checkpoint["prefix_hash"],
            format!("{:x}", Sha256::digest(committed))
        );
        assert!(committed.ends_with(b"\n"));
        let records: Vec<Value> = serde_json::Deserializer::from_slice(committed)
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(checkpoint["last_seq"], records.len() - 1);
        for (seq, record) in records.iter().enumerate() {
            assert_eq!(record["seq"], seq);
            assert_eq!(record["turn_key"]["id"], 43);
            assert_eq!(record["turn_key"]["nonce"], Uuid::from_u128(44).to_string());
        }
        records
    }
}

fn shell(dir: &std::path::Path, lines: &[Value]) -> PreparedCommand {
    let mut args = vec![
        "-c".into(),
        "printf '%s\\n' \"$@\"".into(),
        "agy-fixture".into(),
    ];
    args.extend(lines.iter().map(Value::to_string));
    PreparedCommand {
        executable: "/bin/sh".into(),
        resolution: BinaryResolution {
            requested_binary: "sh".into(),
            resolved_path: Some("/bin/sh".into()),
            canonical_path: None,
            source: None,
            attempts: vec![],
            failure_kind: None,
            exec_path: None,
        },
        args,
        redacted_args: vec![],
        current_dir: dir.into(),
        env: vec![],
        unset_env: vec![],
        codec: Box::new(AgyCodec::new()),
    }
}

fn transcript() -> Vec<Value> {
    vec![
        json!({"event": "init", "conversation_id": ID}),
        json!({"event": "step_update", "step_type": "agent_response", "text_delta": "draft"}),
    ]
}

fn result(body: &str) -> Value {
    json!({"event": "result", "result": {"status": "SUCCESS", "conversation_id": ID, "response": body}})
}

#[test]
fn stores_authoritative_body_and_preserves_the_legacy_done() {
    let mut fixture = Fixture::new();
    let body = format!("{}```\n", "한🙂".repeat(1100));
    let mut lines = transcript();
    lines.push(result(&body));
    let (tx, rx) = std::sync::mpsc::channel();
    fixture.run(&lines, tx).unwrap();

    let records = fixture.records();
    assert_eq!(records[0]["data"]["ingress"], "discord_foreground");
    assert_eq!(records[0]["data"]["writer_generation"], 45);
    // The gate's records precede anything the child could write.
    assert_eq!(records[1]["kind"], "launch");
    assert_eq!(records[2]["kind"], "started");
    assert_eq!(records[3]["kind"], "init");
    assert_eq!(records[4]["data"]["text"], "draft");
    let done = records.last().unwrap();
    assert_eq!(done["kind"], "done");
    assert_eq!(done["data"]["body_bytes"], body.len());
    let mut rebuilt = String::new();
    for part in &records[5..records.len() - 1] {
        assert_eq!(part["kind"], "body_part");
        assert_eq!(part["data"]["start"], rebuilt.len());
        rebuilt.push_str(part["data"]["text"].as_str().unwrap());
        assert_eq!(part["data"]["end"], rebuilt.len());
    }
    assert_eq!(rebuilt, body);
    assert!(rx.try_iter().any(|message| matches!(
        message, StreamMessage::Done { result, .. } if result == "draft"
    )));
}

#[test]
fn records_a_terminal_after_the_legacy_receiver_disconnects() {
    let mut fixture = Fixture::new();
    let mut lines = transcript();
    lines.push(json!({"event": "result", "status": "SUCCESS", "conversation_id": ID}));
    let (tx, rx) = std::sync::mpsc::channel();
    drop(rx);
    fixture.run(&lines, tx).unwrap();
    let records = fixture.records();
    assert_eq!(records[5]["data"]["text"], "draft");
    assert_eq!(records.last().unwrap()["kind"], "done");
}

#[test]
fn a_spawn_failure_settles_the_reservation() {
    let mut fixture = Fixture::new();
    let mut command = shell(fixture.root.path(), &[]);
    command.executable = fixture.root.path().join("missing-agy");
    let (tx, _rx) = std::sync::mpsc::channel();
    assert!(run_prepared_recorded(command, tx, Duration::ZERO, None, &mut fixture.spool).is_err());
    let records = fixture.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["kind"], "failed");
    assert_eq!(records[1]["data"]["reason"], "spawn");
}

#[test]
fn a_nonzero_child_exit_cannot_commit_done() {
    let mut fixture = Fixture::new();
    let mut lines = transcript();
    lines.push(result("final"));
    let mut command = shell(fixture.root.path(), &lines);
    command.args[1].push_str("; exit 17");
    let (tx, _rx) = std::sync::mpsc::channel();
    run_prepared_recorded(command, tx, Duration::ZERO, None, &mut fixture.spool).unwrap();
    let records = fixture.records();
    assert_eq!(records[5]["kind"], "failed");
    assert_eq!(records[5]["data"]["reason"], "child");
}

#[test]
fn empty_or_conflicting_authoritative_results_are_protocol_failures() {
    for terminal in [vec![result("")], vec![result("first"), result("second")]] {
        let mut fixture = Fixture::new();
        let mut lines = transcript();
        lines.extend(terminal);
        let (tx, _rx) = std::sync::mpsc::channel();
        fixture.run(&lines, tx).unwrap();
        let records = fixture.records();
        assert_eq!(records[5]["kind"], "failed");
        assert_eq!(records[5]["data"]["reason"], "protocol");
    }
}

#[test]
fn an_unsynced_segment_does_not_advance_the_frontier() {
    let mut fixture = Fixture::new();
    let open = fixture.checkpoint();
    let _fault = fault::plant(
        &fixture.dir,
        fault::Step::AgySegmentSync,
        std::io::ErrorKind::StorageFull,
        None,
    );
    let mut lines = transcript();
    lines.push(result("final"));
    let (tx, _rx) = std::sync::mpsc::channel();
    // No durable launch, no spawn: the child never runs.
    assert!(fixture.run(&lines, tx).is_err());
    assert_eq!(fixture.checkpoint(), open);
    let len = fs::metadata(fixture.dir.join(SEGMENT)).unwrap().len();
    assert!(len > open["committed_bytes"].as_u64().unwrap());
}

#[test]
fn legacy_headless_uses_delta_done_and_never_creates_a_spool() {
    let root = tempfile::tempdir().unwrap();
    let mut lines = transcript();
    lines.push(result("final"));
    let (tx, rx) = std::sync::mpsc::channel();
    run_prepared(shell(root.path(), &lines), tx, Duration::ZERO, None).unwrap();
    assert!(rx.try_iter().any(|message| matches!(
        message, StreamMessage::Done { result, .. } if result == "draft"
    )));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

// C2: the gate runs its command only after the go line; a closed stdin exits 75 and runs nothing.
#[test]
fn the_gate_runs_input_only_after_the_go_line() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    for (go, code) in [(false, 75), (true, 0)] {
        let marker = dir.path().join(format!("ran-{go}"));
        let args = vec!["-c".to_string(), format!("touch '{}'", marker.display())];
        let mut command = super::agy_recording::gated(std::path::Path::new("/bin/sh"), &args);
        let mut child = command.stdin(std::process::Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        if go {
            stdin.write_all(b"go\n").unwrap();
        }
        drop(stdin);
        assert_eq!(child.wait().unwrap().code(), Some(code));
        assert_eq!(marker.exists(), go);
    }
}
