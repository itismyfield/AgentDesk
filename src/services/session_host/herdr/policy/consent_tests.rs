use super::*;

fn question(id: &str) -> Question {
    let mut witness = ServerWitness::for_test(7);
    witness.socket = "/fixture/socket".into();
    Question {
        key: ConsentKey {
            node: "one".into(),
            user: 501,
            socket: "/fixture/socket".into(),
            named_session: "agentdesk".into(),
            config: "/home/.config/herdr/config.toml".into(),
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
        message: 4,
        intent: IntentRef {
            store: "queue".into(),
            id: id.into(),
            provider: "claude".into(),
            channel: 3,
            source_message: 9,
            fifo: if id == "first" { 10 } else { 11 },
        },
    }
}

#[test]
fn consent_one_card_preserves_original_fifo_and_custody_after_denial_expiry_and_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("consents");
    let store = ConsentStore::new(Some(root.clone()));
    let first = store
        .hold(question("first"), 100)
        .map(QuestionResult::into_consent)
        .unwrap();
    let second = store
        .hold(question("second"), 101)
        .map(QuestionResult::into_consent)
        .unwrap();
    assert_eq!(
        first.nonce, second.nonce,
        "one_pending_card_per_key_contract"
    );
    assert_eq!(second.expires, 1900);
    let denied = store.decide(&second, 1, false, 102).unwrap();
    let preserved = store
        .hold(question("second"), 103)
        .map(QuestionResult::into_consent)
        .unwrap();
    assert_eq!(
        preserved.state,
        State::Denied,
        "denied_input_does_not_requestion_contract"
    );
    assert_eq!(preserved.intents, second.intents);
    drop(store);
    let store = ConsentStore::new(Some(root));
    let loaded = store.load(&first.key).unwrap().unwrap();
    assert_eq!(loaded, denied, "consent_custody_survives_restart_contract");
    let retried = store.retry(question("first"), 2000).unwrap();
    assert_ne!(retried.nonce, first.nonce);
    assert_eq!(retried.preview.revision, 2);
    let expired = store.expire(&retried, 3800).unwrap();
    assert_eq!(expired.state, State::Expired);
    assert_eq!(
        expired
            .intents
            .iter()
            .map(|i| (&i.id, i.fifo))
            .collect::<Vec<_>>(),
        [(&"first".to_owned(), 10), (&"second".to_owned(), 11)],
        "consent_expiry_retains_fifo_custody_contract"
    );
    assert!(store.decide(&expired, 1, true, 3801).is_err());
    let mut automatic = question("second");
    automatic.preview.revision = 2;
    assert_eq!(
        store
            .hold(automatic, 3802)
            .map(QuestionResult::into_consent)
            .unwrap()
            .state,
        State::Expired,
        "expiry_does_not_requestion_contract"
    );
}

#[test]
fn consent_owner_revision_nonce_endpoint_and_preview_cas_accept_only_first_decision() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let current = store
        .hold(question("first"), 10)
        .map(QuestionResult::into_consent)
        .unwrap();
    assert!(
        store.decide(&current, 2, true, 11).is_err(),
        "consent_owner_only_contract"
    );
    for edit in [0, 1, 2, 3, 4, 5] {
        let mut stale = current.clone();
        match edit {
            0 => stale.nonce.push('x'),
            1 => stale.preview.revision += 1,
            2 => stale.preview.witness.pid += 1,
            3 => stale.preview.chain_hash[0] += 1,
            4 => stale.message += 1,
            _ => stale.bot += 1,
        }
        assert!(
            store.decide(&stale, 1, true, 11).is_err(),
            "consent_stale_decision_contract: {edit}"
        );
    }
    let accepted = store.decide(&current, 1, true, 12).unwrap();
    assert_eq!(accepted.state, State::Accepted);
    assert!(
        store.decide(&current, 1, false, 13).is_err(),
        "consent_first_decision_wins_contract"
    );
    let result = ApplyResult {
        before: Some([1; 32]),
        after: Some([4; 32]),
        backup: Some("/backup/config".into()),
        detail: None,
    };
    assert_eq!(
        store.record_apply(&accepted, result.clone()).unwrap().apply,
        Some(result)
    );
    let encoded = std::fs::read_dir(temp.path().join("consents"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let bytes = std::fs::read(encoded.path()).unwrap();
    assert!(
        !String::from_utf8(bytes).unwrap().contains("prompt"),
        "consent_metadata_contains_no_prompt_copy_contract"
    );
}

#[test]
fn consent_key_separates_every_scope_dimension_but_not_witness_or_channel() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let first = store
        .hold(question("first"), 1)
        .map(QuestionResult::into_consent)
        .unwrap();
    for dimension in 0..6 {
        let mut next = question("first");
        match dimension {
            0 => next.key.node.push('x'),
            1 => next.key.user += 1,
            2 => next.key.socket = "/other/socket".into(),
            3 => next.key.named_session.push('x'),
            4 => next.key.config = "/other/config".into(),
            _ => next.key.target = "/other/target".into(),
        }
        let distinct = store.hold(next, 1).map(QuestionResult::into_consent);
        assert!(
            matches!(distinct, Ok(ref value) if value.nonce != first.nonce),
            "consent_full_dedup_key_contract: {dimension}"
        );
    }
    let mut channel = question("second");
    channel.channel = 100;
    channel.message = 101;
    channel.intent.channel = 100;
    channel.preview.witness.pid += 1;
    assert_eq!(
        store
            .hold(channel, 2)
            .map(QuestionResult::into_consent)
            .unwrap()
            .nonce,
        first.nonce,
        "consent_witness_and_channel_are_evidence_not_dedup_key_contract"
    );
}

