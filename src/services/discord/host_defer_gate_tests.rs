use poise::serenity_prelude::ChannelId;

use crate::db::dispatched_sessions::hosted_execution::test_support::{owner, pending, wire};
use crate::services::discord::host_defer_gate::tests::{
    Case, Nameless, isolated_runtime_root, map_channel, postgres, with_second_bot,
};
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, shared_on};
use crate::services::discord::inflight;
use crate::services::provider::ProviderKind;

// The queued-turn promote gate holds a session the host guard keeps at the queue front;
// a legacy row, or a row the promoted turn has not written yet, promotes as in main.
#[tokio::test]
async fn the_promote_gate_holds_only_what_the_host_guard_keeps_pg() {
    let _root = isolated_runtime_root();
    let _tmux = crate::services::provider_teardown::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let channel_of = |n: usize| ChannelId::new(1_479_671_301_387_061_000 + n as u64);
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let channel = channel_of(n);
        let channel_name = format!("p4c1-promote-{n}");
        let name = provider.build_tmux_session_name(&channel_name);
        map_channel(&shared, channel, &channel_name).await;
        case.seed(&pool, &channel_key(&shared, &name), &name, channel.get())
            .await;
        let held = gate(&shared, &provider, channel).await;
        assert_eq!(held, !case.admitted(), "{case:?}");
    }
    pool.close().await;
    let unread = gate(&shared, &provider, channel_of(0)).await;
    assert!(unread, "a failed row read is not a legacy answer");
    db.drop().await;
}

// A row written under another tmux name, with no alias for the name the channel now builds,
// still holds the promotion through its channel row; a legacy or absent row promotes.
#[tokio::test]
async fn the_promote_gate_reads_the_channel_row_under_another_name_pg() {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::test_support::{future_schema, record};
    let _root = isolated_runtime_root();
    let _tmux = crate::services::provider_teardown::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let cases = [
        Stored::Legacy,
        Stored::Hosted,
        Stored::Future,
        Stored::Missing,
    ];
    for (n, stored) in cases.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_062_000 + n as u64);
        map_channel(&shared, channel, &format!("p4c1-renamed-{n}")).await;
        let old = provider.build_tmux_session_name(&format!("p4c1-old-{n}"));
        let key = channel_key(&shared, &old);
        let mut row_owner = owner(&channel.get().to_string());
        row_owner.discord_token_hash = shared.token_hash.clone();
        let raw = match stored {
            Stored::Hosted => Some(wire(&record(&row_owner, "n1", HostedState::Bound))),
            Stored::Future => Some(future_schema(&row_owner)),
            _ => None,
        };
        if stored != Stored::Missing {
            let hash = Some(shared.token_hash.as_str());
            let seed = crate::services::discord::host_key_derivation::tests::seed_row;
            seed(&pool, "claude", hash, &key, channel.get(), raw).await;
        }
        let held = gate(&shared, &provider, channel).await;
        let expected = matches!(stored, Stored::Hosted | Stored::Future);
        assert_eq!(held, expected, "{stored:?}");
    }
    db.drop().await;
}

// A Herdr-configured channel leaves every row, live or retired, named or not, to its turn host;
// the same row on an unconfigured channel, or an unreadable one there, still holds the queue.
#[tokio::test]
async fn the_promote_gate_leaves_a_herdr_channel_to_its_turn_host_pg() {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::test_support::record;
    let _root = isolated_runtime_root();
    let _tmux = crate::services::provider_teardown::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let channel_of = |n: u64| ChannelId::new(1_479_671_301_387_063_000 + n);
    let configured: Vec<_> = (0..6).map(|n| (channel_of(n).get(), "mac-mini")).collect();
    let _hosts = crate::config::session_hosts::force_for_test(Some("mac-mini"), &configured);
    // (state, configured, channel session, held)
    let cases = [
        (Some(HostedState::Retired), true, true, false),
        (Some(HostedState::Bound), true, true, false),
        (None, true, true, false),
        (Some(HostedState::Retired), true, false, false),
        (Some(HostedState::Bound), true, false, false),
        (None, true, false, false),
        (Some(HostedState::Retired), false, true, true),
    ];
    for (n, (state, on_herdr, mapped, held)) in cases.into_iter().enumerate() {
        let channel = if on_herdr {
            channel_of(n as u64)
        } else {
            channel_of(10)
        };
        let channel_name = format!("p11-q-promote-{n}");
        if mapped {
            map_channel(&shared, channel, &channel_name).await;
        }
        let name = provider.build_tmux_session_name(&channel_name);
        let mut row_owner = owner(&channel.get().to_string());
        row_owner.discord_token_hash = shared.token_hash.clone();
        let raw = match state {
            Some(state) => wire(&record(&row_owner, "n1", state)),
            None => wire(&pending(&row_owner, "n1")),
        };
        let seed = crate::services::discord::host_key_derivation::tests::seed_row;
        let hash = Some(shared.token_hash.as_str());
        seed(
            &pool,
            "claude",
            hash,
            &channel_key(&shared, &name),
            channel.get(),
            Some(raw),
        )
        .await;
        assert_eq!(
            gate(&shared, &provider, channel).await,
            held,
            "{state:?} configured={on_herdr} mapped={mapped}"
        );
    }
    pool.close().await;
    assert!(
        !gate(&shared, &provider, channel_of(1)).await,
        "a Herdr channel reads no row here"
    );
    assert!(
        gate(&shared, &provider, channel_of(10)).await,
        "an unreadable row never reads as retired"
    );
    db.drop().await;
}

