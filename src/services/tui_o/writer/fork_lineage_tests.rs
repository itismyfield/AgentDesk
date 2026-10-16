use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;

fn json(value: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn prompt(id: &str) -> Vec<u8> {
    json(serde_json::json!({"type":"user","uuid":id,"message":{"content":"한글"}}))
}

fn scan(body: &[u8]) -> (tempfile::TempDir, SourceId, Lineage) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.jsonl");
    std::fs::write(&path, body).unwrap();
    let source = source_id_for("parent", &path).unwrap();
    let lineage = sync(None, ShadowProvider::Claude, &source, body.len() as u64).unwrap();
    (root, source, lineage)
}

fn row(id: &str) -> RowIds {
    RowIds::of(ShadowProvider::Claude, &prompt(id)).unwrap()
}

#[test]
fn repeated_ids_use_maximum_parent_start_and_include_the_exact_boundary() {
    let r = prompt("r");
    let padding = b" \n{invalid}\n\n";
    let second_start = (r.len() + padding.len()) as u64;
    let body = [r.as_slice(), padding, r.as_slice()].concat();
    let (_root, _source, lineage) = scan(&body);
    assert_eq!(lineage.class(&row("r")), Class::Inherited);
    assert_eq!(
        lineage.class_from(&row("r"), second_start),
        Class::Inherited
    );
    assert_eq!(lineage.class_from(&row("r"), second_start + 1), Class::New);
    assert_eq!(
        lineage.class_from(&row("r"), body.len() as u64),
        Class::New,
        "end==from is excluded"
    );
}

#[test]
fn every_byte_split_preserves_offsets_and_never_promotes_an_incomplete_tail() {
    let first = prompt("first");
    let second = prompt("second");
    let tail = prompt("tail");
    let body = [
        first.as_slice(),
        b" \nnot json\n\n",
        second.as_slice(),
        &tail[..tail.len() - 1],
    ]
    .concat();
    let (_root, source, full) = scan(&body);
    assert_eq!(full.class(&row("tail")), Class::New);
    let second_start = (first.len() + b" \nnot json\n\n".len()) as u64;
    assert_eq!(
        full.class_from(&row("second"), second_start),
        Class::Inherited
    );
    assert_eq!(
        full.class_from(&row("second"), second_start + 1),
        Class::New
    );
    for split in 0..=body.len() {
        let head = sync(None, ShadowProvider::Claude, &source, split as u64).unwrap();
        let incremented = sync(
            Some(head),
            ShadowProvider::Claude,
            &source,
            body.len() as u64,
        )
        .unwrap();
        assert_eq!(incremented.ids, full.ids, "byte split {split}");
        assert_eq!(incremented.tail, full.tail, "byte split {split}");
        assert_eq!(incremented.through, full.through);
    }
}

#[test]
fn all_identities_and_each_announced_or_sealed_alternative_need_range_evidence() {
    let announcement = |id| {
        json(serde_json::json!({"type":"event_msg","payload":{
        "type":"item_completed","item":{"type":"AgentMessage","id":id}}}))
    };
    let sealed = |id| {
        json(serde_json::json!({"type":"response_item","payload":{
        "type":"message","role":"assistant","id":id,"content":[{"type":"output_text","text":"answer"}]}}))
    };
    let a = announcement("msg_a");
    let b = sealed("msg_b");
    let from = (a.len() + b.len()) as u64;
    let body = [a.clone(), b.clone(), sealed("msg_a"), announcement("msg_b")].concat();
    let (_root, source, _) = scan(&body);
    let lineage = sync(None, ShadowProvider::Codex, &source, body.len() as u64).unwrap();
    for bytes in [&a, &announcement("msg_b")] {
        let row = RowIds::of(ShadowProvider::Codex, bytes).unwrap();
        assert!(!row.is_empty());
        assert_eq!(lineage.class_from(&row, from), Class::Inherited);
    }
    let ranged = RowIds {
        row: None,
        units: Some(vec![
            ("msg_a".into(), UnitKind::Body, Seen::Sealed),
            ("msg_b".into(), UnitKind::Body, Seen::Sealed),
        ]),
    };
    assert_eq!(lineage.class(&ranged), Class::Inherited);
    assert_eq!(lineage.class_from(&ranged, from), Class::Unidentified);
    let lineage = sync(None, ShadowProvider::Codex, &source, from).unwrap();
    let row = RowIds::of(ShadowProvider::Codex, &announcement("msg_b")).unwrap();
    assert_eq!(
        lineage.class_from(&row, from),
        Class::New,
        "old Sealed does not bypass from"
    );
    let mixed = RowIds {
        row: None,
        units: Some(vec![
            ("msg_a".into(), UnitKind::Body, Seen::Announced),
            ("msg_new".into(), UnitKind::Body, Seen::Sealed),
        ]),
    };
    assert_eq!(lineage.class(&mixed), Class::Unidentified);
    let blocked = RowIds {
        row: Some("known".into()),
        units: None,
    };
    let (_root, _, lineage) = scan(&prompt("known"));
    assert_eq!(lineage.class_from(&blocked, 0), Class::Inherited);
    assert_eq!(lineage.class_from(&blocked, 1), Class::Unidentified);
    let empty = RowIds::of(ShadowProvider::Claude, b" \t").unwrap();
    assert!(empty.is_empty());
    assert_eq!(lineage.class(&empty), Class::New);
}

#[test]
fn warm_scanner_rechecks_identity_length_absence_and_scan_cap_without_more_bytes() {
    for damage in ["replace", "truncate", "remove", "cap"] {
        let body = prompt("r");
        let (_root, source, lineage) = scan(&body);
        let mut through = body.len() as u64;
        match damage {
            "replace" => {
                let swap = source.path.with_extension("swap");
                std::fs::write(&swap, &body).unwrap();
                std::fs::rename(swap, &source.path).unwrap();
            }
            "truncate" => std::fs::OpenOptions::new()
                .write(true)
                .open(&source.path)
                .unwrap()
                .set_len(0)
                .unwrap(),
            "remove" => std::fs::remove_file(&source.path).unwrap(),
            "cap" => {
                through = LINEAGE_SCAN_CAP_BYTES + 1;
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&source.path)
                    .unwrap()
                    .set_len(through)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            sync(Some(lineage), ShadowProvider::Claude, &source, through).is_none(),
            "{damage}"
        );
    }
}
