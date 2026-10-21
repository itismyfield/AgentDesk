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
    assert_eq!(
        restored.load_all(&[endpoint.clone()]).await.unwrap(),
        [expected.clone()]
    );
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
    assert!(store.load_all(&[]).await.unwrap().is_empty());
    assert!(!root.exists(), "legacy_boot_grant_store_io_zero_contract");
    let one = endpoint("one");
    let two = endpoint("two");
    store.save(grant(&one, "nonce", 7)).await.unwrap();
    store.save(grant(&two, "nonce", 8)).await.unwrap();
    let restored = FileGrantStore::new(Some(root));
    assert_eq!(
        restored.load_all(&[one, two]).await.unwrap().len(),
        2,
        "grant_complete_endpoint_key_contract"
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
