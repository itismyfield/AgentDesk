#![cfg(unix)]

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use codex::{Claim, ClaimEvidence, Decision, Fold};

const PARENT: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";
const OTHER: &str = "019e660d-4859-7522-9cee-8ba7c4e7c744";

struct Fixture {
    root: tempfile::TempDir,
    context: BindingContext,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        set_test_root(Some(root.path()));
        let sessions = root.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: Utc::now(),
            execution_nonce: "a".repeat(32),
            tmux_session: format!("claim-fixture-{}", uuid::Uuid::new_v4()),
            channel_id: Some(584_503),
            owner_runtime_root: root.path().display().to_string(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(sessions),
            first_prompt_digest: Some(format!("sha256:{}", "a".repeat(64))),
            source_policy: Some("verified".into()),
        };
        Self { root, context }
    }

    fn source(&self, session: &str) -> SourceId {
        let path = self
            .context
            .provider_root
            .as_ref()
            .unwrap()
            .join(format!("rollout-{session}.jsonl"));
        fs::write(&path, format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"source\":\"cli\"}}}}\n")).unwrap();
        source_id(
            Some(session),
            path.to_str().unwrap(),
            file_identity(&fs::metadata(&path).unwrap()),
        )
    }

    fn claim(&self, source: &SourceId) -> Claim {
        Claim {
            session_id: source.session_id.clone(),
            path: Some(source.path.clone()),
            evidence: ClaimEvidence::NativeHook {
                event: "session_start".into(),
                source: Some("startup".into()),
                first_prompt_digest: None,
            },
        }
    }

    fn commit(&self, claim: &Claim, decision: Decision) -> Fold {
        crate::services::tmux_common::with_tmux_source_authority(&self.context.tmux_session, |_| {
            codex::commit_claim(&self.context, claim, decision, || Ok(())).unwrap()
        })
    }

    fn path(&self) -> PathBuf {
        self.root.path().join(BINDING_EVENTS_DIR).join("584503.log")
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(self.path()).unwrap_or_default()
    }

    fn generic(&self, source: &SourceId, nonce: &str, old: Option<SourceId>, cause: BindingCause) {
        commit_with(584_503, |writer| {
            Planned::Append(
                BindingEvent {
                    seq: writer.last_seq + 1,
                    channel_id: 584_503,
                    provider: "codex".into(),
                    tmux_session: self.context.tmux_session.clone(),
                    execution_nonce: Some(nonce.into()),
                    old,
                    new: BindingTarget::Source(source.clone()),
                    cause,
                    parent_hint: None,
                    evidence: BindingEvidence {
                        hook_event: None,
                        received_at: Utc::now(),
                    },
                    committed_at: Utc::now(),
                },
                false,
                None,
            )
        })
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        APPEND_FAULT.with(|fault| fault.set(None));
        forget_channel_for_tests(584_503);
        set_test_root(None);
    }
}

#[test]
fn same_native_generic_is_unverified_until_a_typed_claim_commits() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    fixture.generic(
        &source,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    let legacy_bytes = fixture.bytes();
    let legacy = codex::read_ownership(&fixture.context).unwrap();
    assert!(legacy.verified.is_none());
    assert_eq!(legacy.legacy_unverified.len(), 1);
    assert_eq!(fixture.bytes(), legacy_bytes, "fold must be read-only");
    let mut watch = subscribe_binding_events(584_503).unwrap();
    watch.borrow_and_update();
    let claim = fixture.claim(&source);
    let proof = fixture.commit(&claim, Decision::Verified(source.clone()));
    assert_eq!(proof.verified.as_ref().unwrap().source, source);
    assert_eq!(proof.verified.as_ref().unwrap().seq, 2);
    assert!(watch.has_changed().unwrap());
    watch.borrow_and_update();
    let committed = fixture.bytes();
    let retry = fixture.commit(&claim, Decision::Verified(source));
    assert_eq!(retry, proof);
    assert_eq!(fixture.bytes(), committed);
    assert!(
        !watch.has_changed().unwrap(),
        "same proof does not wake a reader twice"
    );
    forget_channel_for_tests(584_503);
    assert_eq!(codex::read_ownership(&fixture.context).unwrap(), proof);
}

