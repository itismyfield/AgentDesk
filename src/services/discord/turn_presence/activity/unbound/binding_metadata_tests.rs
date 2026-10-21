use super::*;

const CHANNEL: u64 = 69_182_001;
const TMUX: &str = "AgentDesk-codex-binding-metadata-test";

fn context() -> binding_context::BindingContext {
    binding_context::BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: "0123456789abcdef0123456789abcdef".into(),
        tmux_session: TMUX.into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: "/private/test-runtime".into(),
        host: Some("private-test-host".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some("/private/test-provider".into()),
        first_prompt_digest: None,
        source_policy: Some("verified".into()),
    }
}

fn empty_candidates() -> Candidates {
    Candidates {
        channel: CHANNEL,
        provider: ProviderKind::Codex,
        current: None,
        names: Vec::new(),
        conflict: false,
        inflight: false,
    }
}

fn selected() -> Snapshot {
    Snapshot {
        records: Vec::new(),
        name: Some(Name {
            seq: 7,
            context: context(),
        }),
    }
}

#[test]
fn metadata_supplements_only_a_name_without_installing_current_or_source() {
    let mut candidates = empty_candidates();
    let snapshot = selected();
    snapshot.supplement(&mut candidates).unwrap();
    snapshot.supplement(&mut candidates).unwrap();
    assert!(candidates.current.is_none());
    assert_eq!(
        candidates.names,
        vec![Candidate {
            name: TMUX.into(),
            origin: Origin::Binding(7),
        }]
    );
    assert!(!candidates.conflict && !candidates.inflight);
}

#[test]
fn metadata_rejects_scope_and_existing_name_conflicts_before_supplementing() {
    for variant in 0..3 {
        let mut candidates = empty_candidates();
        match variant {
            0 => candidates.channel += 1,
            1 => candidates.provider = ProviderKind::Claude,
            _ => candidates.names.push(Candidate {
                name: "AgentDesk-codex-conflicting-name".into(),
                origin: Origin::Binding(6),
            }),
        }
        let before = candidates.clone();
        assert!(selected().supplement(&mut candidates).is_err());
        assert_eq!(candidates, before);
    }
}

#[test]
fn metadata_snapshot_pins_context_nonce_even_when_the_candidate_name_does_not_change() {
    let first = selected();
    let mut changed = first.clone();
    changed.name.as_mut().unwrap().context.execution_nonce =
        "1123456789abcdef0123456789abcdef".into();
    assert_ne!(first, changed);
    let mut changed = first.clone();
    changed.name.as_mut().unwrap().context.owner_runtime_root = "/private/another-runtime".into();
    assert_ne!(first, changed);
}

fn pending(seq: u64) -> p5::BindingEvent {
    let now = chrono::Utc::now();
    p5::BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: "codex".into(),
        tmux_session: TMUX.into(),
        execution_nonce: Some(context().execution_nonce),
        old: None,
        new: p5::BindingTarget::Pending {
            payload_session_id: "019e660d-4859-7522-9cee-8ba7c4e7c743".into(),
            payload_transcript_path: Some("/private/no-source-body-is-opened.jsonl".into()),
        },
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: now,
        },
        committed_at: now,
    }
}

#[test]
fn strict_metadata_refuses_duplicate_gap_foreign_and_unreadable_records() {
    let root = tempfile::tempdir().unwrap();
    let _root =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let dir = root.path().join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CHANNEL}.log"));
    for variant in 0..6 {
        let first = pending(1);
        let mut second = pending(2);
        match variant {
            0 => second.seq = 1,
            1 => second.seq = 3,
            2 => second.channel_id += 1,
            3 => second.provider = "claude".into(),
            4 => second.tmux_session.clear(),
            _ => {}
        }
        let second = if variant == 5 {
            "unreadable record".to_owned()
        } else {
            serde_json::to_string(&second).unwrap()
        };
        std::fs::write(
            &path,
            format!("{}\n{second}\n", serde_json::to_string(&first).unwrap()),
        )
        .unwrap();
        for needed in [true, false] {
            let mut candidates = empty_candidates();
            if !needed {
                candidates.current = Some(TMUX.into());
            }
            assert!(
                collect(CHANNEL, &ProviderKind::Codex, &[], &Ok(candidates)).is_err(),
                "invalid metadata variant {variant} was accepted, needed={needed}"
            );
        }
    }
}

#[test]
fn known_candidates_do_not_read_ownership_while_pinning_strict_metadata() {
    let root = tempfile::tempdir().unwrap();
    let _root =
        crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(root.path());
    let dir = root.path().join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let mut event = pending(1);
    event.new = p5::BindingTarget::Source(p5::SourceId {
        session_id: "019e660d-4859-7522-9cee-8ba7c4e7c743".into(),
        path: "/private/no-source-body-is-opened.jsonl".into(),
        dev: 1,
        ino: 2,
    });
    std::fs::write(
        dir.join(format!("{CHANNEL}.log")),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    let delivery = BindingEvent {
        seq: event.seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Codex,
        tmux_session: TMUX.into(),
        execution_nonce: context().execution_nonce,
        record: BindingRecord::Rejected {
            detail: "old source remains denied".into(),
            closes: None,
        },
        committed_at: event.committed_at,
    };
    for variant in 0..5 {
        let mut candidates = empty_candidates();
        match variant {
            0 => candidates.current = Some(TMUX.into()),
            1 => candidates.names.push(Candidate {
                name: TMUX.into(),
                origin: Origin::Binding(1),
            }),
            2 => candidates.inflight = true,
            3 => candidates.conflict = true,
            _ => {}
        }
        let candidates = if variant == 4 {
            Err("existing candidate failure".into())
        } else {
            Ok(candidates)
        };
        OWNERSHIP_LOOKUPS.with(|count| count.set(0));
        let snapshot = collect(
            CHANNEL,
            &ProviderKind::Codex,
            std::slice::from_ref(&delivery),
            &candidates,
        )
        .expect("known candidates must not require absence ownership metadata");
        assert!(snapshot.name.is_none());
        assert_eq!(snapshot.records, vec![event.clone()]);
        assert_eq!(OWNERSHIP_LOOKUPS.with(std::cell::Cell::get), 0);
    }
}
