use super::*;

#[tokio::test]
async fn thread_children_refresh_reader_precedes_unrelated_provider_return() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _channels = cutover::test_override::force_channels(&[(42, RuntimeHandoffKind::ClaudeTui)]);
    let init = serde_json::from_value(serde_json::json!({
        "channel":43,"policy_parent":42,"sources":[],"initial_anchor":0,
        "build_digest":"fixture","at":"2026-11-12T00:00:00Z"
    }))
    .unwrap();
    cutover::test_override::with_channels(|boot| boot.unwrap().record_child_fixture(&init));
    let boot = cutover::boot_snapshot().unwrap();
    assert_eq!(boot.child_fixture_held(43), Some(false));
    let channel = root.path().join("o_store/43");
    std::fs::create_dir_all(&channel).unwrap();
    std::fs::write(channel.join("init"), b"damaged").unwrap();
    refresh(&ProviderKind::Gemini, None).await;
    assert_eq!(boot.child_fixture_held(43), Some(true));
}