#[test]
fn pending_survives_restart_and_child_rejection_unblocks_the_parent() {
    for child_first in [false, true] {
        let fixture = Fixture::new();
        let parent = fixture.source(PARENT);
        let child = fixture.source(OTHER);
        let parent_claim = fixture.claim(&parent);
        let child_claim = fixture.claim(&child);
        let claims = if child_first {
            [&child_claim, &parent_claim]
        } else {
            [&parent_claim, &child_claim]
        };
        for claim in claims {
            fixture.commit(claim, Decision::Pending);
        }
        let original = fixture.bytes();
        fixture.commit(&parent_claim, Decision::Pending);
        assert_eq!(fixture.bytes(), original);
        forget_channel_for_tests(584_503);
        let conflict = codex::read_ownership(&fixture.context).unwrap();
        assert!(conflict.conflicted);
        assert_eq!(conflict.pending.len(), 2);
        let held = fixture.commit(&parent_claim, Decision::Verified(parent.clone()));
        assert!(held.verified.is_none());
        let remaining = fixture.commit(&child_claim, Decision::Rejected("subagent".into()));
        assert!(!remaining.conflicted);
        assert_eq!(remaining.pending.len(), 1);
        assert_eq!(remaining.pending[0].claim, parent_claim);
        let resolved = fixture.commit(&parent_claim, Decision::Verified(parent.clone()));
        assert!(resolved.pending.is_empty());
        assert_eq!(resolved.verified.unwrap().source, parent);
    }
}

#[test]
fn later_claim_preserves_current_proof_and_two_parents_never_pick_a_winner() {
    for reverse in [false, true] {
        let fixture = Fixture::new();
        let (a, b) = if reverse {
            (OTHER, PARENT)
        } else {
            (PARENT, OTHER)
        };
        let parent = fixture.source(a);
        let other = fixture.source(b);
        let parent_claim = fixture.claim(&parent);
        let other_claim = fixture.claim(&other);
        let first = fixture.commit(&parent_claim, Decision::Verified(parent.clone()));
        let conflict = fixture.commit(&other_claim, Decision::Verified(other));
        assert!(conflict.conflicted);
        assert_eq!(
            conflict.verified, first.verified,
            "conflict must retain the old proof as evidence"
        );
        assert_eq!(conflict.pending.len(), 1);
        let bytes = fixture.bytes();
        fixture.commit(&other_claim, Decision::Pending);
        assert_eq!(fixture.bytes(), bytes);
        let remaining = fixture.commit(&other_claim, Decision::Rejected("subagent".into()));
        assert!(!remaining.conflicted);
        assert_eq!(remaining.verified, first.verified);
    }
}

#[test]
fn legacy_replacement_prior_native_and_lifecycle_do_not_become_first_proof() {
    for case in 0..4 {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let other = fixture.source(OTHER);
        match case {
            0 => fixture.generic(&source, &"b".repeat(32), None, BindingCause::Unknown),
            1 => fixture.generic(
                &other,
                &fixture.context.execution_nonce,
                None,
                BindingCause::Startup,
            ),
            2 => {
                fixture.generic(
                    &other,
                    &fixture.context.execution_nonce,
                    None,
                    BindingCause::Unknown,
                );
                fixture.generic(
                    &source,
                    &fixture.context.execution_nonce,
                    Some(other),
                    BindingCause::Unknown,
                );
            }
            _ => fixture.generic(
                &source,
                &fixture.context.execution_nonce,
                None,
                BindingCause::Clear,
            ),
        }
        let folded = fixture.commit(&fixture.claim(&source), Decision::Verified(source));
        assert!(
            folded.verified.is_none(),
            "legacy case {case} must not create ownership"
        );
        assert_eq!(folded.pending.len(), 1);
    }
    let fixture = Fixture::new();
    let old = fixture.source(OTHER);
    let source = fixture.source(PARENT);
    fixture.generic(&old, &"b".repeat(32), None, BindingCause::Unknown);
    fixture.generic(
        &source,
        &fixture.context.execution_nonce,
        Some(old),
        BindingCause::Unknown,
    );
    assert!(
        fixture
            .commit(&fixture.claim(&source), Decision::Verified(source))
            .verified
            .is_some()
    );
}

