use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn discord(channel: u64, parent: u64) -> poise::serenity_prelude::GuildChannel {
    serde_json::from_value(serde_json::json!({
        "id": channel.to_string(), "parent_id": parent.to_string(), "type": 11, "name": "fixture"
    }))
    .unwrap()
}

fn install(
    provider: &str,
    read: Result<Option<poise::serenity_prelude::GuildChannel>, String>,
) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    install_channel_lookup(
        provider,
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            let read = read.clone();
            Box::pin(async move { read })
        }),
    );
    calls
}

fn calls(counts: &[Arc<AtomicUsize>]) -> Vec<usize> {
    counts
        .iter()
        .map(|count| count.load(Ordering::SeqCst))
        .collect()
}

#[tokio::test]
async fn thread_lookup_uses_parent_boot_provider_regardless_of_install_order() {
    for (kind, provider) in [(ClaudeTui, "claude"), (CodexTui, "codex")] {
        for order in [
            ["antigravity", "codex", "claude"],
            ["claude", "codex", "antigravity"],
        ] {
            let _boot = test_override::force_channels(&[(41, kind)]);
            let _lookups = test_override::force_channel_lookups();
            let counts: Vec<_> = order
                .iter()
                .map(|name| {
                    install(
                        name,
                        if *name == provider {
                            Ok(Some(discord(42, 41)))
                        } else {
                            Err("Missing Access".into())
                        },
                    )
                })
                .collect();
            assert_eq!(record_thread_child_by_id(42, 41).await, Ok(true));
            assert_eq!(
                calls(&counts),
                order.map(|name| usize::from(name == provider))
            );
            assert_eq!(
                boot_snapshot().unwrap().child(42).unwrap().unwrap().kind,
                kind
            );
        }
    }
}

#[tokio::test]
async fn reserved_placement_uses_its_provider_regardless_of_install_order() {
    use super::intake_route::for_input_placement;
    for (kind, provider) in [(ClaudeTui, "claude"), (CodexTui, "codex")] {
        for order in [
            ["antigravity", "codex", "claude"],
            ["claude", "codex", "antigravity"],
        ] {
            let _boot = test_override::force_channels(&[(51, kind)]);
            let boot = boot_snapshot().unwrap();
            boot.reserve_boot_children(&[(52, 51)]);
            let _lookups = test_override::force_channel_lookups();
            let counts: Vec<_> = order
                .iter()
                .map(|name| {
                    install(
                        name,
                        if *name == provider {
                            Ok(Some(discord(52, 51)))
                        } else {
                            Err("Missing Access".into())
                        },
                    )
                })
                .collect();
            let route = for_input_placement(provider, 52).await;
            assert!(boot.child(52).is_some(), "{provider}: {route:?}");
            assert!(boot.unresolved_child(52).is_none());
            assert_eq!(
                calls(&counts),
                order.map(|name| usize::from(name == provider))
            );
        }
    }
}

#[tokio::test]
async fn missing_parent_provider_never_uses_another_bot() {
    let _boot = test_override::force_channels(&[(61, ClaudeTui)]);
    let _lookups = test_override::force_channel_lookups();
    let other = install("codex", Ok(Some(discord(62, 61))));
    let error = record_thread_child_by_id(62, 61).await.unwrap_err();
    assert!(error.contains("provider=claude"), "{error}");
    assert!(boot_snapshot().unwrap().child(62).is_none());
    assert_eq!(calls(&[other]), [0]);
}

#[tokio::test]
async fn missing_placement_provider_keeps_reservation_and_never_uses_another_bot() {
    use super::intake_route::{IntakeRoute, for_input_placement};
    let _boot = test_override::force_channels(&[(71, ClaudeTui)]);
    let boot = boot_snapshot().unwrap();
    boot.reserve_boot_children(&[(72, 71)]);
    let _lookups = test_override::force_channel_lookups();
    let other = install("claude", Ok(Some(discord(72, 71))));
    // Placement's provider takes precedence even when a different parent provider is installed.
    let route = for_input_placement("codex", 72).await;
    assert!(
        matches!(route, IntakeRoute::Hold(ref detail) if detail.contains("provider=codex")),
        "{route:?}"
    );
    assert!(boot.unresolved_child(72).is_some());
    assert_eq!(calls(&[other]), [0]);
}

#[tokio::test]
async fn checked_lookup_errors_name_the_provider_and_keep_the_reservation() {
    use super::intake_route::{IntakeRoute, text_for_input_placement};
    let _boot = test_override::force_channels(&[(81, ClaudeTui)]);
    let boot = boot_snapshot().unwrap();
    boot.reserve_boot_children(&[(82, 81)]);
    let _lookups = test_override::force_channel_lookups();
    let denied = install("claude", Err("Missing Access".into()));
    let error = record_thread_child_by_id(83, 81).await.unwrap_err();
    assert!(
        error.contains("provider=claude") && error.contains("Missing Access"),
        "{error}"
    );
    let route = text_for_input_placement("claude", "82").await;
    assert!(
        matches!(route, IntakeRoute::Hold(ref detail) if detail.contains("provider=claude") && detail.contains("Missing Access")),
        "{route:?}"
    );
    assert!(boot.unresolved_child(82).is_some());
    assert_eq!(calls(&[denied]), [2]);
}

#[tokio::test]
async fn reinstalling_a_provider_replaces_its_lookup() {
    let _boot = test_override::force_channels(&[(91, ClaudeTui)]);
    let _lookups = test_override::force_channel_lookups();
    let old = install("claude", Err("stale client".into()));
    let new = install("claude", Ok(Some(discord(92, 91))));
    assert_eq!(record_thread_child_by_id(92, 91).await, Ok(true));
    assert_eq!(calls(&[old, new]), [0, 1]);
}
