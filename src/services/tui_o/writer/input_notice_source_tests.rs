use super::*;
use crate::services::tui_o::writer::input_notices::Projection;

fn input(uuid: &str, prompt_id: &str) -> Vec<u8> {
    let mut line = serde_json::to_vec(&serde_json::json!({"type":"user","uuid":uuid,
        "promptId":prompt_id,"message":{"role":"user","content":"same request"}}))
    .unwrap();
    line.push(b'\n');
    line
}

#[tokio::test(start_paused = true)]
async fn adopted_root_projects_each_native_uuid_with_its_first_binding_on_replay() {
    let (harness, path, source, bindings) = started(b"");
    let first = input("root-one", "shared-hook");
    let second = input("root-two", "shared-hook");
    append(&path, &[first.clone(), second.clone()].concat());
    for replay in 0..2 {
        let mut writer = harness.writer();
        let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude)
            .with_input_notices(Projection::for_tests());
        let mut owed = RecordingOwe::default();
        sources
            .resume(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        sources.follow(&mut writer).unwrap();
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(writer.store().rotation().unwrap().link(&source).is_none());
        let pieces: Vec<_> = owed
            .derived
            .iter()
            .filter_map(|derived| match derived {
                Derived::Piece(piece) if piece.unit_key.kind == UnitKind::InputNotice => {
                    Some(piece)
                }
                _ => None,
            })
            .collect();
        assert_eq!(pieces.len(), 2, "an adopted root must project both UUIDs");
        assert_eq!(pieces[0].unit_key.native_key, "root-one");
        assert_eq!(pieces[1].unit_key.native_key, "root-two");
        assert_eq!(pieces[0].payload, pieces[1].payload);
        for (piece, start, end) in [
            (pieces[0], 0, first.len()),
            (pieces[1], first.len(), first.len() + second.len()),
        ] {
            let origin = piece.notice_origin.as_ref().unwrap();
            assert_eq!(origin.record.source, source);
            assert_eq!(
                origin.record.start..origin.record.end,
                start as u64..end as u64
            );
            assert_eq!(origin.binding_seq, 1);
            assert_eq!(origin.logical_session, "tmux");
            assert_eq!(origin.execution_nonce, "nonce");
        }
        assert_eq!(
            writer.store().cursor(&source).unwrap().captured_through,
            std::fs::metadata(&path).unwrap().len()
        );
        assert!(harness.port.posts().is_empty());
        assert_eq!(writer.store().ledger().next_serial(), 0);
        if replay == 0 {
            let mut rebound = bound(
                2,
                Some(&source),
                BindingTarget::Source(source.clone()),
                BindingCause::Resume,
                None,
            );
            rebound.tmux_session = "new-logical-pane".into();
            rebound.execution_nonce = "nonce-new".into();
            bindings.commit(rebound);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn inherited_rows_are_masked_before_uuid_notice_projection_on_live_and_replay() {
    let old = input("old-uuid", "old-hook");
    let (harness, parent_path, parent, bindings) = started(&old);
    let inherited = input("old-uuid", "fork-hook");
    let first = input("new-one", "fork-hook");
    let late_inherited = input("old-uuid", "fork-hook");
    let second = input("new-two", "fork-hook");
    let (path, child) = transcript(
        &parent_path,
        "child.jsonl",
        "child-native",
        &[
            inherited.clone(),
            first.clone(),
            late_inherited.clone(),
            second.clone(),
        ]
        .concat(),
    );
    bindings.commit(rotate(2, &parent, &child, BindingCause::Fork));
    for replay in 0..2 {
        let mut writer = harness.writer();
        let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude)
            .with_input_notices(Projection::for_tests());
        let mut owed = RecordingOwe::default();
        sources
            .resume(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        sources.follow(&mut writer).unwrap();
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(!owed.saw(&child, 0, inherited.len() as u64));
        let late_start = inherited.len() + first.len();
        let late_end = late_start + late_inherited.len();
        assert!(
            !owed.saw(&child, late_start as u64, late_end as u64),
            "an inherited row after the first new input must stay masked"
        );
        let pieces: Vec<_> = owed
            .derived
            .iter()
            .filter_map(|derived| match derived {
                Derived::Piece(piece) if piece.unit_key.kind == UnitKind::InputNotice => {
                    Some(piece)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            pieces.len(),
            2,
            "Sources must project both new UUIDs after masking"
        );
        assert_eq!(
            pieces
                .iter()
                .map(|p| p.unit_key.native_key.as_str())
                .collect::<Vec<_>>(),
            ["new-one", "new-two"]
        );
        assert_eq!(pieces[0].payload, pieces[1].payload);
        for (piece, start, end) in [
            (pieces[0], inherited.len(), inherited.len() + first.len()),
            (pieces[1], late_end, late_end + second.len()),
        ] {
            let origin = piece.notice_origin.as_ref().unwrap();
            assert_eq!(origin.record.source, child);
            assert_eq!(
                origin.record.start..origin.record.end,
                start as u64..end as u64
            );
            assert_eq!(origin.binding_seq, 2);
            assert_eq!(origin.logical_session, "tmux");
            assert_eq!(origin.execution_nonce, "nonce");
        }
        assert_eq!(
            writer.store().cursor(&child).unwrap().captured_through,
            std::fs::metadata(&path).unwrap().len()
        );
        assert!(
            harness.port.posts().is_empty(),
            "source probing does not POST"
        );
        if replay == 0 {
            let mut rebound = bound(
                3,
                Some(&child),
                BindingTarget::Source(child.clone()),
                BindingCause::Resume,
                None,
            );
            rebound.execution_nonce = "nonce-new".into();
            bindings.commit(rebound);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn legacy_binding_without_execution_provenance_stays_notice_debt_without_blocking_body() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let (harness, path, source, bindings) = started(b"");
    bindings.events.lock().unwrap()[0].execution_nonce.clear();
    let user = input("legacy-input", "legacy-hook");
    append(&path, &[user.clone(), row("later", "later body")].concat());
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    let record = crate::services::tui_o::shadow::CapturedRecord {
        start: 0,
        end: user.len() as u64,
        line: user[..user.len() - 1].to_vec(),
    };
    assert!(
        sources.notice_origin(&source, &record).is_err(),
        "a legacy missing nonce must remain withheld, not become invalid durable origin"
    );
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude)
        .with_input_notices(Projection::for_tests());
    let mut owed = RecordingOwe::default();
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    assert!(!writer.is_stopped());
    assert!(
        !owed
            .derived
            .iter()
            .any(|d| matches!(d, Derived::Blocked { .. }))
    );
    assert!(
        owed.derived
            .iter()
            .any(|d| matches!(d,Derived::Piece(p) if p.unit_key.kind==UnitKind::Body))
    );
    let retained = deriver.take_notice_evidence();
    assert_eq!(retained.len(), 1);
    assert!(retained[0].origin.is_none());
    assert_eq!(retained[0].raw_record, record.line);
}
