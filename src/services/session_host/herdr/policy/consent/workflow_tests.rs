use super::super::TTL;
use super::*;
use crate::services::session_host::herdr::contract::ServerWitness;

fn question(id: &str) -> Question {
    let mut witness = ServerWitness::for_test(7);
    witness.socket = "/fixture/socket".into();
    Question {
        key: ConsentKey {
            node: "one".into(),
            user: 501,
            socket: "/fixture/socket".into(),
            named_session: "agentdesk".into(),
            config: "/home/config.toml".into(),
            target: "/actual/config.toml".into(),
        },
        preview: Preview {
            revision: 1,
            witness,
            target_hash: [1; 32],
            chain_hash: [2; 32],
            checker_hash: [3; 32],
        },
        owner: 1,
        bot: 2,
        provider: "claude".into(),
        channel: 3,
        message: 0,
        intent: super::super::IntentRef {
            store: "queue".into(),
            id: id.into(),
            provider: "claude".into(),
            channel: 3,
            source_message: if id == "first" { 9 } else { 10 },
            fifo: if id == "first" { 10 } else { 11 },
        },
    }
}

fn current(consent: &Consent) -> Current {
    Current {
        owner: Some(consent.owner),
        bot: consent.bot,
        provider: consent.provider.clone(),
        channel: consent.channel,
        message: consent.message,
        nonce: consent.nonce.clone(),
        key: consent.key.clone(),
        preview: consent.preview.clone(),
        local_node: consent.key.node.clone(),
    }
}

fn bound(store: &ConsentStore) -> Consent {
    let reserved = Workflow(store)
        .hold(question("first"), 100, Request::ExplicitLaunch)
        .unwrap();
    assert_eq!(reserved.effect, Effect::SendCard);
    Workflow(store)
        .bind(&reserved.consent, &current(&reserved.consent), 40)
        .unwrap()
        .consent
}

fn result(success: bool) -> ApplyResult {
    ApplyResult {
        before: Some([1; 32]),
        after: success.then_some([4; 32]),
        backup: Some("/backup/config".into()),
        detail: None,
    }
}

#[test]
fn workflow_reserves_one_card_and_restart_never_resends_an_unbound_card() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let automatic = Workflow(&store)
        .hold(question("first"), 100, Request::Automatic)
        .unwrap();
    assert_eq!(
        automatic.effect,
        Effect::KeepHeld,
        "workflow_automatic_never_questions_contract"
    );
    assert_eq!(automatic.consent.card, CardState::Awaiting);
    let reserved = Workflow(&store)
        .hold(question("second"), 101, Request::ExplicitLaunch)
        .unwrap();
    assert_eq!(
        reserved.effect,
        Effect::SendCard,
        "workflow_explicit_reserves_card_contract"
    );
    assert_eq!(reserved.consent.card, CardState::Reserved);
    for request in [Request::Automatic, Request::ExplicitLaunch] {
        let duplicate = Workflow(&store)
            .hold(question("first"), 102, request)
            .unwrap();
        assert_eq!(
            duplicate.effect,
            Effect::KeepHeld,
            "workflow_reserved_card_is_not_resent_contract"
        );
    }
    let reopened = ConsentStore::new(Some(root.path().join("consents")));
    let recovered = Workflow(&reopened)
        .recover(&reserved.consent.key)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.effect,
        Effect::KeepHeld,
        "workflow_restart_reserved_card_stays_held_contract"
    );
    let bound = Workflow(&reopened)
        .bind(&reserved.consent, &current(&reserved.consent), 40)
        .unwrap();
    assert_eq!(bound.effect, Effect::EnableButtons);
    assert_eq!(bound.consent.card, CardState::Bound);
    assert!(
        Workflow(&reopened)
            .bind(&reserved.consent, &current(&reserved.consent), 41)
            .is_err(),
        "workflow_card_binding_is_single_cas_contract"
    );
    assert_eq!(
        bound
            .consent
            .intents
            .iter()
            .map(|i| (&*i.id, i.fifo))
            .collect::<Vec<_>>(),
        [("first", 10), ("second", 11)]
    );
}

