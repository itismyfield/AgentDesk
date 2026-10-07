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
        assert_eq!(
            fixture.commit(&parent_claim, Decision::Verified(parent)),
            conflict
        );
        assert_eq!(
            fixture.bytes(),
            bytes,
            "no matching Pending leaves the proof retry unchanged"
        );
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
    let mut fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let alias = source.path.with_extension("alias");
    std::os::unix::fs::symlink(&source.path, &alias).unwrap();
    let mut generic = source.clone();
    generic.path = alias.clone();
    fixture.generic(
        &generic,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    let claim = fixture.claim(&source);
    let proof = fixture.commit(&claim, Decision::Verified(source.clone()));
    assert_eq!(
        proof
            .verified
            .as_ref()
            .unwrap()
            .ownership
            .canonical_witnesses
            .len(),
        1
    );
    let bytes = fixture.bytes();
    fs::remove_file(alias).unwrap();
    assert_eq!(codex::read_ownership(&fixture.context).unwrap(), proof);
    forget_channel_for_tests(584_503);
    assert_eq!(codex::read_ownership(&fixture.context).unwrap(), proof);
    assert_eq!(
        fixture.commit(&claim, Decision::Verified(source.clone())),
        proof
    );
    assert_eq!(fixture.bytes(), bytes);
    fixture.commit(&claim, Decision::Pending);
    forget_channel_for_tests(584_503);
    let resolved = fixture.commit(&claim, Decision::Verified(source));
    assert!(!resolved.conflicted);
    assert!(
        resolved.pending.is_empty(),
        "prior witness survives same-proof resolution"
    );

    let old_context = fixture.context.clone();
    fixture.context.execution_nonce = "b".repeat(32);
    assert!(
        codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .is_none()
    );
    let other = fixture.source(OTHER);
    let other_proof = fixture.commit(&fixture.claim(&other), Decision::Verified(other));
    forget_channel_for_tests(584_503);
    assert_eq!(codex::read_ownership(&old_context).unwrap(), resolved);
    assert_eq!(
        codex::read_ownership(&fixture.context).unwrap(),
        other_proof
    );
}

#[test]
fn pending_order_does_not_consume_the_first_current_source_observation() {
    for pending_first in [false, true] {
        for restart in [false, true] {
            let fixture = Fixture::new();
            let old = fixture.source(OTHER);
            let source = fixture.source(PARENT);
            let claim = fixture.claim(&source);
            fixture.generic(&old, &"b".repeat(32), None, BindingCause::Unknown);
            for pending in [pending_first, !pending_first] {
                if pending {
                    fixture.commit(&claim, Decision::Pending);
                } else {
                    fixture.generic(
                        &source,
                        &fixture.context.execution_nonce,
                        Some(old.clone()),
                        BindingCause::Unknown,
                    );
                }
                if restart {
                    forget_channel_for_tests(584_503);
                    assert!(
                        codex::read_ownership(&fixture.context)
                            .unwrap()
                            .verified
                            .is_none()
                    );
                }
            }
            let resolved = fixture.commit(&claim, Decision::Verified(source.clone()));
            assert_eq!(resolved.verified.as_ref().unwrap().source, source);
            assert!(resolved.pending.is_empty());
            assert!(!resolved.conflicted);
            forget_channel_for_tests(584_503);
            assert_eq!(codex::read_ownership(&fixture.context).unwrap(), resolved);
        }
    }
}

#[test]
fn actual_current_source_replacement_still_blocks_a_returning_claim() {
    for pending_first in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let other = fixture.source(OTHER);
        let claim = fixture.claim(&source);
        if pending_first {
            fixture.commit(&claim, Decision::Pending);
        }
        for (new, old) in [
            (&source, None),
            (&other, Some(source.clone())),
            (&source, Some(other.clone())),
        ] {
            fixture.generic(
                new,
                &fixture.context.execution_nonce,
                old,
                BindingCause::Unknown,
            );
            forget_channel_for_tests(584_503);
        }
        let held = fixture.commit(&claim, Decision::Verified(source));
        assert!(held.verified.is_none());
        assert_eq!(held.pending.len(), 1);
    }
}

