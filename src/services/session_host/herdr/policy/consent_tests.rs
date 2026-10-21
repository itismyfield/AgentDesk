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
    let first = store.hold(question("first"), 100).unwrap();
    let second = store.hold(question("second"), 101).unwrap();
    assert_eq!(
        first.nonce, second.nonce,
        "one_pending_card_per_key_contract"
    );
    assert_eq!(second.expires, 1900);
    let denied = store.decide(&second, 1, false, 102).unwrap();
    let preserved = store.hold(question("second"), 103).unwrap();
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
        store.hold(automatic, 3802).unwrap().state,
        State::Expired,
        "expiry_does_not_requestion_contract"
    );
}

#[test]
fn consent_owner_revision_nonce_endpoint_and_preview_cas_accept_only_first_decision() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().into()));
    let current = store.hold(question("first"), 10).unwrap();
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
    let encoded = std::fs::read_dir(temp.path())
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
    let store = ConsentStore::new(Some(temp.path().into()));
    let first = store.hold(question("first"), 1).unwrap();
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
        assert_ne!(
            store.hold(next, 1).unwrap().nonce,
            first.nonce,
            "consent_full_dedup_key_contract: {dimension}"
        );
    }
    let mut channel = question("second");
    channel.channel = 100;
    channel.message = 101;
    channel.intent.channel = 100;
    channel.preview.witness.pid += 1;
    assert_eq!(
        store.hold(channel, 2).unwrap().nonce,
        first.nonce,
        "consent_witness_and_channel_are_evidence_not_dedup_key_contract"
    );
}

#[test]
fn consent_independent_store_instances_serialize_the_first_owner_decision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let current = ConsentStore::new(Some(root.clone()))
        .hold(question("first"), 1)
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