#[test]
fn workflow_owner_and_current_binding_are_required_before_apply_or_release() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let bound = bound(&store);
    assert!(
        Workflow(&store)
            .decide(&bound, &current(&bound), 2, true, 110)
            .is_err(),
        "workflow_owner_only_contract"
    );
    for field in 0..19 {
        let mut changed = current(&bound);
        match field {
            0 => changed.owner = None,
            1 => changed.owner = Some(2),
            2 => changed.bot += 1,
            3 => changed.provider = "codex".into(),
            4 => changed.channel += 1,
            5 => changed.message += 1,
            6 => changed.nonce.push('x'),
            7 => changed.key.node.push('x'),
            8 => changed.key.user += 1,
            9 => changed.key.socket.push("other"),
            10 => changed.key.named_session.push('x'),
            11 => changed.key.config.push("other"),
            12 => changed.key.target.push("other"),
            13 => changed.preview.revision += 1,
            14 => changed.preview.witness.pid += 1,
            15 => changed.preview.target_hash[0] ^= 1,
            16 => changed.preview.chain_hash[0] ^= 1,
            17 => changed.preview.checker_hash[0] ^= 1,
            _ => changed.local_node.push('x'),
        }
        assert!(
            Workflow(&store)
                .decide(&bound, &changed, 1, true, 110)
                .is_err(),
            "workflow_current_identity_and_preview_contract: {field}"
        );
        assert_eq!(
            store.load(&bound.key).unwrap().unwrap().state,
            State::Pending
        );
    }
}

#[test]
fn workflow_first_decision_is_the_only_apply_and_receipt_precedes_drain() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let bound = bound(&store);
    let accepted = Workflow(&store)
        .decide(&bound, &current(&bound), 1, true, 110)
        .unwrap();
    assert_eq!(
        accepted.effect,
        Effect::Apply,
        "workflow_first_yes_applies_contract"
    );
    assert_eq!(
        store.load(&bound.key).unwrap().unwrap().state,
        State::Accepted
    );
    for yes in [true, false] {
        let duplicate = Workflow(&store)
            .decide(&bound, &current(&bound), 1, yes, 111)
            .unwrap();
        assert_eq!(
            duplicate.effect,
            Effect::ShowResult,
            "workflow_duplicate_decision_has_no_effect_contract"
        );
        assert_eq!(duplicate.consent.state, State::Accepted);
    }
    let committed = Workflow(&store)
        .receipt(&accepted.consent, result(true))
        .unwrap();
    assert_eq!(
        store.load(&bound.key).unwrap().unwrap().apply,
        Some(result(true)),
        "workflow_receipt_durable_before_wake_contract"
    );
    assert_eq!(committed.consent.state, State::Applied);
    assert_eq!(
        committed.effect,
        Effect::ReleaseThenWake,
        "workflow_successful_receipt_releases_then_wakes_contract"
    );
    assert!(
        Workflow(&store)
            .receipt(&accepted.consent, result(true))
            .is_err(),
        "workflow_receipt_is_not_repeated_contract"
    );
    let mut after_apply = current(&committed.consent);
    after_apply.preview.target_hash = [4; 32];
    let duplicate = Workflow(&store)
        .decide(&bound, &after_apply, 1, true, 112)
        .unwrap();
    assert_eq!(
        duplicate.effect,
        Effect::ShowResult,
        "workflow_applied_duplicate_only_shows_result_contract"
    );
}

