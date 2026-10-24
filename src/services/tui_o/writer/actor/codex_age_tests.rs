use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use serde_json::{Value, json};

const SESSION: &str = "019a5f47-28b5-7000-8000-000000000001";
const AT: &str = "2026-09-27T00:00:00.123456789Z";

fn header() -> Value {
    json!({"type":"session_meta","payload":{
        "id":SESSION,"cwd":"/work/project","source":"cli"
    }})
}

fn row(kind: &str, payload: Value) -> Value {
    json!({"type":kind,"timestamp":AT,"payload":payload})
}

fn message() -> Value {
    row(
        "response_item",
        json!({"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"answer"}]}),
    )
}

fn native(root: &Path, head: &Value, rows: &[Value]) -> Cursor {
    let mut bytes = Vec::new();
    for row in std::iter::once(head).chain(rows.iter()) {
        serde_json::to_writer(&mut bytes, row).unwrap();
        bytes.push(b'\n');
    }
    let path = root.join(format!("rollout-2026-09-27T00-00-00-{SESSION}.jsonl"));
    std::fs::write(&path, &bytes).unwrap();
    Cursor {
        source: source_id_for(SESSION, &path).unwrap(),
        captured_through: bytes.len() as u64,
        prefix_hash: hex::encode(Sha256::digest(&bytes)),
        retired: false,
        transition_cleanup: None,
    }
}

fn age(cursor: &Cursor, root: &Path) -> Option<SystemTime> {
    last_dialogue_for(cursor, ShadowProvider::Codex, Some(root))
}

fn at(raw: &str) -> SystemTime {
    SystemTime::from(
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc),
    )
}

#[test]
fn automatic_codex_age_native_dialogue_formats_and_legacy_are_timestamped() {
    let mut legacy =
        json!({"type":"item.completed","item":{"type":"agent_message","text":"legacy"}});
    legacy["timestamp"] = json!(AT);
    for candidate in [
        message(),
        row(
            "response_item",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}),
        ),
        row(
            "response_item",
            json!({"type":"agent_message","text":"legacy"}),
        ),
        row(
            "event_msg",
            json!({"type":"user_message","message":"hello"}),
        ),
        row(
            "event_msg",
            json!({"type":"agent_message","message":"answer"}),
        ),
        row(
            "event_msg",
            json!({"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"hello"}]}}),
        ),
        row(
            "event_msg",
            json!({"type":"item_completed","item":{"type":"AgentMessage","id":"message-mirror"}}),
        ),
        row(
            "event_msg",
            json!({"type":"task_complete","last_agent_message":"fallback"}),
        ),
        legacy,
    ] {
        let root = tempfile::tempdir().unwrap();
        let cursor = native(root.path(), &header(), std::slice::from_ref(&candidate));
        assert_eq!(age(&cursor, root.path()), Some(at(AT)), "{candidate}");
    }
}

#[test]
fn automatic_codex_age_uses_maximum_dialogue_time_and_never_metadata_or_empty_terminal() {
    let root = tempfile::tempdir().unwrap();
    let mut older = message();
    older["timestamp"] = json!("2026-09-26T00:00:00Z");
    let future = "2026-10-23T00:00:00Z";
    let metadata = [
        row(
            "response_item",
            json!({"type":"message","role":"developer",
            "content":[{"type":"input_text","text":"developer instructions"}]}),
        ),
        row(
            "response_item",
            json!({"type":"message","role":"system",
            "content":[{"type":"input_text","text":"system instructions"}]}),
        ),
        row("turn_context", json!({})),
        row("response_item", json!({"type":"reasoning"})),
        row(
            "response_item",
            json!({"type":"function_call","arguments":"{}"}),
        ),
        row("event_msg", json!({"type":"token_count"})),
        row("event_msg", json!({"type":"task_complete"})),
        row(
            "event_msg",
            json!({"type":"task_complete","last_agent_message":null}),
        ),
        row(
            "event_msg",
            json!({"type":"task_complete","last_agent_message":"  "}),
        ),
        row(
            "event_msg",
            json!({"type":"item_completed","item":{"type":"CommandExecution","id":"tool"}}),
        ),
    ];
    let mut rows = vec![message(), older];
    rows.extend(metadata.map(|mut row| {
        row["timestamp"] = json!(future);
        row
    }));
    let cursor = native(root.path(), &header(), &rows);
    assert_eq!(age(&cursor, root.path()), Some(at(AT)));
    let cursor = native(root.path(), &header(), &rows[2..]);
    assert_eq!(
        age(&cursor, root.path()),
        None,
        "metadata never supplies an age"
    );
}

#[test]
fn automatic_codex_age_real_native_mirrors_and_terminal_fallback_are_pure() {
    let fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/codex_busy_inject/model_turn.jsonl"
    ));
    let rows: Vec<Value> = fixture
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let root = tempfile::tempdir().unwrap();
    let cursor = native(root.path(), &header(), &rows);
    let before = std::fs::read(&cursor.source.path).unwrap();
    assert_eq!(
        age(&cursor, root.path()),
        Some(at("2026-10-08T22:01:20.789Z"))
    );
    assert_eq!(std::fs::read(&cursor.source.path).unwrap(), before);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn automatic_codex_age_unknown_rows_roles_content_and_dialogue_times_fail_closed() {
    let mut wrong_role = message();
    wrong_role["payload"]["role"] = json!("future_role");
    let mut wrong_content = message();
    wrong_content["payload"]["content"][0]["type"] = json!("future_message_body");
    let mut missing_time = message();
    missing_time.as_object_mut().unwrap().remove("timestamp");
    let mut bad_time = message();
    bad_time["timestamp"] = json!("2026-09-28T00:00:00");
    for bad in [
        row("new_dialogue_envelope", json!({"message":"newer"})),
        row("response_item", json!({"type":"future_message"})),
        row("event_msg", json!({"type":"future_message"})),
        row(
            "event_msg",
            json!({"type":"item_completed","item":{"type":"FutureMessage"}}),
        ),
        row(
            "event_msg",
            json!({"type":"task_complete","last_agent_message":12}),
        ),
        row("event_msg", json!({"type":"agent_message"})),
        wrong_role,
        wrong_content,
        missing_time,
        bad_time,
        header(),
        Value::Null,
    ] {
        let root = tempfile::tempdir().unwrap();
        let cursor = native(root.path(), &header(), &[message(), bad.clone()]);
        assert_eq!(age(&cursor, root.path()), None, "{bad}");
    }
}

