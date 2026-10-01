//! The runtime-kind recreate against the rows stored under the turn's own key.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record, wire};
use crate::services::discord::host_defer_gate::tests::{Case, postgres};
use crate::services::discord::host_key_derivation::tests::seed_row;
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, shared_on};
use crate::services::provider_teardown::tests::test_support::FakeTmux;

fn wrapper_binding() -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::LegacyTmuxWrapper,
        output_path: "/runtime/p4c1-wrapper.jsonl".to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    }
}

/// Runs the recreate on a live wrapper session and reports whether it was killed and
/// whether its runtime binding survived.
async fn reconcile(
    shared: &SharedData,
    tmux: &FakeTmux,
    channel: u64,
    key: Option<&str>,
    name: &str,
) -> (bool, bool) {
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(name, wrapper_binding());
    reconcile_managed_tmux_runtime_kind_for_config(
        shared,
        &ProviderKind::Claude,
        serenity::ChannelId::new(channel),
        key,
        Some(name),
        Some(RuntimeHandoffKind::ClaudeTui),
    )
    .await;
    let killed = tmux
        .take_calls()
        .iter()
        .any(|c| c.starts_with("kill-session"));
    let bound = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name);
    (killed, bound.is_some())
}

// A live session of the wrong runtime kind is killed and forgotten only when the turn's
// key reads a legacy row or no row yet; a kept session keeps its pane and binding.
#[tokio::test]
async fn a_runtime_kind_mismatch_recreates_only_what_the_turn_key_admits_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    let tmux = FakeTmux::install("-");
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let channel_of = |n: u64| 1_479_671_301_387_062_000 + n;
    for (n, case) in Case::ALL.into_iter().enumerate() {
        let name = format!("AgentDesk-claude-p4c1-kind-{n}");
        let key = channel_key(&shared, &name);
        case.seed(&pool, &key, &name, channel_of(n as u64)).await;
        let got = reconcile(&shared, &tmux, channel_of(n as u64), Some(&key), &name).await;
        assert_eq!(got, (case.admitted(), !case.admitted()), "{case:?}");
    }

    // A row written under the channel's old name still decides through its channel row.
    for (n, stored) in [(80, Stored::Legacy), (81, Stored::Hosted)] {
        let old = format!("AgentDesk-claude-p4c1-kind-old-{n}");
        let mut row_owner = owner(&channel_of(n).to_string());
        row_owner.discord_token_hash = shared.token_hash.clone();
        let raw =
            (stored == Stored::Hosted).then(|| wire(&record(&row_owner, "n1", HostedState::Bound)));
        let hash = Some(shared.token_hash.as_str());
        let old_key = channel_key(&shared, &old);
        seed_row(&pool, "claude", hash, &old_key, channel_of(n), raw).await;
        let name = format!("AgentDesk-claude-p4c1-kind-new-{n}");
        let key = channel_key(&shared, &name);
        let got = reconcile(&shared, &tmux, channel_of(n), Some(&key), &name).await;
        let admitted = stored == Stored::Legacy;
        assert_eq!(got, (admitted, !admitted), "renamed {stored:?}");
    }

    let unkeyed = reconcile(
        &shared,
        &tmux,
        channel_of(90),
        None,
        "AgentDesk-claude-p4c1-nokey",
    );
    assert_eq!(
        unkeyed.await,
        (true, false),
        "a turn with no key keeps main's path"
    );
    pool.close().await;
    let name = "AgentDesk-claude-p4c1-kind-0";
    let key = channel_key(&shared, name);
    let unread = reconcile(&shared, &tmux, channel_of(0), Some(&key), name).await;
    assert_eq!(
        unread,
        (false, true),
        "a failed row read is not a legacy answer"
    );
    db.drop().await;
}