#[test]
fn workflow_failed_receipt_keeps_custody_and_cannot_emit_a_drain_effect() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let bound = bound(&store);
    let accepted = Workflow(&store)
        .decide(&bound, &current(&bound), 1, true, 110)
        .unwrap();
    let key = crate::services::session_host::herdr::policy::durable::key(&bound.key).unwrap();
    std::fs::write(
        directory.join(format!("{key}.pending")),
        b"interrupted receipt",
    )
    .unwrap();
    assert!(
        Workflow(&store)
            .receipt(&accepted.consent, result(true))
            .is_err(),
        "workflow_receipt_failure_never_wakes_drain_contract"
    );
    let previous: Consent =
        serde_json::from_slice(&std::fs::read(directory.join(format!("{key}.json"))).unwrap())
            .unwrap();
    assert_eq!(previous.state, State::Accepted);
    assert_eq!(previous.intents, accepted.consent.intents);
    let reopened = ConsentStore::new(Some(directory));
    assert!(
        Workflow(&reopened).recover(&bound.key).is_err(),
        "workflow_interrupted_receipt_is_fail_closed_contract"
    );
}

#[test]
fn workflow_denial_expiry_and_failed_apply_preserve_fifo_without_requestion() {
    for phase in ["denied", "expired", "apply_failed", "accepted", "applied"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("consents");
        let store = ConsentStore::new(Some(directory.clone()));
        let bound = if phase == "pending" {
            Workflow(&store)
                .hold(question("first"), 100, Request::Automatic)
                .unwrap()
                .consent
        } else if phase == "reserved" {
            Workflow(&store)
                .hold(question("first"), 100, Request::ExplicitLaunch)
                .unwrap()
                .consent
        } else {
            bound(&store)
        };
        let terminal = match phase {
            "denied" => Workflow(&store)
                .decide(&bound, &current(&bound), 1, false, 110)
                .unwrap(),
            "expired" => Workflow(&store)
                .decide(&bound, &current(&bound), 1, true, bound.expires)
                .unwrap(),
            _ => {
                let accepted = Workflow(&store)
                    .decide(&bound, &current(&bound), 1, true, 110)
                    .unwrap();
                if phase == "accepted" {
                    accepted
                } else {
                    let receipt = Workflow(&store)
                        .receipt(&accepted.consent, result(phase == "applied"))
                        .unwrap();
                    if phase == "apply_failed" {
                        assert_eq!(
                            receipt.effect,
                            Effect::KeepHeld,
                            "workflow_failed_apply_keeps_hold_contract"
                        );
                    }
                    receipt
                }
            }
        };
        if phase == "expired" {
            assert_eq!(
                (&terminal.consent.state, terminal.effect),
                (&State::Expired, Effect::KeepHeld),
                "workflow_expiry_prevents_apply_contract"
            );
        }
        let held = Workflow(&store)
            .hold(question("second"), 2000, Request::Automatic)
            .unwrap();
        assert_eq!(
            held.effect,
            Effect::KeepHeld,
            "workflow_terminal_input_never_requestions_contract: {phase}"
        );
        let reopened = ConsentStore::new(Some(directory));
        let recovered = Workflow(&reopened).recover(&bound.key).unwrap().unwrap();
        assert_eq!(
            recovered.effect,
            if phase == "applied" {
                Effect::ReleaseThenWake
            } else {
                Effect::KeepHeld
            },
            "workflow_restart_only_applied_releases_contract: {phase}"
        );
        assert_eq!(
            recovered.consent.state,
            if phase == "accepted" {
                State::ApplyFailed
            } else {
                terminal.consent.state.clone()
            }
        );
        assert_eq!(
            recovered
                .consent
                .intents
                .iter()
                .map(|i| (&*i.id, i.fifo))
                .collect::<Vec<_>>(),
            [("first", 10), ("second", 11)],
            "workflow_restart_custody_fifo_contract: {phase}"
        );
    }
}

