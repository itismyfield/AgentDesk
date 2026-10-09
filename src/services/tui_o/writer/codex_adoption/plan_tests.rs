use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::coord::{Namespace, NativeCheckpoint, Offset, Pane, WrapperCheckpoint};
use super::fold::{LivePane, fold};
use super::judge::Verdict;
use super::plan::{Reads, Refusal, VerifiedCodexInit, verify};
use super::{Evidence, Load, Retirement};
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvidence, BindingRecord, BindingTarget,
};

const CHANNEL: u64 = 77;

fn turn(id: &str, tool: bool) -> Vec<Value> {
    let mut lines = vec![
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": id}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "typed in the TUI"}]}}),
    ];
    if tool {
        lines.push(
            json!({"type": "response_item", "payload": {"type": "function_call",
            "id": format!("fc-{id}"), "call_id": format!("c-{id}"), "name": "shell",
            "arguments": "{}"}}),
        );
        lines.push(
            json!({"type": "response_item", "payload": {"type": "function_call_output",
            "call_id": format!("c-{id}"), "output": "ok"}}),
        );
    }
    lines.extend([
        json!({"type": "event_msg", "payload": {"type": "item_completed",
            "item": {"type": "AgentMessage", "id": format!("m-{id}")}}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
            "id": format!("m-{id}"), "content": [{"type": "output_text", "text": "same body"}]}}),
        json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}}),
        json!({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": id,
            "last_agent_message": "same body"}}),
    ]);
    lines
}

fn quiet() -> Value {
    json!({"type": "event_msg", "payload": {"type": "token_count", "info": null}})
}

fn append(path: &Path, lines: &[Value]) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn source(dir: &Path, session: &str, turns: usize) -> SourceId {
    let path = dir.join(format!("{session}.jsonl"));
    append(
        &path,
        &[json!({"type": "session_meta", "payload": {"id": session, "cwd": "/tmp"}})],
    );
    for index in 0..turns {
        append(&path, &turn(&format!("{session}-t{index}"), index % 3 == 0));
    }
    let (dev, ino) = file_identity(&std::fs::metadata(&path).unwrap());
    SourceId {
        session_id: session.into(),
        path,
        dev,
        ino,
    }
}

fn len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

fn event(seq: u64, pane: (&str, &str), record: BindingRecord) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Codex,
        tmux_session: pane.0.into(),
        execution_nonce: pane.1.into(),
        record,
        committed_at: Utc::now(),
    }
}

fn bind(old: Option<&SourceId>, new: BindingTarget, cause: BindingCause) -> BindingRecord {
    let evidence = BindingEvidence {
        hook_event: "SessionStart".into(),
        received_at: Utc::now(),
        reclaims: false,
    };
    BindingRecord::Bound {
        old: old.cloned(),
        new,
        cause,
        parent_hint: None,
        evidence,
    }
}

/// `S0 Startup` under an ended execution, then `S1 Startup → S2 Clear (Pending) → Resolved S2`
/// on the live one, with an empty named-only source; 33+ turns with tools and repeated bodies.
struct Fixture {
    _dir: tempfile::TempDir,
    sources: [SourceId; 3],
    named: SourceId,
    wrapper: PathBuf,
    events: Vec<BindingEvent>,
    live: Vec<LivePane>,
    checkpoints: Vec<NativeCheckpoint>,
    wrappers: Vec<WrapperCheckpoint>,
    facts: Value,
}

fn pane(tmux: &str, nonce: &str) -> Pane {
    Pane {
        tmux: tmux.into(),
        nonce: nonce.into(),
    }
}

