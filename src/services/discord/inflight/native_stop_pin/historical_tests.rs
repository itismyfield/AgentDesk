use super::*;
use std::os::unix::fs::MetadataExt;

fn fixture() -> (tempfile::TempDir, InflightTurnState) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("historical.jsonl");
    let prefix = b"unrelated malformed bytes before the pinned range\n";
    let raw = serde_json::json!({
        "type": "user", "uuid": "native-A", "sessionId": "historical-session",
        "message": {"role": "user", "content": "historical input"}
    })
    .to_string()
        + "\n";
    std::fs::write(&path, [prefix.as_slice(), raw.as_bytes()].concat()).unwrap();
    let metadata = path.metadata().unwrap();
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        780_701,
        None,
        7,
        8,
        9,
        "historical input".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some("episode-A".into());
    let input = NativeUserPin {
        uuid: "native-A".into(),
        source_file: (metadata.dev(), metadata.ino()),
        range: (prefix.len() as u64, metadata.len()),
        receipt_start: 0,
        row_digest: blake3::hash(raw.as_bytes()).to_hex().to_string(),
        frame_digest: "unused-historical-frame".into(),
        session_id: "historical-session".into(),
        output_path: path.display().to_string(),
        tmux_session: "historical-pane".into(),
        spawn_nonce: Some("historical-incarnation".into()),
        direct: false,
    };
    row.native_stop_user_pin = Some(VerifiedNativeUserPin::new(&row, input).unwrap());
    (directory, row)
}

#[test]
fn historical_input_accepts_pin_coordinates_across_device_renumbering() {
    let (_directory, mut row) = fixture();
    row.output_path = Some("/absent/successor-transcript.jsonl".into());
    row.turn_start_offset = Some(u64::MAX);
    assert!(historical_input_verified(&row));
    row.native_stop_user_pin
        .as_mut()
        .unwrap()
        .input
        .source_file
        .0 ^= 1;
    assert!(historical_input_verified(&row));
}

#[test]
fn historical_input_requires_complete_matching_native_identity() {
    let (_directory, row) = fixture();
    type RowMutation = fn(&mut InflightTurnState);
    let cases: [(&str, RowMutation); 12] = [
        ("missing pin", |row| row.native_stop_user_pin = None),
        ("successor owner", |row| row.user_msg_id += 1),
        ("missing turn nonce", |row| row.turn_nonce = None),
        ("missing incarnation", |row| {
            row.native_stop_user_pin.as_mut().unwrap().input.spawn_nonce = None
        }),
        ("empty incarnation", |row| {
            row.native_stop_user_pin.as_mut().unwrap().input.spawn_nonce = Some(" ".into())
        }),
        ("empty uuid", |row| {
            row.native_stop_user_pin
                .as_mut()
                .unwrap()
                .input
                .uuid
                .clear()
        }),
        ("wrong uuid", |row| {
            row.native_stop_user_pin.as_mut().unwrap().input.uuid = "native-B".into()
        }),
        ("wrong session", |row| {
            row.native_stop_user_pin.as_mut().unwrap().input.session_id = "successor-session".into()
        }),
        ("empty session", |row| {
            row.native_stop_user_pin
                .as_mut()
                .unwrap()
                .input
                .session_id
                .clear()
        }),
        ("empty episode nonce", |row| {
            row.turn_nonce = Some(String::new());
            row.native_stop_user_pin
                .as_mut()
                .unwrap()
                .turn_nonce
                .clear();
        }),
        ("wrong inode", |row| {
            row.native_stop_user_pin
                .as_mut()
                .unwrap()
                .input
                .source_file
                .1 ^= 1
        }),
        ("wrong digest", |row| {
            row.native_stop_user_pin.as_mut().unwrap().input.row_digest = "wrong".into()
        }),
    ];
    for (name, mutate) in cases {
        let mut changed = row.clone();
        mutate(&mut changed);
        assert!(!historical_input_verified(&changed), "{name}");
    }
}

#[test]
fn historical_input_rejects_non_native_and_enqueue_only_records() {
    let (_directory, row) = fixture();
    let invalid = [
        serde_json::json!({"type":"queue-operation","uuid":"native-A","sessionId":"historical-session","operation":"enqueue","message":{"role":"user","content":"historical input"}}),
        serde_json::json!({"type":"user","isMeta":true,"uuid":"native-A","sessionId":"historical-session","message":{"role":"user","content":"historical input"}}),
        serde_json::json!({"type":"user","isSidechain":true,"uuid":"native-A","sessionId":"historical-session","message":{"role":"user","content":"historical input"}}),
        serde_json::json!({"type":"user","isCompactSummary":true,"uuid":"native-A","sessionId":"historical-session","message":{"role":"user","content":"historical input"}}),
    ];
    for record in invalid {
        let mut changed = row.clone();
        let input = &mut changed.native_stop_user_pin.as_mut().unwrap().input;
        let raw = record.to_string() + "\n";
        std::fs::write(&input.output_path, raw.as_bytes()).unwrap();
        input.range = (0, raw.len() as u64);
        input.row_digest = blake3::hash(raw.as_bytes()).to_hex().to_string();
        assert!(!historical_input_verified(&changed), "{}", record["type"]);
    }
}

#[test]
fn historical_input_rejects_truncated_torn_and_excessive_ranges() {
    let (_directory, row) = fixture();
    let mut changed = row.clone();
    changed.native_stop_user_pin.as_mut().unwrap().input.range.1 += 1;
    assert!(!historical_input_verified(&changed));
    changed.native_stop_user_pin.as_mut().unwrap().input.range = (0, 16 * 1024 * 1024 + 1);
    assert!(!historical_input_verified(&changed));
    let input = &row.native_stop_user_pin.as_ref().unwrap().input;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&input.output_path)
        .unwrap()
        .set_len(input.range.1 - 1)
        .unwrap();
    assert!(!historical_input_verified(&row));
    let mut torn = row.clone();
    let input = &mut torn.native_stop_user_pin.as_mut().unwrap().input;
    let bytes = std::fs::read(&input.output_path).unwrap();
    input.range.1 -= 1;
    input.row_digest = blake3::hash(&bytes[input.range.0 as usize..])
        .to_hex()
        .to_string();
    assert!(!historical_input_verified(&torn));
}

#[test]
fn historical_input_rejects_replaced_paths_and_symlinks() {
    let (directory, row) = fixture();
    let input = &row.native_stop_user_pin.as_ref().unwrap().input;
    let replacement = directory.path().join("replacement.jsonl");
    std::fs::copy(&input.output_path, &replacement).unwrap();
    std::fs::rename(&replacement, &input.output_path).unwrap();
    assert!(!historical_input_verified(&row));
    let link = directory.path().join("transcript-link");
    std::os::unix::fs::symlink(&input.output_path, &link).unwrap();
    let mut linked = row.clone();
    let input = &mut linked.native_stop_user_pin.as_mut().unwrap().input;
    input.output_path = link.display().to_string();
    input.source_file.1 = std::fs::metadata(&input.output_path).unwrap().ino();
    assert!(!historical_input_verified(&linked));
}