#[test]
fn workflow_racing_owner_decisions_emit_one_apply_and_one_stored_result() {
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(ConsentStore::new(Some(root.path().join("consents"))));
    let bound = bound(&store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads = [true, false]
        .into_iter()
        .map(|yes| {
            let store = store.clone();
            let bound = bound.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Workflow(&store).decide(&bound, &current(&bound), 1, yes, 110)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let transitions = threads
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        transitions
            .iter()
            .filter(|t| t.effect == Effect::ShowResult)
            .count(),
        1,
        "workflow_racing_duplicate_only_shows_result_contract"
    );
    let applied = transitions
        .iter()
        .filter(|t| t.effect == Effect::Apply)
        .count();
    let denied = transitions
        .iter()
        .filter(|t| t.effect == Effect::KeepHeld)
        .count();
    assert_eq!(
        applied + denied,
        1,
        "workflow_racing_first_decision_is_unique_contract"
    );
    assert!(
        transitions
            .iter()
            .all(|t| t.consent.intents == bound.intents)
    );
}

#[test]
fn workflow_retry_requires_current_owner_and_node_and_invalidates_old_buttons() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let bound = bound(&store);
    let denied = Workflow(&store)
        .decide(&bound, &current(&bound), 1, false, 110)
        .unwrap();
    for (actor, owner, node) in [
        (2, Some(1), "one"),
        (1, None, "one"),
        (1, Some(2), "one"),
        (1, Some(1), "other"),
    ] {
        assert!(
            Workflow(&store)
                .retry(question("second"), actor, owner, node, 120)
                .is_err(),
            "workflow_retry_owner_and_node_contract"
        );
    }
    let retry = Workflow(&store)
        .retry(question("second"), 1, Some(1), "one", 120)
        .unwrap();
    assert_eq!(retry.effect, Effect::SendCard);
    assert_ne!(retry.consent.nonce, denied.consent.nonce);
    assert!(
        Workflow(&store)
            .decide(&bound, &current(&bound), 1, true, 121)
            .is_err(),
        "workflow_retry_invalidates_old_card_contract"
    );
    assert_eq!(
        retry
            .consent
            .intents
            .iter()
            .map(|i| (&*i.id, i.fifo))
            .collect::<Vec<_>>(),
        [("first", 10), ("second", 11)]
    );
}

#[test]
fn workflow_changed_revision_preserves_new_intent_without_implicitly_retrying() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let bound = bound(&store);
    let accepted = Workflow(&store)
        .decide(&bound, &current(&bound), 1, true, 110)
        .unwrap();
    Workflow(&store)
        .receipt(&accepted.consent, result(true))
        .unwrap();
    let mut revised = question("second");
    revised.preview.revision += 1;
    let held = Workflow(&store)
        .hold(revised, 120, Request::ExplicitLaunch)
        .unwrap();
    assert_eq!(
        held.effect,
        Effect::RevisionChanged,
        "workflow_changed_revision_is_not_owner_retry_contract"
    );
    assert_eq!(held.consent.nonce, bound.nonce);
    assert_eq!(
        held.consent.intents.len(),
        2,
        "workflow_changed_revision_preserves_custody_contract"
    );
}