fn native(source: &SourceId, at: u64) -> Offset {
    Offset {
        namespace: Namespace::Native {
            path: source.path.clone(),
            ino: source.ino,
        },
        at,
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let s0 = source(dir.path(), "s0", 4);
    let s1 = source(dir.path(), "s1", 34);
    let s2 = source(dir.path(), "s2", 34);
    append(&s2.path, &[quiet()]);
    let named = source(dir.path(), "named", 0);
    std::fs::write(&named.path, "").unwrap();
    let wrapper = dir.path().join("wrapper.jsonl");
    std::fs::write(&wrapper, vec![b'x'; 500]).unwrap();
    let pending = BindingTarget::Pending {
        payload_session_id: "s2".into(),
        payload_transcript_path: s2.path.clone(),
    };
    let mut clear = bind(Some(&s1), pending, BindingCause::Clear);
    if let BindingRecord::Bound { parent_hint, .. } = &mut clear {
        *parent_hint = Some(s0.clone());
    }
    let events = vec![
        event(
            1,
            ("a", "n0"),
            bind(
                None,
                BindingTarget::Source(s0.clone()),
                BindingCause::Startup,
            ),
        ),
        event(
            2,
            ("a", "n1"),
            bind(
                Some(&named),
                BindingTarget::Source(s1.clone()),
                BindingCause::Startup,
            ),
        ),
        event(3, ("a", "n1"), clear),
        event(
            4,
            ("a", "n1"),
            BindingRecord::Resolved {
                resolves_seq: 3,
                source: s2.clone(),
            },
        ),
    ];
    let live = vec![LivePane {
        pane: pane("a", "n1"),
        output_path: s2.path.clone(),
        relay_output_path: Some(wrapper.clone()),
    }];
    let checkpoints = vec![checkpoint(
        &s2,
        pane("a", "n1"),
        4,
        len(&s2.path) - quiet_len(),
    )];
    let wrappers = vec![wrapped(&wrapper, pane("a", "n1"), 500)];
    let clear: serde_json::Map<String, Value> = super::Obligation::ALL
        .iter()
        .map(|kind| (super::judge::snake(kind), json!("clear")))
        .collect();
    let facts = json!({
        "channel": CHANNEL, "provider": "codex", "runtime_kind": "codex_tui", "role": "gateway",
        "store": "fresh", "candidate": "pending", "discovery": "complete", "recovery": "complete",
        "obligations": clear, "anchor": {"status": "latest", "id": 900},
    });
    Fixture {
        _dir: dir,
        sources: [s0, s1, s2],
        named,
        wrapper,
        events,
        live,
        checkpoints,
        wrappers,
        facts,
    }
}

fn quiet_len() -> u64 {
    format!("{}\n", quiet()).len() as u64
}

fn checkpoint(source: &SourceId, pane: Pane, proof_seq: u64, at: u64) -> NativeCheckpoint {
    NativeCheckpoint {
        channel: CHANNEL,
        pane,
        proof_seq,
        session: source.session_id.clone(),
        path: source.path.clone(),
        ino: source.ino,
        cursor: native(source, at),
    }
}

fn wrapped(path: &Path, pane: Pane, floor: u64) -> WrapperCheckpoint {
    let ino = file_identity(&std::fs::metadata(path).unwrap()).1;
    let namespace = Namespace::Wrapper {
        path: path.into(),
        ino,
        generation: 3,
    };
    let at = |at| Offset {
        namespace: namespace.clone(),
        at,
    };
    WrapperCheckpoint {
        channel: CHANNEL,
        pane,
        namespace: namespace.clone(),
        eof: 500,
        cursor: at(500),
        floor: at(floor),
        backlog: Load::Clear,
    }
}

impl Fixture {
    fn verify(&self) -> Result<VerifiedCodexInit, Refusal> {
        let facts: Evidence = serde_json::from_value(self.facts.clone()).unwrap();
        let reads = Reads {
            events: &self.events,
            live: &self.live,
            checkpoints: &self.checkpoints,
            wrappers: &self.wrappers,
        };
        verify(facts, reads)
    }

    /// The codes the evaluator refused the plan with.
    fn refused(&self) -> Vec<String> {
        match self.verify() {
            Err(Refusal::Judged(judgment)) if judgment.boundary == Verdict::Refused => {
                judgment.refused.into_iter().collect()
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

fn hash(path: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
}

#[test]
fn a_history_without_receipts_starts_every_bound_source_at_its_read_end() {
    let fixture = fixture();
    let plan = fixture.verify().unwrap();
    let [s0, s1, s2] = &fixture.sources;
    let starts: Vec<(&SourceId, u64, String)> = plan
        .sources()
        .iter()
        .map(|init| {
            (
                &init.source_id,
                init.delivery_start,
                init.prefix_hash.clone(),
            )
        })
        .collect();
    fn read(s: &SourceId) -> (&SourceId, u64, String) {
        (s, len(&s.path), hash(&s.path))
    }
    assert_eq!(starts, vec![read(s2), read(s1), read(s0)]);
    assert_eq!(
        (plan.channel(), plan.seq(), plan.anchor()),
        (CHANNEL, 4, 900)
    );
    let folded = fold(&fixture.events, &fixture.live).unwrap();
    let retired: Vec<Retirement> = folded.retired.iter().map(|(_, r, _)| *r).collect();
    assert_eq!(retired, vec![Retirement::Replaced, Retirement::Exited]);
    assert_eq!(folded.named, vec![fixture.named.clone()]);
}

#[test]
fn records_past_the_checkpoint_refuse_unless_quiet() {
    let body = turn("late", false)[3].clone();
    let tool = turn("late", true)[2].clone();
    let prompt = turn("late", false)[1].clone();
    let start = turn("late", false)[0].clone();
    for (record, code) in [
        (prompt, "current.suffix_prompt"),
        (body, "current.suffix_output"),
        (tool, "current.suffix_output"),
        (start, "current.suffix_start"),
        (
            json!({"type": "compacted", "payload": {}}),
            "current.suffix_unrecognized",
        ),
    ] {
        let fixture = fixture();
        append(&fixture.sources[2].path, &[record]);
        assert!(fixture.refused().contains(&code.to_owned()), "{code}");
    }
    let torn = fixture();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&torn.sources[2].path)
        .unwrap();
    file.write_all(b"{\"type\":").unwrap();
    assert!(
        torn.refused()
            .contains(&"current.suffix_partial".to_owned())
    );
    let mut off = fixture();
    off.checkpoints[0].cursor.at -= 1;
    assert!(off.refused().contains(&"current.suffix_partial".to_owned()));
}

#[test]
fn only_the_last_turns_own_completion_closes_the_current_source() {
    let start =
        |id: &str| json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": id}});
    let close = |kind: &str, id: Option<&str>| json!({"type": "event_msg", "payload": {"type": kind, "turn_id": id}});
    let announce = json!({"type": "event_msg", "payload": {"type": "item_completed",
        "item": {"type": "AgentMessage", "id": "unsealed"}}});
    let call = json!({"type": "response_item", "payload": {"type": "function_call",
        "id": "fc-open", "call_id": "c-open", "name": "shell", "arguments": "{}"}});
    let cases: Vec<(&str, Vec<Value>)> = vec![
        (
            "another turn's completion",
            vec![start("x"), close("task_complete", Some("y"))],
        ),
        (
            "an unnamed abort",
            vec![start("x"), close("turn_aborted", None)],
        ),
        (
            "an unsealed announcement",
            vec![start("x"), announce, close("task_complete", Some("x"))],
        ),
        (
            "an unanswered tool call",
            vec![start("x"), call, close("task_complete", Some("x"))],
        ),
        ("a new start", vec![start("x")]),
    ];
    for (case, records) in cases {
        let mut fixture = fixture();
        append(&fixture.sources[2].path, &records);
        fixture.checkpoints[0].cursor.at = len(&fixture.sources[2].path);
        assert!(
            fixture.refused().contains(&"current.turn_open".to_owned()),
            "{case}"
        );
    }
    let mut named_abort = fixture();
    let records = [start("x"), close("turn_aborted", Some("x"))];
    append(&named_abort.sources[2].path, &records);
    named_abort.checkpoints[0].cursor.at = len(&named_abort.sources[2].path);
    assert!(named_abort.verify().is_ok());
    let malformed = fixture();
    append(&malformed.sources[1].path, &[json!("not an object")]);
    assert!(
        malformed
            .refused()
            .contains(&"retired.prefix_malformed".to_owned())
    );
}

#[test]
fn a_checkpoint_or_wrapper_off_the_sources_coordinates_refuses() {
    type Edit = fn(&mut Fixture);
    let cases: [(&str, Edit, &str); 11] = [
        (
            "channel",
            |f| f.checkpoints[0].channel = 78,
            "current.proof_mismatch",
        ),
        (
            "session",
            |f| f.checkpoints[0].session = "s1".into(),
            "current.proof_mismatch",
        ),
        (
            "nonce",
            |f| f.checkpoints[0].pane.nonce = "n0".into(),
            "current.proof_unlinked",
        ),
        (
            "path",
            |f| f.checkpoints[0].path = f.sources[1].path.clone(),
            "current.proof_mismatch",
        ),
        (
            "proof seq",
            |f| f.checkpoints[0].proof_seq = 3,
            "current.proof_mismatch",
        ),
        (
            "absent",
            |f| f.checkpoints.clear(),
            "current.proof_unlinked",
        ),
        (
            "cursor in the wrapper's namespace",
            |f| f.checkpoints[0].cursor.namespace = f.wrappers[0].namespace.clone(),
            "current.proof_mismatch",
        ),
        (
            "foreign provider",
            |f| f.events[0].provider = ShadowProvider::Claude,
            "retired.proof_mismatch",
        ),
        (
            "floor in the native namespace",
            |f| f.wrappers[0].floor.namespace = f.checkpoints[0].cursor.namespace.clone(),
            "wrapper.unsupported",
        ),
        (
            "another generation's floor",
            |f| {
                let path = f.wrapper.clone();
                let ino = file_identity(&std::fs::metadata(&path).unwrap()).1;
                f.wrappers[0].floor.namespace = Namespace::Wrapper {
                    path,
                    ino,
                    generation: 2,
                };
            },
            "wrapper.unsupported",
        ),
        (
            "wrapper backlog",
            |f| f.wrappers[0].backlog = Load::Busy,
            "wrapper.backlog",
        ),
    ];
    for (case, edit, code) in cases {
        let mut fixture = fixture();
        edit(&mut fixture);
        assert!(
            fixture.refused().contains(&code.to_owned()),
            "{case}: {:?}",
            fixture.verify()
        );
    }
    // A live spool with no checkpoint, or a checkpoint for another file, is not skipped.
    let mut unread = fixture();
    unread.wrappers.clear();
    assert!(unread.refused().contains(&"wrapper.unsupported".to_owned()));
    let mut elsewhere = fixture();
    elsewhere.live[0].relay_output_path = Some(elsewhere.sources[0].path.with_extension("spool"));
    assert!(
        elsewhere
            .refused()
            .contains(&"wrapper.unsupported".to_owned())
    );
    let mut short = fixture();
    short.wrappers[0] = wrapped(&short.wrapper, pane("a", "n1"), 499);
    assert!(
        short
            .refused()
            .contains(&"wrapper.suffix_unrecognized".to_owned())
    );
    let renamed = fixture();
    let meta = json!({"type": "session_meta", "payload": {"id": "other", "cwd": "/tmp"}});
    let text = std::fs::read_to_string(&renamed.sources[1].path).unwrap();
    let (_, rest) = text.split_once('\n').unwrap();
    std::fs::write(&renamed.sources[1].path, format!("{meta}\n{rest}")).unwrap();
    assert!(
        renamed
            .refused()
            .contains(&"retired.proof_mismatch".to_owned())
    );
}

#[test]
fn a_past_source_retires_only_by_a_later_old_bind_or_its_ended_execution() {
    let mut read_live = fixture();
    read_live.live[0].relay_output_path = Some(read_live.sources[0].path.clone());
    assert!(read_live.refused().contains(&"retired.live".to_owned()));
    // The live execution bound s1 and nothing on its pane named it old.
    let mut unproven = fixture();
    if let BindingRecord::Bound { old, .. } = &mut unproven.events[2].record {
        *old = None;
    }
    assert!(unproven.refused().contains(&"retired.unproven".to_owned()));
    let nonempty = fixture();
    append(&nonempty.named.path, &[quiet()]);
    assert!(nonempty.refused().contains(&"named.nonempty".to_owned()));
    let mut pending = fixture();
    pending.events.pop();
    assert!(matches!(pending.verify(), Err(Refusal::Fold(_))));
    let mut stray = fixture();
    stray.live.push(LivePane {
        pane: pane("b", "m1"),
        output_path: stray.sources[0].path.clone(),
        relay_output_path: None,
    });
    assert!(matches!(stray.verify(), Err(Refusal::Fold(_))));
}

#[test]
fn every_live_pane_keeps_its_own_current_source() {
    let mut fixture = fixture();
    let dir = fixture.sources[0].path.parent().unwrap().to_path_buf();
    let s3 = source(&dir, "s3", 2);
    let bound = bind(
        None,
        BindingTarget::Source(s3.clone()),
        BindingCause::Startup,
    );
    fixture.events.insert(2, event(3, ("b", "m1"), bound));
    for (seq, event) in fixture.events.iter_mut().enumerate() {
        event.seq = seq as u64 + 1;
    }
    if let BindingRecord::Resolved { resolves_seq, .. } = &mut fixture.events[4].record {
        *resolves_seq = 4;
    }
    fixture.checkpoints[0].proof_seq = 5;
    fixture.live.push(LivePane {
        pane: pane("b", "m1"),
        output_path: s3.path.clone(),
        relay_output_path: None,
    });
    fixture
        .checkpoints
        .push(checkpoint(&s3, pane("b", "m1"), 3, len(&s3.path)));
    let plan = fixture.verify().unwrap();
    let starts: Vec<&SourceId> = plan.sources().iter().map(|init| &init.source_id).collect();
    let [s0, s1, s2] = &fixture.sources;
    assert_eq!(starts, vec![s2, &s3, s1, s0]);
}