/// What the channel rows say to a runtime that holds no channel name for the channel.
// A nameless or absent unregistered session holds the promote unless a legacy row admits it;
// no row promotes only with nothing in flight.
#[tokio::test]
async fn the_promote_gate_reads_the_channel_row_with_no_channel_name_pg() {
    let _root = isolated_runtime_root();
    let _tmux = crate::services::provider_teardown::test_support::FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let (shared, _registry) = with_second_bot(&pool, "p4c1f-second-bot").await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    let channel_of = |n: usize| ChannelId::new(1_479_671_301_387_065_000 + n as u64);
    let own = shared.token_hash.clone();
    for (n, case) in Nameless::ALL.into_iter().enumerate() {
        let channel = channel_of(n);
        let name = provider.build_tmux_session_name(&format!("p4c1f-promote-{n}"));
        map_channel(&shared, channel, "unnamed").await;
        let mut core = shared.core.lock().await;
        core.sessions.get_mut(&channel).unwrap().channel_name = None;
        drop(core);
        case.seed(&pool, &own, "p4c1f-second-bot", channel.get(), &name)
            .await;
        let held = gate(&shared, &provider, channel).await;
        let promoted = matches!(case, Nameless::Legacy | Nameless::Missing);
        assert_eq!(held, !promoted, "{case:?}");
    }
    for (n, case) in [(10, Nameless::Hosted), (11, Nameless::Legacy)] {
        let name = provider.build_tmux_session_name(&format!("p4c1f-promote-{n}"));
        case.seed(&pool, &own, "p4c1f-second-bot", channel_of(n).get(), &name)
            .await;
        let held = gate(&shared, &provider, channel_of(n)).await;
        assert_eq!(held, case != Nameless::Legacy, "no session, {case:?}");
    }
    let unkeyed = channel_of(12);
    assert!(!gate(&shared, &provider, unkeyed).await, "unkeyed, idle");
    let row = inflight::InflightTurnState::new(
        provider.clone(),
        unkeyed.get(),
        None,
        1,
        unkeyed.get() + 1,
        unkeyed.get() + 2,
        "p4c1f in flight".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    inflight::save_inflight_state_create_new(&row).expect("inflight row");
    assert!(
        gate(&shared, &provider, unkeyed).await,
        "unkeyed, in flight"
    );
    pool.close().await;
    let unread = gate(&shared, &provider, channel_of(0)).await;
    assert!(unread, "a failed row read is not a legacy answer");
    db.drop().await;
}

// A channel that left Herdr starts the queued turn that moves this node's Claude execution back
// to tmux, Bound or not yet back; a pending, foreign, other-provider or unreadable row still holds.
#[tokio::test]
async fn the_promote_gate_starts_only_the_turn_that_moves_this_node_s_claude_back_pg() {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::test_support::{future_schema, record};
    let _root = isolated_runtime_root();
    let _tmux = crate::services::provider_teardown::test_support::FakeTmux::install("-");
    let _hosts = crate::config::session_hosts::force_for_test(Some("test-node"), &[]);
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let provider = ProviderKind::Claude;
    let gate = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    // (case, state, execution node, provider, readable, held)
    let cases = [
        (
            "bound",
            HostedState::Bound,
            "test-node",
            "claude",
            true,
            false,
        ),
        (
            "retired, not back yet",
            HostedState::Retired,
            "test-node",
            "claude",
            true,
            false,
        ),
        (
            "pending",
            HostedState::Pending,
            "test-node",
            "claude",
            true,
            true,
        ),
        (
            "another node",
            HostedState::Bound,
            "another-node",
            "claude",
            true,
            true,
        ),
        (
            "another provider",
            HostedState::Bound,
            "test-node",
            "codex",
            true,
            true,
        ),
        (
            "unreadable",
            HostedState::Bound,
            "test-node",
            "claude",
            false,
            true,
        ),
    ];
    for (n, (case, state, node, by, readable, held)) in cases.into_iter().enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_066_000 + n as u64);
        let channel_name = format!("i7503-return-{n}");
        map_channel(&shared, channel, &channel_name).await;
        let name = provider.build_tmux_session_name(&channel_name);
        let mut row_owner = owner(&channel.get().to_string());
        row_owner.discord_token_hash = shared.token_hash.clone();
        row_owner.provider = by.into();
        let mut stored = record(&row_owner, "n1", state);
        if let Some(at) = stored.location.as_mut() {
            at.execution_node = node.into();
        }
        let raw = match readable {
            true => wire(&stored),
            false => future_schema(&row_owner),
        };
        let seed = crate::services::discord::host_key_derivation::tests::seed_row;
        let hash = Some(shared.token_hash.as_str());
        let key = channel_key(&shared, &name);
        seed(&pool, "claude", hash, &key, channel.get(), Some(raw)).await;
        assert_eq!(gate(&shared, &provider, channel).await, held, "{case}");
    }
    db.drop().await;
}