#[test]
fn automatic_codex_age_native_identity_header_root_and_descriptor_are_required() {
    let root = tempfile::tempdir().unwrap();
    for (field, value) in [
        ("id", json!(uuid::Uuid::new_v4().to_string())),
        ("cwd", json!("relative/project")),
        ("cwd", json!("")),
        ("source", json!({"subagent":{"thread_spawn":{}}})),
        ("source", json!("wrapper")),
        ("parent_thread_id", json!(SESSION)),
    ] {
        let mut head = header();
        head["payload"][field] = value;
        let cursor = native(root.path(), &head, &[message()]);
        assert_eq!(age(&cursor, root.path()), None, "header {head}");
    }
    let mut wrapper = header();
    wrapper["type"] = json!("normalized_relay");
    let cursor = native(root.path(), &wrapper, &[message()]);
    assert_eq!(age(&cursor, root.path()), None);
    let mut cursor = native(root.path(), &header(), &[message()]);
    let elsewhere = tempfile::tempdir().unwrap();
    assert_eq!(
        age(&cursor, elsewhere.path()),
        None,
        "outside provider root"
    );
    assert_eq!(
        last_dialogue_for(&cursor, ShadowProvider::Codex, None),
        None
    );
    cursor.source.session_id = "not-a-native-uuid".into();
    assert_eq!(age(&cursor, root.path()), None);
    let mut cursor = native(root.path(), &header(), &[message()]);
    let renamed = root.path().join("wrapper.jsonl");
    std::fs::rename(&cursor.source.path, &renamed).unwrap();
    cursor.source.path = renamed;
    assert_eq!(age(&cursor, root.path()), None, "wrapper filename");
}

#[test]
fn automatic_codex_age_shared_exact_cursor_hash_eof_and_budget_guards_hold() {
    let root = tempfile::tempdir().unwrap();
    let original = native(root.path(), &header(), &[message()]);
    let mut changed = original.clone();
    changed.captured_through -= 1;
    assert_eq!(age(&changed, root.path()), None, "uncaptured bytes");
    changed = original.clone();
    changed.prefix_hash = "wrong".into();
    assert_eq!(age(&changed, root.path()), None, "hash changed");
    changed = original.clone();
    changed.source.ino = changed.source.ino.wrapping_add(1);
    assert_eq!(age(&changed, root.path()), None, "inode changed");
    changed = original;
    changed.captured_through = MAX_BYTES + 1;
    assert_eq!(age(&changed, root.path()), None, "budget before read");
    let cursor = native(root.path(), &header(), &[message()]);
    let mut bytes = std::fs::read(&cursor.source.path).unwrap();
    bytes.extend_from_slice(b"{bad-json}\n");
    std::fs::write(&cursor.source.path, &bytes).unwrap();
    let mut malformed = cursor;
    malformed.captured_through = bytes.len() as u64;
    malformed.prefix_hash = hex::encode(Sha256::digest(&bytes));
    assert_eq!(age(&malformed, root.path()), None);
    bytes.truncate(bytes.len() - b"{bad-json}\n".len() - 1);
    std::fs::write(&malformed.source.path, &bytes).unwrap();
    malformed.captured_through = bytes.len() as u64;
    malformed.prefix_hash = hex::encode(Sha256::digest(&bytes));
    assert_eq!(age(&malformed, root.path()), None, "torn tail");
}