#[test]
fn consent_independent_store_instances_serialize_the_first_owner_decision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("consents");
    let current = ConsentStore::new(Some(root.clone()))
        .hold(question("first"), 1)
        .map(QuestionResult::into_consent)
        .unwrap();
    let tasks: Vec<_> = [true, false]
        .into_iter()
        .map(|yes| {
            let (root, expected) = (root.clone(), current.clone());
            std::thread::spawn(move || ConsentStore::new(Some(root)).decide(&expected, 1, yes, 2))
        })
        .collect();
    assert_eq!(
        tasks
            .into_iter()
            .map(|task| task.join().expect("consent CAS worker"))
            .filter(Result::is_ok)
            .count(),
        1,
        "consent_concurrent_first_decision_contract"
    );
}

#[test]
fn consent_key_normalization_keeps_one_card_without_collapsing_scope() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let first = store
        .hold(question("first"), 1)
        .map(QuestionResult::into_consent)
        .unwrap();
    let mut alias = question("second");
    alias.key.node = " one ".into();
    alias.key.named_session = " agentdesk ".into();
    alias.key.socket = "/fixture//./socket".into();
    alias.key.config = "/home/.config/./herdr/config.toml".into();
    alias.key.target = "/actual/./config.toml".into();
    let same = store
        .hold(alias, 2)
        .map(QuestionResult::into_consent)
        .unwrap();
    assert_eq!(same.nonce, first.nonce, "consent_normalized_key_contract");
    assert_eq!(same.key, first.key);
    let mut parent_path = question("parent");
    parent_path.key.config = "/home/link/../config.toml".into();
    assert!(
        store
            .hold(parent_path, 3)
            .map(QuestionResult::into_consent)
            .is_err(),
        "consent_parent_directory_components_are_refused_contract"
    );
    let mut unsafe_path = question("bad");
    unsafe_path.key.socket = "relative.sock".into();
    assert!(
        store
            .hold(unsafe_path, 3)
            .map(QuestionResult::into_consent)
            .is_err(),
        "consent_relative_key_is_refused_contract"
    );
}

#[test]
fn consent_owner_decision_expires_at_thirty_minutes_without_expire_call() {
    for elapsed in [TTL - 1, TTL, TTL + 1] {
        let temp = tempfile::tempdir().unwrap();
        let store = ConsentStore::new(Some(temp.path().join("consents")));
        let current = store
            .hold(question("first"), 100)
            .map(QuestionResult::into_consent)
            .unwrap();
        assert_eq!(current.expires, 100 + TTL);
        let decided = store.decide(&current, 1, true, 100 + elapsed);
        assert_eq!(
            decided.is_ok(),
            elapsed < TTL,
            "consent_thirty_minute_owner_decision_contract: elapsed={elapsed}"
        );
        if elapsed >= TTL {
            assert_eq!(
                store.load(&current.key).unwrap().unwrap().intents,
                current.intents,
                "consent_expired_decision_keeps_custody_contract"
            );
        }
    }
}

#[test]
fn consent_explicit_retry_replaces_revision_nonce_and_rejects_the_old_card() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let first = store
        .hold(question("first"), 1)
        .map(QuestionResult::into_consent)
        .unwrap();
    let denied = store.decide(&first, 1, false, 2).unwrap();
    let mut changed = question("second");
    changed.preview.revision = 2;
    changed.preview.target_hash[0] += 1;
    let changed = store.hold(changed, 3).unwrap();
    assert!(
        matches!(&changed, QuestionResult::RevisionChanged(value)
            if value.state == State::Denied && value.decision == denied.decision
                && value.preview == denied.preview && value.intents.len() == 2),
        "consent_changed_policy_preserves_denial_and_custody_contract"
    );
    let retried = store.retry(question("second"), 4).unwrap();
    assert_ne!(retried.nonce, denied.nonce);
    assert!(retried.preview.revision > denied.preview.revision);
    assert!(
        store.decide(&denied, 1, true, 5).is_err(),
        "consent_old_card_cannot_decide_retry_contract"
    );
    assert_eq!(
        retried
            .intents
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"],
        "consent_retry_retains_original_custody_contract"
    );
    let mut forged = question("first");
    forged.intent.fifo += 1;
    forged.preview.revision = retried.preview.revision;
    assert!(
        store
            .hold(forged, 5)
            .map(QuestionResult::into_consent)
            .is_err(),
        "consent_custody_identity_cannot_be_replaced_contract"
    );
}