#[test]
fn workflow_racing_retries_can_bind_only_the_latest_card() {
    let root = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(ConsentStore::new(Some(root.path().join("consents"))));
    let initial = bound(&store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let threads = (0..2)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Workflow(&store).retry(question("second"), 1, Some(1), "one", 120)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let attempts = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    let mut bindings = Vec::new();
    for (index, attempt) in attempts.into_iter().enumerate() {
        if let Ok(reserved) = attempt {
            let bound = Workflow(&store).bind(
                &reserved.consent,
                &current(&reserved.consent),
                50 + index as u64,
            );
            if let Ok(bound) = bound {
                bindings.push(bound);
            }
        }
    }
    assert_eq!(
        bindings.len(),
        1,
        "workflow_retry_race_has_one_live_card_contract"
    );
    let latest = store.load(&initial.key).unwrap().unwrap();
    assert_eq!(latest.nonce, bindings[0].consent.nonce);
    assert!(
        Workflow(&store)
            .decide(&initial, &current(&initial), 1, true, 121)
            .is_err()
    );
}

#[test]
fn workflow_unsent_card_ttl_starts_at_reservation() {
    for elapsed in [TTL - 1, TTL, TTL + 1] {
        let root = tempfile::tempdir().unwrap();
        let store = ConsentStore::new(Some(root.path().join("consents")));
        let automatic = Workflow(&store)
            .hold(question("first"), 100, Request::Automatic)
            .unwrap();
        assert_eq!(automatic.consent.card, CardState::Awaiting);
        let at = 100 + elapsed;
        let reserved = Workflow(&store)
            .hold(question("second"), at, Request::ExplicitLaunch)
            .unwrap();
        assert_eq!(
            reserved.effect,
            Effect::SendCard,
            "workflow_awaiting_card_never_expires_contract: elapsed={elapsed}"
        );
        assert_eq!(
            reserved.consent.expires,
            at + TTL,
            "workflow_reserved_card_gets_full_ttl_contract: elapsed={elapsed}"
        );
        let bound = Workflow(&store)
            .bind(&reserved.consent, &current(&reserved.consent), 40)
            .unwrap()
            .consent;
        assert!(
            store.decide(&bound, 1, true, bound.expires - 1).is_ok(),
            "workflow_reserved_ttl_last_instant_contract"
        );
    }
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let bound = bound(&store);
    assert!(
        store.decide(&bound, 1, true, bound.expires).is_err(),
        "workflow_reserved_ttl_boundary_contract"
    );
}

#[test]
fn workflow_retry_never_discards_an_in_flight_or_completed_apply() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let bound = bound(&store);
    let accepted = Workflow(&store)
        .decide(&bound, &current(&bound), 1, true, 110)
        .unwrap()
        .consent;
    let path = directory.join(format!(
        "{}.json",
        crate::services::session_host::herdr::policy::durable::key(&bound.key).unwrap()
    ));
    let before = std::fs::read(&path).unwrap();
    let retry = Workflow(&store).retry(question("second"), 1, Some(1), "one", 120);
    assert!(
        matches!(retry, Err(ref error) if error.contains("apply in progress")),
        "workflow_retry_preserves_in_flight_apply_contract: {retry:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "workflow_accepted_retry_bytes_unchanged_contract"
    );
    let applied = Workflow(&store)
        .receipt(&accepted, result(true))
        .unwrap()
        .consent;
    let retry = Workflow(&store)
        .retry(question("second"), 1, Some(1), "one", 130)
        .unwrap();
    assert_eq!(
        retry.effect,
        Effect::ShowResult,
        "workflow_applied_retry_already_applied_contract"
    );
    assert_eq!(
        (
            &retry.consent.nonce,
            &retry.consent.preview,
            &retry.consent.decision,
            &retry.consent.apply
        ),
        (
            &applied.nonce,
            &applied.preview,
            &applied.decision,
            &applied.apply
        ),
        "workflow_applied_retry_preserves_history_contract"
    );
    assert_eq!(
        retry.consent.intents.len(),
        2,
        "workflow_applied_retry_records_its_intent_contract"
    );
    let before = std::fs::read(&path).unwrap();
    Workflow(&store)
        .retry(question("second"), 1, Some(1), "one", 131)
        .unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "workflow_applied_duplicate_retry_bytes_unchanged_contract"
    );
}

