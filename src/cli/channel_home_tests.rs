use std::cell::Cell;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;

const C: u64 = 1_490_141_479_707_086_938;

fn config(switch: Option<bool>) -> Config {
    let mut config = Config::default();
    config.runtime.channel_home_delegation_enabled = switch;
    config.cluster.instance_id = Some("gw".into());
    config
}

fn delegate_to(provider: &str, to: &str) -> ChannelHomeCommand {
    ChannelHomeCommand::Delegate {
        channel: C,
        provider: provider.into(),
        to: to.into(),
    }
}

async fn rows(pool: &PgPool) -> Vec<ChannelHome> {
    o_channel_homes::list_homes(pool).await.expect("list")
}

/// Runs the CLI entry on its own pool of `db`, which it closes, counting whether it connected.
async fn run(
    config: &Config,
    command: ChannelHomeCommand,
    db: &TestPostgresDb,
    connected: &Cell<usize>,
) -> Result<String, String> {
    let connect = || async {
        connected.set(connected.get() + 1);
        let url = &db.database_url;
        crate::db::postgres::connect_test_pool_with_max_connections(url, "channel-home cli", 2)
            .await
    };
    execute(config, command, connect).await
}

async fn intake(pool: &PgPool, key: &str, home_epoch: Option<i64>, status: &str) {
    sqlx::query(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status, claim_owner,
            home_epoch
         ) VALUES ('mini', 'gw', $1, $2, 'user', 'SECRET-USER-TEXT', 'standard', 'agent',
            'claude', $3, 'mini', $4)",
    )
    .bind(C.to_string())
    .bind(key)
    .bind(status)
    .bind(home_epoch)
    .execute(pool)
    .await
    .expect("seed intake row");
}

/// With the switch unset or off every mutation is refused before connecting, so nothing is
/// written; switched on, delegate stores the provider in the form intake rows carry.
#[tokio::test]
async fn mutations_are_refused_before_any_database_access_while_the_switch_is_off_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let connected = Cell::new(0);
    for switch in [None, Some(false)] {
        let config = config(switch);
        let commands = [
            delegate_to("claude", "mini"),
            ChannelHomeCommand::Reclaim { channel: C },
            ChannelHomeCommand::Force { channel: C },
        ];
        for command in commands {
            let refused = run(&config, command, &pg_db, &connected).await;
            assert!(refused.is_err_and(|e| e.contains("is off")));
        }
    }
    assert_eq!((connected.get(), rows(&pool).await), (0, vec![]));

    let on = config(Some(true));
    let inputs = [
        ("gemini", "mini", "provider"),
        ("claude", "gw", "target"),
        ("claude", " ", "target"),
    ];
    for (provider, to, reason) in inputs {
        let refused = run(&on, delegate_to(provider, to), &pg_db, &connected).await;
        assert!(
            refused.is_err_and(|e| e.contains(reason)),
            "{provider} to {to:?}"
        );
    }
    assert_eq!((connected.get(), rows(&pool).await), (0, vec![]));

    let delegated = run(&on, delegate_to(" Claude ", "mini"), &pg_db, &connected).await;
    delegated.expect("delegate");
    let homes = rows(&pool).await;
    let stored: Vec<_> = homes
        .iter()
        .map(|h| {
            (
                h.provider.as_str(),
                h.state,
                h.holder.as_deref(),
                h.target.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        stored,
        [("claude", HomeState::Releasing, Some("gw"), Some("mini"))]
    );
    pool.close().await;
    pg_db.drop().await;
}

/// `status` changes nothing, prints no intake text, and splits open intake by routed epoch so an
/// operator sees which retries no holder will claim.
#[tokio::test]
async fn status_reads_only_and_shows_open_intake_by_routed_epoch_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let connected = Cell::new(0);
    let off = config(None);
    let empty = run(&off, ChannelHomeCommand::Status, &pg_db, &connected).await;
    assert_eq!(empty.expect("status"), "{\n  \"homes\": []\n}");

    let channel = C.to_string();
    let first = o_channel_homes::delegate(&pool, &channel, "claude", "gw", "mini").await;
    let HomeWrite::Applied(first) = first.expect("delegate") else {
        panic!("delegated");
    };
    let left = o_channel_homes::finish_release(&pool, &channel, "gw", first.epoch).await;
    let HomeWrite::Applied(released) = left.expect("release") else {
        panic!("released");
    };
    intake(&pool, "finished", Some(released.epoch), "done").await;
    // One open route per channel: a retry kept at the old epoch, then one routed before the row.
    for (key, routed, open) in [
        ("older", Some(first.epoch), [0, 1, 0]),
        ("before", None, [0, 0, 1]),
    ] {
        sqlx::query("UPDATE intake_outbox SET status = 'done' WHERE user_msg_id <> 'finished'")
            .execute(&pool)
            .await
            .expect("settle earlier rows");
        intake(&pool, key, routed, "pending").await;
        let before = rows(&pool).await;
        let shown = run(&off, ChannelHomeCommand::Status, &pg_db, &connected).await;
        let shown = shown.expect("status");
        assert!(!shown.contains("SECRET-USER-TEXT"));
        let shown: Value = serde_json::from_str(&shown).expect("json");
        let home = &shown["homes"][0];
        let expected = (json!("released"), json!(released.epoch));
        assert_eq!((home["state"].clone(), home["epoch"].clone()), expected);
        let [current, other, unrouted] = open;
        let open = json!({"current_epoch": current, "other_epoch": other, "unrouted": unrouted});
        assert_eq!(home["open_intake"], open, "{key}");
        assert_eq!(rows(&pool).await, before, "status wrote nothing");
    }
    pool.close().await;
    pg_db.drop().await;
}