#[test]
fn initial_prompt_resume_and_unsupported_lifecycle_keep_their_qualification() {
    for (event, source_reason, digest, allowed) in [
        ("user_prompt_submit", None, None, false),
        (
            "user_prompt_submit",
            None,
            Some(format!("sha256:{}", "b".repeat(64))),
            false,
        ),
        (
            "user_prompt_submit",
            None,
            Some(format!("sha256:{}", "a".repeat(64))),
            true,
        ),
        ("session_start", Some("clear"), None, false),
        ("session_start", Some("resume"), None, false),
        ("session_start", Some("fork"), None, false),
        ("session_start", Some("compact"), None, false),
        ("stop", None, None, false),
    ] {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let mut claim = fixture.claim(&source);
        claim.evidence = ClaimEvidence::NativeHook {
            event: event.into(),
            source: source_reason.map(str::to_owned),
            first_prompt_digest: digest,
        };
        assert_eq!(
            fixture
                .commit(&claim, Decision::Verified(source))
                .verified
                .is_some(),
            allowed,
            "{event}/{source_reason:?}"
        );
    }
    for executed in [PARENT, OTHER] {
        let mut fixture = Fixture::new();
        fixture.context.launch_mode = "resume".into();
        fixture.context.expected_native_session_id = Some(PARENT.into());
        let source = fixture.source(PARENT);
        let mut claim = fixture.claim(&source);
        claim.evidence = ClaimEvidence::ExplicitResume {
            executed_session_id: executed.into(),
        };
        assert_eq!(
            fixture
                .commit(&claim, Decision::Verified(source))
                .verified
                .is_some(),
            executed == PARENT
        );
    }
}

#[test]
fn same_uuid_path_and_descriptor_replacement_are_not_idempotent_proofs() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let claim = fixture.claim(&source);
    fixture.commit(&claim, Decision::Verified(source.clone()));
    let replacement = source.path.with_extension("replacement");
    fs::write(&replacement, b"replacement\n").unwrap();
    fs::rename(&replacement, &source.path).unwrap();
    let before = fixture.bytes();
    assert!(
        codex::commit_claim(
            &fixture.context,
            &claim,
            Decision::Verified(source.clone()),
            || Ok(())
        )
        .is_err()
    );
    assert_eq!(fixture.bytes(), before);
    let changed = source_id(
        Some(PARENT),
        source.path.to_str().unwrap(),
        file_identity(&fs::metadata(&source.path).unwrap()),
    );
    assert_ne!((changed.dev, changed.ino), (source.dev, source.ino));
    assert!(
        fixture
            .commit(&claim, Decision::Verified(changed))
            .conflicted
    );
}

#[test]
fn symlink_alias_generic_uses_open_identity_and_canonical_path() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let alias = source.path.with_extension("alias");
    std::os::unix::fs::symlink(&source.path, &alias).unwrap();
    let mut generic = source.clone();
    generic.path = alias;
    fixture.generic(
        &generic,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    assert!(
        fixture
            .commit(&fixture.claim(&source), Decision::Verified(source))
            .verified
            .is_some()
    );
}

#[test]
fn commit_revalidates_current_context_after_the_optimistic_read() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let claim = fixture.claim(&source);
    assert!(
        codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .is_none()
    );
    let result = codex::commit_claim(&fixture.context, &claim, Decision::Verified(source), || {
        Err(io::Error::other("spawn nonce changed"))
    });
    assert!(result.is_err(), "a stale observation cannot commit");
    assert!(fixture.bytes().is_empty());
    assert!(
        codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .is_none()
    );
}

