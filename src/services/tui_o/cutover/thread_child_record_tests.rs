//! Thread children are recorded only from channels Discord itself returned, and a boot
//! reservation's input waits for one checked parent read.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::channel_policy::Adoption;

const PARENT: u64 = 8_118_042;

fn boot() -> BootChannels {
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "a", "name": "A", "channels":
            {"claude": {"id": PARENT.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"all_tui": false, "channels": [PARENT]}}
    }))
    .unwrap();
    BootChannels::validate(&config)
        .unwrap()
        .adopted(Adoption::Committed)
}

fn discord(channel: u64, parent: u64, kind: u8) -> poise::serenity_prelude::GuildChannel {
    serde_json::from_value(serde_json::json!({
        "id": channel.to_string(), "parent_id": parent.to_string(), "type": kind, "name": "fixture"
    }))
    .unwrap()
}

fn lookup(read: Result<Option<poise::serenity_prelude::GuildChannel>, String>) -> ChannelLookup {
    std::sync::Arc::new(move |_| {
        let read = read.clone();
        Box::pin(async move { read })
    })
}

#[test]
fn a_thread_child_is_recorded_only_from_a_returned_thread_under_a_selected_parent() {
    let boot = boot();
    let _forced = test_override::force_boot(boot.clone());
    assert!(!record_thread_child(&discord(PARENT + 1, 99, 11), true));
    assert!(!record_thread_child(&discord(PARENT + 2, PARENT, 0), true));
    assert!(boot.child(PARENT + 1).is_none() && boot.child(PARENT + 2).is_none());
    // A thread this process just made starts undecided; one found later starts late.
    assert!(record_thread_child(&discord(PARENT + 3, PARENT, 11), true));
    assert!(record_thread_child(&discord(PARENT + 4, PARENT, 11), false));
    let fresh = boot.child(PARENT + 3).unwrap().unwrap();
    assert_eq!((fresh.parent, fresh.kind), (PARENT, ClaudeTui));
    assert_eq!(fresh.candidate.peek(), Adoption::Pending);
    assert!(
        boot.child(PARENT + 4)
            .unwrap()
            .unwrap()
            .candidate
            .late_adopt_origin()
    );
}

#[tokio::test]
async fn a_reserved_thread_reads_its_parent_once_before_placement_and_holds_on_failure() {
    let boot = boot();
    let (child, plain) = (PARENT + 5, PARENT + 6);
    boot.reserve_boot_children(&[(child, PARENT), (plain, PARENT)]);
    let _forced = test_override::force_boot(boot.clone());
    let reserved = boot.unresolved_child(child).unwrap();
    // A failed read holds the input and leaves the reservation as it was.
    let failed = resolve_reserved_with(child, Some(lookup(Err("down".into())))).await;
    assert!(failed.unwrap_err().contains("parent read failed"));
    assert!(boot.unresolved_child(child).is_some());
    assert!(resolve_reserved_with(child, None).await.is_err());
    // A returned thread under the selected parent becomes its child on the same adoption.
    let read = Ok(Some(discord(child, PARENT, 11)));
    assert_eq!(
        resolve_reserved_with(child, Some(lookup(read))).await,
        Ok(())
    );
    let resolved = boot.child(child).unwrap().unwrap();
    assert_eq!(resolved.parent, PARENT);
    *reserved.lock() = Adoption::Held;
    assert_eq!(resolved.candidate.peek(), Adoption::Held);
    // A channel that is no thread leaves the reservation to Legacy for good.
    let text = Ok(Some(discord(plain, PARENT, 0)));
    assert_eq!(
        resolve_reserved_with(plain, Some(lookup(text))).await,
        Ok(())
    );
    assert!(boot.unresolved_child(plain).is_none() && boot.child(plain).is_none());
}

#[tokio::test]
async fn a_placement_on_a_reserved_thread_never_routes_before_its_parent_is_read() {
    use crate::services::tui_o::cutover::intake_route::{IntakeRoute, for_input_placement};
    let boot = boot();
    let child = PARENT + 7;
    boot.reserve_boot_children(&[(child, PARENT)]);
    let _forced = test_override::force_boot(boot.clone());
    // This process has no Discord read that can succeed, so the input is held, not placed.
    assert!(matches!(
        for_input_placement("claude", child).await,
        IntakeRoute::Hold(_)
    ));
    assert!(boot.unresolved_child(child).is_some());
}
