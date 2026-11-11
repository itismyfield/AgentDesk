use super::*;

fn stamp(root: &Path) -> AbsenceStamp {
    let canonical = root.join("canonical");
    let legacy = root.join("legacy");
    std::fs::write(&canonical, "codex_tui").unwrap();
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "token".into(),
        channel_id: "41".into(),
        logical_key: "AgentDesk-codex-proof".into(),
        owner_node: "node".into(),
        runtime_root: root.display().to_string(),
    };
    AbsenceStamp {
        owner: owner.clone(),
        expected: MarkerRowExpectation::Legacy {
            owner: (&owner).into(),
            row_id: 7,
            exact_session_key: "exact-key".into(),
        },
        front: QueueIdentity {
            primary: 41,
            queued_generation: 3,
            sources: vec![],
        },
        epoch: "epoch-5".into(),
        paths: [
            HintPath::observe(&canonical).unwrap(),
            HintPath::observe(&legacy).unwrap(),
        ],
        nonce: Ok(None),
        source: Ok(None),
    }
}

#[test]
fn marker_absence_requires_exact_row_owner_front_epoch_and_paths() {
    let root = tempfile::tempdir().unwrap();
    let original = stamp(root.path());
    assert!(original.matches(&original.clone()));
    let mut changed = Vec::new();
    let mut row = original.clone();
    let MarkerRowExpectation::Legacy { row_id, .. } = &mut row.expected else {
        unreachable!()
    };
    *row_id += 1;
    changed.push(row);
    let mut owner = original.clone();
    owner.owner.discord_token_hash.push('x');
    changed.push(owner);
    let mut key = original.clone();
    let MarkerRowExpectation::Legacy {
        exact_session_key, ..
    } = &mut key.expected
    else {
        unreachable!()
    };
    exact_session_key.push('x');
    changed.push(key);
    let mut front = original.clone();
    front.front.queued_generation += 1;
    changed.push(front);
    let mut epoch = original.clone();
    epoch.epoch.push('x');
    changed.push(epoch);
    let mut path = original.clone();
    path.paths.swap(0, 1);
    changed.push(path);
    for current in changed {
        assert!(!original.matches(&current));
    }
}

#[test]
fn marker_absence_distinguishes_initial_nonce_absence_from_loss_and_read_errors() {
    let root = tempfile::tempdir().unwrap();
    let absent = stamp(root.path());
    let mut known = absent.clone();
    known.nonce = Ok(Some("incarnation".into()));
    assert!(!known.matches(&absent));
    let mut unreadable = absent.clone();
    unreadable.nonce = Err("unreadable".into());
    assert!(!unreadable.matches(&unreadable));
    unreadable = absent.clone();
    unreadable.source = Err("unreadable".into());
    assert!(!unreadable.matches(&unreadable));
}

#[test]
fn marker_absence_keeps_native_source_execution_identity() {
    let root = tempfile::tempdir().unwrap();
    let mut original = stamp(root.path());
    original.source = Ok(Some(NativeSource {
        path: root.path().join("source.jsonl"),
        session: "session".into(),
        nonce: "nonce".into(),
        proof_seq: 8,
        declared_file: [1, 2],
        file: [1, 2],
    }));
    assert!(original.matches(&original));
    for field in 0..8 {
        let mut current = original.clone();
        let source = current.source.as_mut().unwrap().as_mut().unwrap();
        match field {
            0 => source.path = root.path().join("other.jsonl"),
            1 => source.session.push('x'),
            2 => source.nonce.push('x'),
            3 => source.proof_seq += 1,
            4 => source.file[0] += 1,
            5 => source.file[1] += 1,
            6 => source.declared_file[0] += 1,
            _ => source.declared_file[1] += 1,
        }
        assert!(!original.matches(&current), "field {field}");
    }
}

#[test]
fn marker_paths_reject_foreign_symlink_and_replaced_entries() {
    let root = tempfile::tempdir().unwrap();
    let original = stamp(root.path());
    let path = &original.paths[0].path;
    let replacement = root.path().join("replacement");
    std::fs::write(&replacement, "codex_tui").unwrap();
    std::fs::rename(&replacement, path).unwrap();
    assert_ne!(HintPath::observe(path).unwrap(), original.paths[0]);
    std::fs::write(path, "claude_tui").unwrap();
    assert!(HintPath::observe(path).is_err());
    std::fs::remove_file(path).unwrap();
    std::os::unix::fs::symlink(root.path().join("legacy"), path).unwrap();
    assert!(HintPath::observe(path).is_err());
}
