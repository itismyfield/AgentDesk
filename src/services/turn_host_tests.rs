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

const DELEGATED: u64 = 9_200_000_000_000_001;

/// What the turn's two judgements return for `channel`, and whether either read the session key.
async fn judged(channel: u64) -> (TurnHost, Option<HerdrRefusal>, bool) {
    let asked = std::cell::Cell::new(false);
    let key = || {
        asked.set(true);
        async { Some(SESSION_KEY.to_owned()) }
    };
    let first = refusal_before_turn(None, &ProviderKind::Claude, channel, key).await;
    let host = for_turn(None, &ProviderKind::Claude, channel, Some(SESSION_KEY)).await;
    (host, first, asked.get())
}

// A channel with no home registered here keeps the existing path whether the registry was never
// used or holds other channels; a delegated one runs only while held with intake open.
#[tokio::test]
async fn a_delegated_channel_runs_a_turn_only_where_its_home_is_held_with_intake_open() {
    use crate::db::o_channel_homes::HomeState;
    use crate::services::cluster::channel_home::register_for_test as register;
    assert!(!channel_home::any_registered(), "never registered");
    let unregistered = judged(8).await;
    assert!(matches!(unregistered, (TurnHost::Tmux, None, false)));
    register(DELEGATED + 1, Some(HomeState::Worker));
    let unregistered = judged(8).await;
    assert!(matches!(unregistered, (TurnHost::Tmux, None, false)));

    let home = register(DELEGATED, Some(HomeState::Worker));
    assert!(matches!(
        judged(DELEGATED).await,
        (TurnHost::Tmux, None, false)
    ));
    let refused = |(host, first, asked): (TurnHost, Option<HerdrRefusal>, bool), want| {
        assert_eq!((first, asked), (Some(Clone::clone(&want)), false));
        assert_eq!(refusal(host), want);
    };
    home.close_intake();
    refused(judged(DELEGATED).await, HerdrRefusal::HomeDraining);
    let _open = force_admission(Admission::new(Some("on".as_ref()), None));
    let switch = force_switch_for_test(Some(true));
    let draining = intake_refusal_before_turn(None, &ProviderKind::Claude, DELEGATED, || async {
        Some(SESSION_KEY.to_owned())
    });
    assert_eq!(draining.await, Some(HerdrRefusal::HomeDraining));
    drop(switch);
    // A Herdr-configured channel is judged on its home before its endpoint.
    let _hosts = force_hosts(Some("mac-mini"), &[(DELEGATED, "mac-book")]);
    refused(judged(DELEGATED).await, HerdrRefusal::HomeDraining);
    home.close();
    refused(judged(DELEGATED).await, HerdrRefusal::HomeNotHeld);
    register(DELEGATED, None);
    refused(judged(DELEGATED).await, HerdrRefusal::HomeNotHeld);
}

// The turn takes its mailbox before this check, so a drain closing intake right after it reads
// the turn as running and waits; a turn checked after the close is refused.
#[tokio::test]
async fn a_close_right_after_the_check_finds_the_turn_in_its_mailbox() {
    use crate::db::o_channel_homes::HomeState;
    use crate::services::cluster::channel_home_drain::DrainPort;
    use crate::services::cluster::channel_home_port::ChannelHomePort;
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    const CHECKED: u64 = DELEGATED + 2;
    let home = channel_home::register_for_test(CHECKED, Some(HomeState::Worker));
    let mailboxes = ChannelMailboxRegistry::default();
    let mailbox = mailboxes.handle(ChannelId::new(CHECKED));
    let token = std::sync::Arc::new(CancelToken::new());
    assert!(
        mailbox
            .try_start_turn(token, UserId::new(1), MessageId::new(1))
            .await
    );
    let host = for_turn(None, &ProviderKind::Claude, CHECKED, None).await;
    assert!(matches!(host, TurnHost::Tmux), "{host:?}");
    home.close_intake();
    let restored = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let port = ChannelHomePort::new(CHECKED, std::sync::Arc::default(), restored);
    assert_eq!(port.turn_running().await, Some(true), "the drain waits");
    let late = for_turn(None, &ProviderKind::Claude, CHECKED, None).await;
    assert_eq!(refusal(late), HerdrRefusal::HomeDraining);
    mailboxes.remove_fixture_for_test(ChannelId::new(CHECKED));
}
