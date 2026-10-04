use serde_json::Value;

use super::*;
use crate::config::session_hosts::force_for_test as force_hosts;
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};
use crate::services::herdr_admission::{Admission, force_for_test as force_admission};

const CHANNEL: u64 = 1_490_141_479_707_086_938;
const SESSION_KEY: &str = "claude/discord_0123456789abcdef/test-node:AgentDesk-claude-hosted";

async fn seed(pool: &PgPool, raw: Option<Value>) {
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)
         ON CONFLICT (session_key) DO UPDATE SET hosted_execution = EXCLUDED.hosted_execution",
    )
    .bind(SESSION_KEY)
    .bind(&owner(&CHANNEL.to_string()).discord_token_hash)
    .bind(CHANNEL.to_string())
    .bind(raw)
    .execute(pool)
    .await
    .unwrap();
}

fn recorded(state: HostedState) -> Option<Value> {
    let record = record(&owner(&CHANNEL.to_string()), "nonce-1", state);
    Some(serde_json::to_value(record).unwrap())
}

fn refusal(host: TurnHost) -> HerdrRefusal {
    match host {
        TurnHost::Refused(refusal) => refusal,
        other => panic!("{other:?}"),
    }
}

// A configured channel is refused for its provider or a remote endpoint before the row is read,
// and without a readable row before admission; none of it falls back to tmux.
#[tokio::test]
async fn configured_turn_refuses_provider_and_remote_endpoint_before_the_row_and_an_unread_row() {
    let _hosts = force_hosts(Some("mac-mini"), &[(CHANNEL, "mac-mini"), (7, "mac-book")]);
    let _open = force_admission(Admission::new(Some("on".as_ref()), None));
    let judge = |provider: ProviderKind, channel| async move {
        refusal(for_turn(None, &provider, channel, Some(SESSION_KEY)).await)
    };
    assert_eq!(
        judge(ProviderKind::Codex, CHANNEL).await,
        HerdrRefusal::ProviderUnsupported {
            provider: "codex".into()
        }
    );
    assert_eq!(
        judge(ProviderKind::Claude, 7).await,
        HerdrRefusal::EndpointNotLocal {
            node: "mac-book".into()
        }
    );
    assert!(matches!(
        judge(ProviderKind::Claude, CHANNEL).await,
        HerdrRefusal::HostedRowUnreadable { .. }
    ));
    let _unnamed = force_hosts(None, &[(CHANNEL, "mac-mini")]);
    assert_eq!(
        judge(ProviderKind::Claude, CHANNEL).await,
        HerdrRefusal::EndpointNotLocal {
            node: "mac-mini".into()
        },
        "a node without cluster.instance_id runs no endpoint"
    );
    // An unconfigured channel without a readable row keeps its existing path.
    assert!(matches!(
        for_turn(None, &ProviderKind::Claude, 8, Some(SESSION_KEY)).await,
        TurnHost::Tmux
    ));
}

// Unconfigured: a Pending, Bound or unreadable Herdr row refuses, any other row keeps tmux.
// Configured: an unreadable record refuses, a readable row goes on to the admission switch.
#[tokio::test(flavor = "multi_thread")]
async fn unconfigured_channel_with_a_herdr_row_is_refused_and_a_configured_one_needs_a_readable_row_pg()
 {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let judge = || {
        for_turn(
            Some(&pool),
            &ProviderKind::Claude,
            CHANNEL,
            Some(SESSION_KEY),
        )
    };
    assert!(matches!(judge().await, TurnHost::Tmux), "no sessions row");
    for (raw, expected) in [
        (None, None),
        (recorded(HostedState::Retired), None),
        (
            recorded(HostedState::Pending),
            Some(Some(HostedState::Pending)),
        ),
        (recorded(HostedState::Bound), Some(Some(HostedState::Bound))),
        (Some(serde_json::json!({"schema": 99})), Some(None)),
    ] {
        seed(&pool, raw.clone()).await;
        match (judge().await, expected) {
            (TurnHost::Tmux, None) => {}
            (TurnHost::Refused(HerdrRefusal::HostedHerdrUnconfigured { state }), Some(want)) => {
                assert_eq!(state, want, "{raw:?}")
            }
            (host, _) => panic!("{raw:?}: {host:?}"),
        }
    }

    let _hosts = force_hosts(Some("mac-mini"), &[(CHANNEL, "mac-mini")]);
    let _off = force_admission(Admission::new(Some("off".as_ref()), None));
    assert!(matches!(
        refusal(judge().await),
        HerdrRefusal::HostedRowUnreadable { .. }
    ));
    seed(&pool, recorded(HostedState::Bound)).await;
    assert_eq!(
        refusal(judge().await),
        HerdrRefusal::AdmissionStopped {
            cause: StopCause::Env
        }
    );
    pool.close().await;
    db.drop().await;
}