#[test]
fn failed_durable_append_never_publishes_proof_or_watch_notification() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let claim = fixture.claim(&source);
    let mut watch = subscribe_binding_events(584_503).unwrap();
    watch.borrow_and_update();
    for fault in ["write", "sync"] {
        APPEND_FAULT.with(|slot| slot.set(Some(fault)));
        let result = codex::commit_claim(
            &fixture.context,
            &claim,
            Decision::Verified(source.clone()),
            || Ok(()),
        );
        APPEND_FAULT.with(|slot| slot.set(None));
        assert!(result.is_err(), "{fault} must fail");
        assert!(fixture.bytes().is_empty());
        assert!(!watch.has_changed().unwrap());
        assert!(
            codex::read_ownership(&fixture.context)
                .unwrap()
                .verified
                .is_none()
        );
    }
    assert!(
        fixture
            .commit(&claim, Decision::Verified(source))
            .verified
            .is_some()
    );
    assert!(watch.has_changed().unwrap());
}

#[test]
fn strict_history_rejects_corruption_even_outside_the_requested_nonce() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let claim = fixture.claim(&source);
    fixture.commit(&claim, Decision::Pending);
    fixture.commit(&claim, Decision::Verified(source.clone()));
    let original = fixture.bytes();
    let records: Vec<serde_json::Value> = original
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    for mutation in 0..10 {
        let mut damaged = records.clone();
        let last = &mut damaged[1];
        match mutation {
            0 => last["seq"] = 9.into(),
            1 => last["codex_ownership"]["schema"] = 2.into(),
            2 => last["codex_ownership"]["future"] = true.into(),
            3 => last["codex_ownership"]["seq"] = 1.into(),
            4 => last["codex_ownership"]["context"]["execution_nonce"] = "b".repeat(32).into(),
            5 => last["codex_ownership"]["claim"]["session_id"] = OTHER.into(),
            6 => last["codex_ownership"]["pending_seq"] = 27.into(),
            7 => last["cause"] = "fork".into(),
            8 => last["channel_id"] = 1.into(),
            _ => damaged[0] = serde_json::Value::String("corrupt".into()),
        }
        let bytes: Vec<u8> = damaged
            .iter()
            .flat_map(|record| {
                let mut bytes = serde_json::to_vec(record).unwrap();
                bytes.push(b'\n');
                bytes
            })
            .collect();
        fs::write(fixture.path(), &bytes).unwrap();
        let mut other = fixture.context.clone();
        other.execution_nonce = "c".repeat(32);
        assert!(
            codex::read_ownership(&other).is_err(),
            "corruption {mutation} must not be filtered"
        );
        assert!(
            codex::commit_claim(
                &fixture.context,
                &claim,
                Decision::Verified(source.clone()),
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(
            fixture.bytes(),
            bytes,
            "a rejected commit must preserve the corrupt evidence"
        );
        fs::write(fixture.path(), &original).unwrap();
    }
}

#[test]
fn optional_ownership_preserves_old_parser_and_channel_sequences() {
    #[derive(Deserialize)]
    struct OldEvent {
        seq: u64,
        new: OldTarget,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum OldTarget {
        Source(SourceId),
        Pending {
            payload_session_id: String,
            payload_transcript_path: Option<String>,
        },
        Resolved {
            pending_seq: u64,
            source: SourceId,
        },
        Rejected {
            payload_session_id: String,
            payload_transcript_path: Option<String>,
            reason: String,
        },
    }
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let claim = fixture.claim(&source);
    fixture.generic(
        &source,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    fixture.commit(&claim, Decision::Pending);
    fixture.commit(&claim, Decision::Verified(source));
    let bytes = fixture.bytes();
    let old: Vec<OldEvent> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(
        old.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    match &old[2].new {
        OldTarget::Resolved {
            pending_seq,
            source,
        } => {
            assert_eq!(*pending_seq, 2);
            assert_eq!(source.session_id, PARENT);
        }
        _ => panic!("old parser did not read Resolved"),
    }
    assert_eq!(binding_events_since(584_503, 0).unwrap().len(), 3);
    let legacy: serde_json::Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert!(legacy.get("codex_ownership").is_none());
    assert!(legacy.get("verified").is_none());
}

#[test]
fn later_explicit_resume_does_not_invalidate_the_original_fresh_proof() {
    let mut fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let first_context = fixture.context.clone();
    let original = fixture.commit(&fixture.claim(&source), Decision::Verified(source.clone()));
    fixture.context.execution_nonce = "b".repeat(32);
    fixture.context.launch_mode = "resume".into();
    fixture.context.expected_native_session_id = Some(PARENT.into());
    let mut resume = fixture.claim(&source);
    resume.evidence = ClaimEvidence::ExplicitResume {
        executed_session_id: PARENT.into(),
    };
    let resumed = fixture.commit(&resume, Decision::Verified(source.clone()));
    assert_eq!(resumed.verified.unwrap().source, source);
    assert_eq!(codex::read_ownership(&first_context).unwrap(), original);
    fixture.context.execution_nonce = "c".repeat(32);
    fixture.context.launch_mode = "fresh".into();
    fixture.context.expected_native_session_id = None;
    assert!(
        fixture
            .commit(&fixture.claim(&source), Decision::Verified(source))
            .verified
            .is_none()
    );
}

#[test]
fn legacy_and_shadow_contexts_cannot_commit_typed_ownership() {
    for policy in [None, Some("legacy"), Some("shadow")] {
        let mut fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let claim = fixture.claim(&source);
        fixture.context.source_policy = policy.map(str::to_owned);
        let mut validated = false;
        let result =
            codex::commit_claim(&fixture.context, &claim, Decision::Verified(source), || {
                validated = true;
                Ok(())
            });
        assert!(result.is_err());
        assert!(!validated, "inactive context must not enter preparation");
        assert!(!fixture.path().exists());
    }
}

#[test]
fn generic_same_uuid_with_a_different_open_file_is_not_neutral() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let duplicate_path = source.path.with_extension("duplicate.jsonl");
    fs::copy(&source.path, &duplicate_path).unwrap();
    let duplicate = source_id(
        Some(PARENT),
        duplicate_path.to_str().unwrap(),
        file_identity(&fs::metadata(&duplicate_path).unwrap()),
    );
    fixture.generic(
        &duplicate,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    let folded = fixture.commit(&fixture.claim(&source), Decision::Verified(source));
    assert!(
        folded.verified.is_none(),
        "UUID equality cannot replace opened identity"
    );
}

#[test]
fn typed_proof_does_not_activate_or_replace_legacy_mtime_selection() {
    use crate::services::codex_tui::{
        rollout_index::lock_cache_for_tests, rollout_tail::latest_rollout_for_cwd_since,
        session::source_observation::CodexSourceMode,
    };
    let _cache = lock_cache_for_tests();
    let fixture = Fixture::new();
    let older = fixture.source(PARENT);
    let newer = fixture.source(OTHER);
    let base = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    for (source, seconds) in [(&older, 10), (&newer, 20)] {
        let record = serde_json::json!({"type": "session_meta", "payload": {
            "id": source.session_id, "source": "cli", "cwd": fixture.root.path()
        }});
        fs::write(&source.path, format!("{record}\n")).unwrap();
        filetime::set_file_mtime(
            &source.path,
            filetime::FileTime::from_system_time(base + std::time::Duration::from_secs(seconds)),
        )
        .unwrap();
    }
    let proof = fixture.commit(&fixture.claim(&older), Decision::Verified(older.clone()));
    assert_eq!(proof.verified.unwrap().source, older);
    assert_eq!(
        latest_rollout_for_cwd_since(
            fixture.root.path(),
            base,
            fixture.context.provider_root.as_ref().unwrap(),
        ),
        Some(newer.path),
        "a durable typed proof must not change the production selector in P3a"
    );
    assert_eq!(
        CodexSourceMode::Verified.launch_policy(),
        Err("SourceModeVerifiedNotLanded")
    );
}