#[test]
fn same_verified_retry_resolves_a_later_matching_pending_once() {
    for restart in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let claim = fixture.claim(&source);
        let initial = fixture.commit(&claim, Decision::Verified(source.clone()));
        assert!(initial.pending.is_empty());
        if restart {
            forget_channel_for_tests(584_503);
        }
        assert_eq!(codex::read_ownership(&fixture.context).unwrap(), initial);
        let pending = fixture.commit(&claim, Decision::Pending);
        assert!(pending.conflicted);
        assert_eq!(pending.pending.len(), 1);
        if restart {
            forget_channel_for_tests(584_503);
        }
        assert_eq!(codex::read_ownership(&fixture.context).unwrap(), pending);
        let mut watch = subscribe_binding_events(584_503).unwrap();
        watch.borrow_and_update();
        let resolved = fixture.commit(&claim, Decision::Verified(source.clone()));
        assert!(watch.has_changed().unwrap());
        watch.borrow_and_update();
        assert!(!resolved.conflicted);
        assert!(resolved.pending.is_empty());
        let proof = resolved.verified.as_ref().unwrap();
        assert_eq!(proof.seq, 3);
        assert_eq!(proof.ownership.pending_seq, Some(2));
        if restart {
            forget_channel_for_tests(584_503);
        }
        assert_eq!(codex::read_ownership(&fixture.context).unwrap(), resolved);
        // A restarted writer has a new watch; subscribe to the active writer before the retry.
        let mut watch = subscribe_binding_events(584_503).unwrap();
        watch.borrow_and_update();
        let bytes = fixture.bytes();
        assert_eq!(fixture.commit(&claim, Decision::Verified(source)), resolved);
        assert_eq!(fixture.bytes(), bytes);
        assert!(!watch.has_changed().unwrap());
        if restart {
            forget_channel_for_tests(584_503);
        }
        assert_eq!(codex::read_ownership(&fixture.context).unwrap(), resolved);
    }
}

#[test]
fn same_claim_retry_cannot_resolve_replaced_descriptors_or_other_parents() {
    for replace in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let claim = fixture.claim(&source);
        fixture.commit(&claim, Decision::Verified(source.clone()));
        fixture.commit(&claim, Decision::Pending);
        forget_channel_for_tests(584_503);
        let candidate = if replace {
            fs::rename(&source.path, source.path.with_extension("retired")).unwrap();
            fixture.source(PARENT)
        } else {
            let other = fixture.source(OTHER);
            fixture.commit(&fixture.claim(&other), Decision::Pending);
            source.clone()
        };
        if replace {
            assert_ne!((candidate.dev, candidate.ino), (source.dev, source.ino));
        }
        let bytes = fixture.bytes();
        let held = fixture.commit(&claim, Decision::Verified(candidate));
        assert!(held.conflicted);
        assert_eq!(held.verified.as_ref().unwrap().source, source);
        assert!(!held.pending.is_empty());
        assert_eq!(fixture.bytes(), bytes);
        forget_channel_for_tests(584_503);
        assert_eq!(codex::read_ownership(&fixture.context).unwrap(), held);
    }
}

