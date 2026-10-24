use super::*;
use crate::services::session_host::ServerWitness;

fn endpoint(node: &str) -> HerdrLaunchEndpoint {
    HerdrLaunchEndpoint {
        execution_node: node.into(),
        config_key: "default".into(),
        socket_addr: "/fixture/herdr.sock".into(),
        herdr_session: "agentdesk".into(),
    }
}
fn grant(endpoint: &HerdrLaunchEndpoint, nonce: &str, pid: u32) -> LaunchAdmissionGrant {
    let mut witness = ServerWitness::for_test(pid);
    witness.socket = endpoint.socket_addr.clone().into();
    super::super::permit::grant_for_test(endpoint, nonce, witness)
}
fn successful(restored: GrantRestoration) -> Vec<LaunchAdmissionGrant> {
    assert!(!restored.unknown_failure);
    restored
        .by_endpoint
        .into_values()
        .flat_map(Result::unwrap)
        .collect()
}

#[tokio::test]
async fn durable_grant_roundtrip_survives_owner_restart_without_rechecking_current_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("grants");
    let endpoint = endpoint("one");
    let expected = grant(&endpoint, "nonce", 7);
    let first = FileGrantStore::new(Some(root.clone()));
    assert!(first.load(&endpoint, "nonce").await.unwrap().is_none());
    first.save(expected.clone()).await.unwrap();
    drop(first);
    let restored = FileGrantStore::new(Some(root));
    assert_eq!(
        restored.load(&endpoint, "nonce").await.unwrap(),
        Some(expected.clone()),
        "durable_grant_rehydrates_exact_authority_contract"
    );
    let loaded = successful(
        restored
            .load_all(std::slice::from_ref(&endpoint))
            .await
            .unwrap(),
    );
    assert_eq!(loaded.as_slice(), std::slice::from_ref(&expected));
    assert!(
        restored
            .load(&endpoint, "another-nonce")
            .await
            .unwrap()
            .is_none()
    );
    let mut other = endpoint.clone();
    other.herdr_session = "other".into();
    assert!(restored.load(&other, "nonce").await.unwrap().is_none());
    restored.save(expected).await.unwrap();
    assert!(
        restored.save(grant(&endpoint, "nonce", 8)).await.is_err(),
        "durable_grant_is_immutable_contract"
    );
}

#[tokio::test]
async fn durable_grant_missing_corrupt_newer_and_interrupted_are_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileGrantStore::new(Some(temp.path().join("grants")));
    let endpoint = endpoint("one");
    let expected = grant(&endpoint, "nonce", 7);
    store.save(expected.clone()).await.unwrap();
    let key = durable::key(&(&endpoint, "nonce")).unwrap();
    let path = store.0.0.as_ref().unwrap().join(format!("{key}.json"));
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"corrupt").unwrap();
    assert!(
        store.load(&endpoint, "nonce").await.is_err(),
        "corrupt_grant_is_not_missing_contract"
    );
    let mut newer: serde_json::Value = serde_json::from_slice(&original).unwrap();
    newer["schema"] = 2.into();
    std::fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
    assert!(
        store.load(&endpoint, "nonce").await.is_err(),
        "newer_grant_schema_refuses_contract"
    );
    std::fs::write(&path, original).unwrap();
    std::fs::write(path.with_extension("pending"), b"pending").unwrap();
    assert!(
        store.load(&endpoint, "nonce").await.is_err(),
        "interrupted_grant_is_not_authority_contract"
    );
    assert!(store.load_all(&[endpoint]).await.is_err());
}

#[tokio::test]
async fn durable_grant_boot_with_no_user_endpoints_touches_no_files_and_node_keys_do_not_collide() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("grants");
    let store = FileGrantStore::new(Some(root.clone()));
    assert!(successful(store.load_all(&[]).await.unwrap()).is_empty());
    assert!(!root.exists(), "legacy_boot_grant_store_io_zero_contract");
    let one = endpoint("one");
    let two = endpoint("two");
    store.save(grant(&one, "nonce", 7)).await.unwrap();
    store.save(grant(&two, "nonce", 8)).await.unwrap();
    let restored = FileGrantStore::new(Some(root));
    assert_eq!(
        successful(restored.load_all(&[one, two]).await.unwrap()).len(),
        2,
        "grant_complete_endpoint_key_contract"
    );
}

