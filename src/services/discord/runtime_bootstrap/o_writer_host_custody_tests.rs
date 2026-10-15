//! The Codex takeover entry against a real PG open intake and Legacy's real pending starts.

use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;

use super::super::o_writer_host::test_host;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::tui_direct_pending_start::{
    PendingStartState, TuiDirectPendingStart, delete, persist,
};
use crate::services::message_outbox::{OutboxMessage, enqueue_outbox_pg_returning_id_with_ttl};
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::shadow::ShadowProvider::{self, Claude, Codex};
use crate::services::tui_o::store::{OStore, StoreConfig};
use crate::services::tui_o::writer::activation::{activate_codex_takeover, activate_with};
use crate::services::tui_o::writer::host::{Custody, FencedFacts, HostIo};

async fn open_intake(pool: &PgPool, channel: u64, provider: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status, admission_kind
         ) VALUES ('worker', 'leader', $1, $2, 'user', 'hi', 'standard', 'agent', $3,
            'pending', 'local') RETURNING id",
    )
    .bind(channel.to_string())
    .bind(format!("message-{channel}"))
    .bind(provider)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn pending(provider: ShadowProvider, channel: u64) -> TuiDirectPendingStart {
    let provider = match provider {
        Claude => "claude",
        Codex => "codex",
    };
    TuiDirectPendingStart {
        provider: provider.into(),
        channel_id: channel,
        tmux_session_name: "takeover-custody".into(),
        prompt_text: "hi".into(),
        anchor_message_id: 7,
        lease_relay_owner: "tmux_watcher".into(),
        lease_runtime_kind: None,
        lease_turn_id: None,
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
        native_turn_id: None,
    }
}

/// Fenced facts through the gateway's own intake fence, released before the caller acts.
async fn fenced(host: &impl HostIo, channel: u64, provider: ShadowProvider) -> FencedFacts {
    tokio::time::timeout(Duration::from_secs(5), host.intake_fence(channel, provider))
        .await
        .unwrap()
        .unwrap()
}

/// With Legacy's open intake row and pending start for the turn it keeps, only the Codex entry
/// publishes; the strict entry still refuses both, and terminal custody or a queued body still
/// refuse a Codex takeover.
#[test]
fn codex_takeover_passes_open_intake_and_a_pending_start_the_strict_entry_refuses_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(takeover());
}

async fn takeover() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let local = crate::services::cluster::node_registry::SELF_INSTANCE_ID.get();
    let local = local.map_or("custody-local", String::as_str);
    let host = test_host::over(shared, Duration::ZERO, Some(local));
    let stores = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, stores.path());
    let store = store.unwrap().unwrap();
    let (codex, claude, terminal, queued) = (64_700_401, 64_700_402, 64_700_403, 64_700_404);
    for (channel, provider) in [(codex, Codex), (claude, Claude), (terminal, Codex)] {
        open_intake(
            &pool,
            channel,
            if provider == Codex { "codex" } else { "claude" },
        )
        .await;
        persist(&pending(provider, channel)).unwrap();
        assert_eq!(
            host.local_custody(channel, provider),
            Ok(Custody::PendingStart)
        );
    }
    persist(&pending(Codex, queued)).unwrap();
    let body = OutboxMessage {
        target: &format!("channel:{queued}"),
        content: "body",
        bot: "captain",
        source: "headless_turn",
        reason_code: None,
        session_key: None,
    };
    enqueue_outbox_pg_returning_id_with_ttl(&pool, body, 0)
        .await
        .unwrap()
        .unwrap();
    let key = format!("takeover-custody-{terminal}");
    let payload = json!({"channel_id": terminal});
    crate::services::discord::terminal_delivery_custody::persist(&key, &payload)
        .await
        .unwrap();
    assert_eq!(host.local_custody(terminal, Codex), Ok(Custody::Active));

    for (channel, provider) in [(codex, Codex), (claude, Claude)] {
        let FencedFacts { hold, facts, .. } = fenced(&host, channel, provider).await;
        hold.release().await.unwrap();
        assert_eq!(facts.open_intake, 1);
        let candidate = Candidate::new(Adoption::Deferred);
        let custody = || Ok(host.local_custody(channel, provider)?.active());
        let strict = activate_with(&store, channel, Ok(facts), custody, &candidate, || {
            Ok(Vec::new())
        });
        assert!(
            matches!(&strict, Err(detail) if detail.contains("open intake rows")),
            "{strict:?}"
        );
        assert_eq!(candidate.peek(), Adoption::Deferred);
    }
    sqlx::query("DELETE FROM intake_outbox WHERE channel_id = $1")
        .bind(claude.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let FencedFacts { hold, facts, .. } = fenced(&host, claude, Claude).await;
    hold.release().await.unwrap();
    let candidate = Candidate::new(Adoption::Deferred);
    let custody = || Ok(host.local_custody(claude, Claude)?.active());
    let strict = activate_with(&store, claude, Ok(facts), custody, &candidate, || {
        Ok(Vec::new())
    });
    assert!(
        matches!(&strict, Err(detail) if detail.contains("Legacy retains delivery custody")),
        "a pending start alone still refuses the strict entry: {strict:?}"
    );

    let refusals = [
        (terminal, "Legacy retains delivery custody"),
        (queued, "1 queued Legacy bodies"),
    ];
    for (channel, why) in refusals {
        let FencedFacts {
            hold,
            facts,
            queued_bodies,
        } = fenced(&host, channel, Codex).await;
        hold.release().await.unwrap();
        let candidate = Candidate::new(Adoption::Deferred);
        let custody = || Ok(host.local_custody(channel, Codex)?.blocks_takeover());
        let codex_entry = activate_codex_takeover(
            &store,
            channel,
            facts,
            queued_bodies,
            custody,
            &candidate,
            || Ok(Vec::new()),
        );
        assert!(
            matches!(&codex_entry, Err(detail) if detail.contains(why)),
            "{codex_entry:?}"
        );
        assert!(!store.has_channel_dir(channel));
    }

    let FencedFacts {
        hold,
        facts,
        queued_bodies,
    } = fenced(&host, codex, Codex).await;
    hold.release().await.unwrap();
    let candidate = Candidate::new(Adoption::Deferred);
    let custody = || Ok(host.local_custody(codex, Codex)?.blocks_takeover());
    let codex_entry = activate_codex_takeover(
        &store,
        codex,
        facts,
        queued_bodies,
        custody,
        &candidate,
        || Ok(Vec::new()),
    );
    assert_eq!(codex_entry, Ok(()));
    assert_eq!(candidate.peek(), Adoption::Committed);
    assert!(store.read_init(codex).unwrap().is_some());
    assert_eq!(host.local_custody(codex, Codex), Ok(Custody::PendingStart));

    for (channel, provider) in [
        (codex, Codex),
        (claude, Claude),
        (terminal, Codex),
        (queued, Codex),
    ] {
        delete(&pending(provider, channel));
    }
    pool.close().await;
    fixture.drop().await;
}