#[test]
fn canonical_witness_must_reference_the_exact_observation_and_proof() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let alias = source.path.with_extension("alias");
    std::os::unix::fs::symlink(&source.path, &alias).unwrap();
    let mut observed = source.clone();
    observed.path = alias;
    fixture.generic(
        &observed,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    let claim = fixture.claim(&source);
    fixture.commit(&claim, Decision::Verified(source.clone()));
    let original = fixture.bytes();
    let records: Vec<serde_json::Value> = original
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    for mutation in 0..11 {
        let mut damaged = records.clone();
        let witness = &mut damaged[1]["codex_ownership"]["canonical_witnesses"][0];
        match mutation {
            0 => witness["observation_seq"] = 99.into(),
            1 => witness["observation_seq"] = 2.into(),
            2 => witness["observation_seq"] = 0.into(),
            3 => witness["observed"]["ino"] = (observed.ino + 1).into(),
            4 => witness["source"]["path"] = "/other-proof.jsonl".into(),
            5 => witness["observed"]["session_id"] = OTHER.into(),
            6 => damaged[0]["execution_nonce"] = "b".repeat(32).into(),
            7 => damaged[0]["tmux_session"] = "other-tmux".into(),
            8 => {
                let duplicate = witness.clone();
                damaged[1]["codex_ownership"]["canonical_witnesses"]
                    .as_array_mut()
                    .unwrap()
                    .push(duplicate);
            }
            9 => {
                // Even mutually consistent witness identities must match the recorded endpoints.
                witness["observed"]["ino"] = (observed.ino + 1).into();
                witness["source"]["ino"] = (source.ino + 1).into();
            }
            _ => damaged[1]["codex_ownership"]["canonical_witnesses"] = serde_json::json!([]),
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
            "witness mutation {mutation}"
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
        assert_eq!(fixture.bytes(), bytes);
    }
    fs::write(fixture.path(), original).unwrap();
    forget_channel_for_tests(584_503);
    assert!(
        codex::read_ownership(&fixture.context)
            .unwrap()
            .verified
            .is_some()
    );
}

#[test]
fn matching_inode_without_canonical_equivalence_cannot_create_a_witness() {
    let fixture = Fixture::new();
    let source = fixture.source(PARENT);
    let mut observed = source.clone();
    observed.path = source.path.with_extension("hardlink");
    fs::hard_link(&source.path, &observed.path).unwrap();
    assert_eq!(
        file_identity(&fs::metadata(&observed.path).unwrap()),
        (source.dev, source.ino)
    );
    fixture.generic(
        &observed,
        &fixture.context.execution_nonce,
        None,
        BindingCause::Unknown,
    );
    let held = fixture.commit(&fixture.claim(&source), Decision::Verified(source));
    assert!(held.verified.is_none());
    assert_eq!(held.pending.len(), 1);
    forget_channel_for_tests(584_503);
    assert_eq!(codex::read_ownership(&fixture.context).unwrap(), held);
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

#[test]
fn optional_path_pending_reuses_the_exact_durable_source_proof() {
    for initial_path in [false, true] {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let mut initial = fixture.claim(&source);
        if !initial_path {
            initial.path = None;
        }
        let first = fixture.commit(&initial, Decision::Verified(source.clone()));
        let mut retry = fixture.claim(&source);
        if initial_path {
            retry.path = None;
        }
        let pending = fixture.commit(&retry, Decision::Pending);
        assert_eq!(pending.pending[0].seq, 2);
        forget_channel_for_tests(584_503);
        let resolved = fixture.commit(&retry, Decision::Verified(source.clone()));
        let proof = resolved.verified.as_ref().unwrap();
        assert_eq!(proof.seq, 3);
        assert_eq!(proof.ownership.pending_seq, Some(2));
        assert_eq!(proof.ownership.reused_proof_seq, Some(1));
        assert_eq!(proof.ownership.claim, retry);
        assert_eq!(proof.source, source);
        assert!(resolved.pending.is_empty());
        assert!(!resolved.conflicted);
        assert_eq!(
            codex::proof_at_seq(&fixture.context, 1).unwrap(),
            first.verified
        );
        assert!(codex::proof_at_seq(&fixture.context, 2).unwrap().is_none());
        assert!(codex::proof_at_seq(&fixture.context, 99).unwrap().is_none());
        let mut foreign = fixture.context.clone();
        foreign.execution_nonce = "b".repeat(32);
        assert!(codex::proof_at_seq(&foreign, 1).unwrap().is_none());
        forget_channel_for_tests(584_503);
        assert_eq!(codex::read_ownership(&fixture.context).unwrap(), resolved);
        let bytes = fixture.bytes();
        fixture.commit(&retry, Decision::Verified(source));
        assert_eq!(fixture.bytes(), bytes);
    }
}

#[test]
fn reused_proof_witness_rejects_missing_pending_and_wrong_history() {
    for damage in 0..9 {
        let fixture = Fixture::new();
        let source = fixture.source(PARENT);
        let mut initial = fixture.claim(&source);
        initial.path = None;
        fixture.commit(&initial, Decision::Verified(source.clone()));
        let retry = fixture.claim(&source);
        fixture.commit(&retry, Decision::Pending);
        fixture.commit(&retry, Decision::Verified(source));
        let mut records: Vec<serde_json::Value> = fixture
            .bytes()
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        match damage {
            0 => {
                records[2]["codex_ownership"]
                    .as_object_mut()
                    .unwrap()
                    .remove("reused_proof_seq");
            }
            1 => records[2]["codex_ownership"]["reused_proof_seq"] = 0.into(),
            2 => records[2]["codex_ownership"]["reused_proof_seq"] = 2.into(),
            3 => records[2]["codex_ownership"]["reused_proof_seq"] = 3.into(),
            4 => records[2]["codex_ownership"]["reused_proof_seq"] = 99.into(),
            5 => records[2]["codex_ownership"]["pending_seq"] = 1.into(),
            6 => records[2]["old"]["ino"] = 7.into(),
            7 => {
                records[2]["codex_ownership"]["claim"]["path"] =
                    serde_json::json!(fixture.root.path().join("sessions/foreign.jsonl"))
            }
            _ => {
                records[0]["execution_nonce"] = "b".repeat(32).into();
                records[0]["codex_ownership"]["context"]["execution_nonce"] = "b".repeat(32).into();
            }
        }
        let bytes: Vec<u8> = records
            .into_iter()
            .flat_map(|record| {
                let mut bytes = serde_json::to_vec(&record).unwrap();
                bytes.push(b'\n');
                bytes
            })
            .collect();
        fs::write(fixture.path(), &bytes).unwrap();
        forget_channel_for_tests(584_503);
        assert!(
            codex::read_ownership(&fixture.context).is_err(),
            "damage {damage}"
        );
        assert!(
            codex::proof_at_seq(&fixture.context, 1).is_err(),
            "damage {damage}"
        );
        assert_eq!(fixture.bytes(), bytes);
    }
}