#[tokio::test]
async fn grant_restoration_scopes_verified_foreign_and_current_codec_failures() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("grants");
    let store = FileGrantStore::new(Some(root.clone()));
    let current = endpoint("current");
    let foreign = endpoint("foreign");
    let good = grant(&current, "good", 7);
    store.save(good.clone()).await.unwrap();
    store.save(grant(&foreign, "future", 8)).await.unwrap();
    let future_path = root.join(format!(
        "{}.json",
        durable::key(&(&foreign, "future")).unwrap()
    ));
    let mut future: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&future_path).unwrap()).unwrap();
    future["schema"] = 2.into();
    std::fs::write(&future_path, serde_json::to_vec(&future).unwrap()).unwrap();
    let restored = store
        .load_all(std::slice::from_ref(&current))
        .await
        .unwrap();
    assert_eq!(
        restored,
        GrantRestoration::from_grants(vec![good.clone()]),
        "foreign_endpoint_failure_does_not_block_current_contract"
    );

    let restored = store
        .load_all(&[current.clone(), foreign.clone()])
        .await
        .unwrap();
    assert_eq!(
        restored.by_endpoint.get(&current),
        Some(&Ok(vec![good])),
        "current_endpoint_grant_survives_foreign_codec_error_contract"
    );
    assert_eq!(
        restored.by_endpoint.get(&foreign),
        Some(&Err(Unverified::Cache)),
        "current_endpoint_newer_fails_only_current_contract"
    );
    assert!(
        !restored.unknown_failure,
        "known_codec_failure_is_not_unknown_contract"
    );

    future["endpoint"]["execution_node"] = "forged-owner".into();
    std::fs::write(&future_path, serde_json::to_vec(&future).unwrap()).unwrap();
    assert!(
        store
            .load_all(&[current, foreign])
            .await
            .unwrap()
            .unknown_failure,
        "unowned_failure_blocks_all_authority_contract"
    );
}

#[tokio::test]
async fn grant_save_rejects_non_roundtripping_authority_before_creating_store_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("grants");
    let store = FileGrantStore::new(Some(root.clone()));
    let endpoint = endpoint("one");
    let mut wrong = ServerWitness::for_test(7);
    wrong.socket = "/fixture/another.sock".into();
    let invalid = super::super::permit::grant_for_test(&endpoint, "nonce", wrong);
    assert!(
        store.save(invalid).await.is_err(),
        "grant_save_roundtrip_is_required_before_store_contract"
    );
    assert!(
        !root.exists(),
        "invalid_grant_creates_no_store_files_contract"
    );
}

#[test]
fn durable_grant_codec_binds_endpoint_nonce_and_witness_without_promoting_create_proof() {
    let endpoint = endpoint("one");
    let expected = grant(&endpoint, "nonce", 7);
    let bytes = expected.encode().unwrap();
    assert_eq!(
        LaunchAdmissionGrant::decode(&bytes, &endpoint, "nonce").unwrap(),
        expected
    );
    assert!(
        LaunchAdmissionGrant::decode(&bytes, &endpoint, "other").is_err(),
        "grant_codec_exact_nonce_contract"
    );
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["witness"]["socket"] = "/another/socket".into();
    assert!(
        LaunchAdmissionGrant::decode_any(&serde_json::to_vec(&value).unwrap()).is_err(),
        "grant_codec_witness_endpoint_contract"
    );
}

#[test]
fn durable_grant_codec_requires_version_and_complete_validation_tokens() {
    let endpoint = endpoint("one");
    let expected = grant(&endpoint, "nonce", 7);
    let value: serde_json::Value = serde_json::from_slice(&expected.encode().unwrap()).unwrap();
    for missing in ["config", "checker"] {
        let mut incomplete = value.clone();
        incomplete.as_object_mut().unwrap().remove(missing);
        assert!(
            LaunchAdmissionGrant::decode_any(&serde_json::to_vec(&incomplete).unwrap()).is_err(),
            "grant_codec_requires_validation_tokens_contract: {missing}"
        );
    }
    let mut zero = value.clone();
    zero["schema"] = 0.into();
    assert!(
        LaunchAdmissionGrant::decode_any(&serde_json::to_vec(&zero).unwrap()).is_err(),
        "grant_codec_requires_known_schema_contract"
    );
    let mut future = value;
    future["schema"] = 2.into();
    future["future_field"] = true.into();
    let result = LaunchAdmissionGrant::decode_any(&serde_json::to_vec(&future).unwrap());
    assert!(
        matches!(result, Err(ref reason) if reason.starts_with("admission_grant_newer:")),
        "grant_newer_schema_is_not_corruption_contract: {result:?}"
    );
}