/// Delegate and reclaim name the node they ran on and the holder and target they set; reclaim and
/// force write their row change from the CLI, and force waits out F.
#[tokio::test]
async fn reclaim_and_force_run_from_the_cli_and_show_the_node_they_ran_on_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let (on, connected) = (config(Some(true)), Cell::new(0));
    let shown = run(&on, delegate_to("claude", "mini"), &pg_db, &connected).await;
    let shown: Value = serde_json::from_str(&shown.expect("delegate")).expect("json");
    let planned = json!({"holder": "gw", "target": "mini"});
    assert_eq!(
        (&shown["run_on"], &shown["planned"]),
        (&json!("gw"), &planned)
    );
    assert_eq!(shown["row"]["state"], "releasing");
    let channel = C.to_string();
    let first = rows(&pool).await[0].epoch;
    let released = o_channel_homes::finish_release(&pool, &channel, "gw", first).await;
    let HomeWrite::Applied(released) = released.expect("release") else {
        panic!("released");
    };
    let adopted = o_channel_homes::adopt(&pool, &channel, "mini", released.epoch).await;
    assert!(matches!(adopted, Ok(HomeWrite::Applied(_))), "{adopted:?}");

    let shown = run(
        &on,
        ChannelHomeCommand::Reclaim { channel: C },
        &pg_db,
        &connected,
    )
    .await;
    let shown: Value = serde_json::from_str(&shown.expect("reclaim")).expect("json");
    let planned = json!({"holder": "mini", "target": "gw"});
    assert_eq!(
        (&shown["run_on"], &shown["planned"]),
        (&json!("gw"), &planned)
    );
    let held = |homes: Vec<ChannelHome>| {
        let home = &homes[0];
        (home.state, home.holder.clone(), home.target.clone())
    };
    let reclaiming = (
        HomeState::Reclaiming,
        Some("mini".into()),
        Some("gw".into()),
    );
    assert_eq!(held(rows(&pool).await), reclaiming);

    let force = || ChannelHomeCommand::Force { channel: C };
    let fresh = run(&on, force(), &pg_db, &connected).await;
    assert!(fresh.is_err_and(|e| e.contains("within F")));
    assert_eq!(held(rows(&pool).await), reclaiming);
    sqlx::query("UPDATE o_channel_homes SET renewed_at = NOW() - INTERVAL '201 seconds'")
        .execute(&pool)
        .await
        .expect("silence the lease past F");
    let forced = run(&on, force(), &pg_db, &connected).await;
    let forced: Value = serde_json::from_str(&forced.expect("force")).expect("json");
    assert_eq!(
        (&forced["state"], &forced["holder"]),
        (&json!("orphaned"), &Value::Null)
    );
    let orphaned = (HomeState::Orphaned, None, Some("gw".into()));
    assert_eq!(held(rows(&pool).await), orphaned);
    pool.close().await;
    pg_db.drop().await;
}