#[test]
fn workflow_interrupted_accept_has_an_owner_exit() {
    use sha2::{Digest, Sha256};
    for bytes in [
        None,
        Some(b"resume = true".as_slice()),
        Some(b"resume = false".as_slice()),
    ] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("consents");
        let store = ConsentStore::new(Some(directory.clone()));
        let mut request = question("first");
        request.key.config = root.path().join("config.toml");
        if let Some(bytes) = bytes {
            std::fs::write(&request.key.config, bytes).unwrap();
        }
        let reserved = Workflow(&store)
            .hold(request, 100, Request::ExplicitLaunch)
            .unwrap();
        let bound = Workflow(&store)
            .bind(&reserved.consent, &current(&reserved.consent), 40)
            .unwrap()
            .consent;
        let accepted = Workflow(&store)
            .decide(&bound, &current(&bound), 1, true, 110)
            .unwrap()
            .consent;
        drop(store);
        let reopened = ConsentStore::new(Some(directory));
        let recovered = Workflow(&reopened).recover(&accepted.key).unwrap().unwrap();
        assert_eq!(
            recovered.consent.state,
            State::ApplyFailed,
            "workflow_interrupted_accept_has_owner_exit_contract"
        );
        assert_eq!(recovered.effect, Effect::KeepHeld);
        let receipt = recovered.consent.apply.as_ref().unwrap();
        let observed: Option<[u8; 32]> = bytes.map(|bytes| Sha256::digest(bytes).into());
        assert_eq!(
            (receipt.before, receipt.after),
            (observed, observed),
            "workflow_interrupted_receipt_reobserves_logical_config_contract"
        );
        assert_eq!(receipt.backup, None);
        assert_eq!(
            receipt.detail.as_deref(),
            Some("apply interrupted; config state re-observed")
        );
        assert_eq!(
            recovered.consent.intents, accepted.intents,
            "workflow_interruption_preserves_fifo_contract"
        );
        let mut retry_request = question("second");
        retry_request.key = accepted.key.clone();
        let retry = Workflow(&reopened)
            .retry(retry_request, 1, Some(1), "one", 120)
            .unwrap();
        assert_eq!(
            retry.effect,
            Effect::SendCard,
            "workflow_interrupted_accept_retry_sends_card_contract"
        );
    }
}

#[test]
fn workflow_applied_then_new_revision_retry_opens_a_new_generation_preserving_history() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let bound = bound(&store);
    let accepted = Workflow(&store)
        .decide(&bound, &current(&bound), 1, true, 110)
        .unwrap();
    let applied = Workflow(&store)
        .receipt(&accepted.consent, result(true))
        .unwrap()
        .consent;
    let settled = store
        .acknowledge_discard(&applied, &applied.intents[0])
        .unwrap();
    let old_settlements = settled.settlements.clone();
    let old_preview_entry = settled.intent_previews[0].clone();
    let mut changed = question("second");
    changed.preview.revision += 1;
    changed.preview.target_hash = [9; 32];
    let held = Workflow(&store)
        .hold(changed, 120, Request::ExplicitLaunch)
        .unwrap();
    assert_eq!(held.effect, Effect::RevisionChanged);
    let mut retry_request = question("retry");
    retry_request.preview.revision += 1;
    retry_request.preview.target_hash = [9; 32];
    let retry = Workflow(&store)
        .retry(retry_request, 1, Some(1), "one", 130)
        .unwrap();
    assert_eq!(
        retry.effect,
        Effect::SendCard,
        "workflow_applied_new_revision_opens_generation_contract"
    );
    assert_eq!(retry.consent.state, State::Pending);
    assert_ne!(retry.consent.nonce, applied.nonce);
    assert_eq!(
        retry.consent.history.len(),
        1,
        "workflow_applied_generation_preserves_prior_receipt_contract"
    );
    let previous = &retry.consent.history[0];
    assert_eq!(
        (
            &previous.nonce,
            &previous.preview,
            previous.decision,
            &previous.apply
        ),
        (
            &applied.nonce,
            &applied.preview,
            applied.decision,
            &applied.apply
        )
    );
    assert_eq!(
        previous.apply.as_ref().unwrap().backup,
        Some("/backup/config".into())
    );
    assert_eq!(
        retry.consent.settlements, old_settlements,
        "workflow_generation_preserves_settled_refs_contract"
    );
    assert_eq!(
        retry.consent.intent_previews[0], old_preview_entry,
        "workflow_generation_never_rewrites_settled_preview_contract"
    );
    assert_eq!(retry.consent.intents[..2], held.consent.intents);
    assert_eq!(
        retry.consent.intents[2].id, "retry",
        "workflow_generation_records_retry_intent_contract"
    );
    let reopened = ConsentStore::new(Some(directory));
    assert_eq!(
        reopened.load(&retry.consent.key).unwrap().unwrap(),
        retry.consent,
        "workflow_generation_history_survives_restart_contract"
    );
}