#[test]
fn consent_absent_corrupt_newer_and_permissions_have_distinct_results() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("consents");
    let store = ConsentStore::new(Some(root.clone()));
    let key = question("first").key;
    assert_eq!(store.load(&key).unwrap(), None);
    assert!(
        !root.exists(),
        "consent_constructor_and_absent_load_are_lazy_contract"
    );
    let current = store
        .hold(question("first"), 1)
        .map(QuestionResult::into_consent)
        .unwrap();
    let path = root.join(format!("{}.json", durable::key(&current.key).unwrap()));
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{").unwrap();
    let corrupt = store.load(&key).unwrap_err();
    assert!(
        corrupt.contains("consent_snapshot_corrupt")
            && corrupt.contains(&path.display().to_string()),
        "consent_corruption_is_not_absence_contract: {corrupt}"
    );
    let mut newer: serde_json::Value = serde_json::from_slice(&original).unwrap();
    newer["schema"] = 2.into();
    newer["future_field"] = true.into();
    std::fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
    let future = store.load(&key).unwrap_err();
    assert!(
        future.contains("consent_snapshot_newer") && !future.contains("corrupt"),
        "consent_newer_schema_is_not_corruption_contract: {future}"
    );
    std::fs::write(&path, &original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load(&key).unwrap_err().contains("0600"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        store.load(&key).unwrap().unwrap(),
        current,
        "consent_original_schema_round_trip_contract"
    );
}

#[test]
fn consent_apply_result_from_older_accepted_card_keeps_later_custody_and_fifo() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temporary.path().join("consents")));
    let pending = store
        .hold(question("first"), 1)
        .map(QuestionResult::into_consent)
        .unwrap();
    let accepted = store.decide(&pending, 1, true, 2).unwrap();
    let appended = store
        .hold(question("second"), 3)
        .map(QuestionResult::into_consent)
        .unwrap();
    assert_eq!(appended.state, State::Accepted);
    assert_eq!(appended.nonce, accepted.nonce);
    assert_eq!(accepted.intents.len(), 1);
    assert_eq!(appended.intents.len(), 2);
    let result = ApplyResult {
        before: Some([1; 32]),
        after: Some([4; 32]),
        backup: Some("/fixture/backup".into()),
        detail: None,
    };
    let applied = store.record_apply(&accepted, result).unwrap();
    assert_eq!(applied.state, State::Applied);
    assert_eq!(
        applied.intents, appended.intents,
        "consent_apply_cas_preserves_later_appended_custody_contract"
    );
    assert_eq!(
        applied
            .intents
            .iter()
            .map(|intent| (intent.id.as_str(), intent.fifo))
            .collect::<Vec<_>>(),
        [("first", 10), ("second", 11)],
        "consent_apply_cas_preserves_later_appended_fifo_contract"
    );
    assert_eq!(store.load(&applied.key).unwrap().unwrap(), applied);
}

#[test]
fn consent_changed_revision_preserves_applied_decision_and_appends_custody_before_returning() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("consents");
    let store = ConsentStore::new(Some(root.clone()));
    let first = store.hold(question("first"), 1).unwrap().into_consent();
    let accepted = store.decide(&first, 1, true, 2).unwrap();
    let applied = store
        .record_apply(
            &accepted,
            ApplyResult {
                before: Some([1; 32]),
                after: Some([4; 32]),
                backup: Some("/fixture/backup".into()),
                detail: None,
            },
        )
        .unwrap();
    let mut next = question("second");
    next.preview.revision += 1;
    next.preview.target_hash = [5; 32];
    let result = store.hold(next, 4);
    assert!(
        matches!(&result, Ok(QuestionResult::RevisionChanged(value))
        if value.nonce == applied.nonce && value.preview == applied.preview
            && value.state == State::Applied && value.decision == applied.decision
            && value.apply == applied.apply
            && value.intents.iter().map(|intent| (intent.id.as_str(),intent.fifo))
               .collect::<Vec<_>>() == [("first",10),("second",11)]),
        "revision_changed_preserves_applied_decision_and_fifo_contract: {result:?}"
    );
    let value = result.unwrap().into_consent();
    assert_eq!(
        ConsentStore::new(Some(root)).load(&value.key).unwrap(),
        Some(value),
        "revision_changed_custody_is_durable_before_return_contract"
    );
}

#[test]
fn consent_original_schema_without_card_phase_remains_bound() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let value = store.hold(question("first"), 1).unwrap().into_consent();
    let mut legacy = serde_json::to_value(&value).unwrap();
    legacy.as_object_mut().unwrap().remove("card");
    let decoded = store
        .decode(&serde_json::to_vec(&legacy).unwrap(), &value.key)
        .unwrap();
    assert_eq!(
        decoded, value,
        "old_consent_card_identity_remains_bound_contract"
    );
}