#[test]
fn workflow_old_schema_without_history_still_reads_and_retries() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("consents");
    let store = ConsentStore::new(Some(directory.clone()));
    let bound = bound(&store);
    let key = crate::services::session_host::herdr::policy::durable::key(&bound.key).unwrap();
    let path = directory.join(format!("{key}.json"));
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("history");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let loaded = store.load(&bound.key).unwrap().unwrap();
    assert!(
        loaded.history.is_empty(),
        "workflow_old_schema_defaults_history_contract"
    );
    assert_eq!(loaded.intents, bound.intents);
    let retry = Workflow(&store)
        .retry(question("second"), 1, Some(1), "one", 120)
        .unwrap();
    assert_eq!(
        retry.effect,
        Effect::SendCard,
        "workflow_old_schema_has_owner_exit_contract"
    );
}

#[test]
fn workflow_each_unfinished_state_has_a_retry_exit_after_boot_recovery() {
    for phase in [
        "pending",
        "reserved",
        "denied",
        "expired",
        "accepted",
        "apply_failed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = ConsentStore::new(Some(root.path().join("consents")));
        let bound = if phase == "pending" {
            Workflow(&store)
                .hold(question("first"), 100, Request::Automatic)
                .unwrap()
                .consent
        } else if phase == "reserved" {
            Workflow(&store)
                .hold(question("first"), 100, Request::ExplicitLaunch)
                .unwrap()
                .consent
        } else {
            bound(&store)
        };
        let terminal = match phase {
            "denied" => {
                Workflow(&store)
                    .decide(&bound, &current(&bound), 1, false, 110)
                    .unwrap()
                    .consent
            }
            "expired" => store.expire(&bound, bound.expires).unwrap(),
            "accepted" | "apply_failed" => {
                let accepted = Workflow(&store)
                    .decide(&bound, &current(&bound), 1, true, 110)
                    .unwrap()
                    .consent;
                if phase == "apply_failed" {
                    store.record_apply(&accepted, result(false)).unwrap()
                } else {
                    accepted
                }
            }
            _ => bound,
        };
        let recovered = Workflow(&store).recover(&terminal.key).unwrap().unwrap();
        assert_ne!(
            recovered.consent.state,
            State::Accepted,
            "workflow_unfinished_state_recovers_to_owner_exit_contract: {phase}"
        );
        let retry = Workflow(&store)
            .retry(question("second"), 1, Some(1), "one", 120)
            .unwrap();
        assert_eq!(
            retry.effect,
            Effect::SendCard,
            "workflow_unfinished_state_owner_exit_contract: {phase}"
        );
        assert_eq!(retry.consent.intents.len(), 2);
    }
}

#[test]
fn workflow_interruption_cas_never_overwrites_a_receipt_or_new_generation() {
    let root = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(root.path().join("consents")));
    let bound = bound(&store);
    let accepted = Workflow(&store)
        .decide(&bound, &current(&bound), 1, true, 110)
        .unwrap()
        .consent;
    let applied = store.record_apply(&accepted, result(true)).unwrap();
    assert!(
        store.interrupt_accepted(&accepted, result(false)).is_err(),
        "workflow_interruption_never_overwrites_receipt_contract"
    );
    assert_eq!(store.load(&bound.key).unwrap().unwrap(), applied);
    let mut changed = question("second");
    changed.preview.revision += 1;
    let retry = Workflow(&store)
        .retry(changed, 1, Some(1), "one", 120)
        .unwrap()
        .consent;
    assert!(
        store.interrupt_accepted(&accepted, result(false)).is_err(),
        "workflow_interruption_cas_rejects_old_generation_contract"
    );
    assert_eq!(store.load(&bound.key).unwrap().unwrap(), retry);
}
