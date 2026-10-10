use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::o_piece_attempts::{
    AttemptResult, GrantOutcome, GrantRequest, Intent, attempts, grant, settle, settle_expired,
};
use super::o_piece_delivery::{
    AdmitOutcome, AdmittedBy, DeliveryRow, LedgerError, NewDelivery, PieceKey, Receipt,
    ReceiptMethod, ReceiptOutcome, admit, admitted_keys, load, record_receipt,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};

const CHANNEL: u64 = 6325;
const BOT: u64 = 900;

fn piece(native_key: &str) -> PieceKey {
    let unit = UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: native_key.into(),
        kind: UnitKind::Body,
    };
    PieceKey::new(unit, 0).unwrap()
}

fn delivery(key: &PieceKey, node: &str, serial: u64) -> NewDelivery {
    let payload = "hello";
    NewDelivery {
        key: key.clone(),
        payload: payload.into(),
        payload_sha256: hex::encode(Sha256::digest(payload)),
        identity_version: 1,
        split_version: 1,
        original_anchor: 40,
        sender_id: BOT,
        admitted_by: AdmittedBy::Uncertain,
        origin_serial: serial,
        origin_node: node.into(),
        original: AttemptResult::Uncertain,
        prior_retries: 0,
        failure: None,
    }
}

async fn admitted(pool: &PgPool, key: &PieceKey) -> DeliveryRow {
    match admit(pool, &delivery(key, "node-a", 5)).await.unwrap() {
        AdmitOutcome::Admitted(row) => row,
        other => panic!("first admission: {other:?}"),
    }
}

fn request<'a>(
    key: &'a PieceKey,
    revision: i64,
    owner: &'a str,
    ttl: Duration,
) -> GrantRequest<'a> {
    GrantRequest {
        key,
        expected_revision: revision,
        intent: Intent::AutoReconfirm,
        owner,
        run_id: owner,
        ttl,
    }
}

async fn revision(pool: &PgPool, key: &PieceKey) -> i64 {
    load(pool, key).await.unwrap().unwrap().revision
}

fn spent(rows: &[super::o_piece_attempts::AttemptRow]) -> Vec<(u8, Option<AttemptResult>)> {
    rows.iter().map(|row| (row.slot, row.result)).collect()
}

/// Waits until `count` sessions of this database block on a lock.
async fn lock_waiters(pool: &PgPool, count: i64) {
    let started = Instant::now();
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
              WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting == count {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{waiting} lock waiters"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn sql_state(error: sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(|db| db.code())
        .unwrap_or_default()
        .into_owned()
}

#[tokio::test]
async fn the_piece_tables_refuse_a_second_slot_row_a_fourth_slot_and_a_reattributed_message_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = piece("schema");
    admitted(&pool, &key).await;
    let slot = |slot: i16| {
        sqlx::query(
            "INSERT INTO o_piece_attempts (channel_id, provider, native_key, kind, piece_index,
                 slot, grant_id, intent, owner, run_id, result, settled_at)
             VALUES ('6325', 'claude', 'schema', 'body', 0, $1, gen_random_uuid(),
                 'auto_reconfirm', 'n', 'r', 'uncertain', NOW())",
        )
        .bind(slot)
    };
    let duplicate = sqlx::query(
        "INSERT INTO o_piece_attempts (channel_id, provider, native_key, kind, piece_index,
             slot, grant_id, intent, owner, run_id, result, settled_at)
         VALUES ('6325', 'claude', 'schema', 'body', 0, 0, gen_random_uuid(), 'original',
             'n', 'r', 'uncertain', NOW())",
    );
    assert_eq!(
        sql_state(duplicate.execute(&pool).await.unwrap_err()),
        "23505"
    );
    assert_eq!(
        sql_state(slot(3).execute(&pool).await.unwrap_err()),
        "23514"
    );
    slot(1).execute(&pool).await.unwrap();
    assert_eq!(
        sql_state(slot(1).execute(&pool).await.unwrap_err()),
        "23505"
    );
    let refund = sqlx::query("DELETE FROM o_piece_delivery WHERE native_key = 'schema'");
    assert_eq!(sql_state(refund.execute(&pool).await.unwrap_err()), "23503");
    let codex_tool_result = sqlx::query(
        "INSERT INTO o_piece_delivery (channel_id, provider, native_key, kind, piece_index,
             payload, payload_sha256, identity_version, split_version, original_anchor,
             sender_id, admitted_by, origin_serial, origin_node)
         SELECT channel_id, 'codex', native_key, 'tool_result', piece_index, payload,
             payload_sha256, identity_version, split_version, original_anchor, sender_id,
             admitted_by, origin_serial, origin_node FROM o_piece_delivery",
    );
    assert_eq!(
        sql_state(codex_tool_result.execute(&pool).await.unwrap_err()),
        "23514"
    );

    let other = piece("schema-other");
    admitted(&pool, &other).await;
    let receipt = |native_key: &'static str| {
        sqlx::query(
            "INSERT INTO o_piece_receipts (channel_id, message_id, provider, native_key, kind,
                 piece_index, author_id, method)
             VALUES ('6325', 77, 'claude', $1, 'body', 0, 900, 'marker')",
        )
        .bind(native_key)
    };
    receipt("schema").execute(&pool).await.unwrap();
    assert_eq!(
        sql_state(receipt("schema-other").execute(&pool).await.unwrap_err()),
        "23505"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn two_holders_granting_at_one_revision_consume_one_slot_and_a_lost_reply_mints_none_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = piece("t01");
    let row = admitted(&pool, &key).await;
    // Both grants read the piece before either can write a slot.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE o_piece_attempts IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let racers = ["node-a", "node-b"].map(|owner| {
        let (pool, key) = (pool.clone(), key.clone());
        tokio::spawn(async move {
            grant(
                &pool,
                request(&key, row.revision, owner, Duration::from_secs(60)),
            )
            .await
        })
    });
    lock_waiters(&pool, 2).await;
    blocker.rollback().await.unwrap();
    let mut outcomes = Vec::new();
    for racer in racers {
        outcomes.push(racer.await.unwrap());
    }
    let granted = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Ok(GrantOutcome::Granted(_))))
        .count();
    assert_eq!(granted, 1, "{outcomes:?}");

    // The winner's reply is lost: asking again with the same evidence consumes nothing more.
    drop(outcomes);
    let again = grant(
        &pool,
        request(&key, row.revision, "node-a", Duration::from_secs(60)),
    );
    assert!(matches!(again.await.unwrap(), GrantOutcome::Stale { .. }));
    let rows = attempts(&pool, &key).await.unwrap();
    assert_eq!(
        spent(&rows),
        [(0, Some(AttemptResult::Uncertain)), (1, None)]
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_slot_spent_before_its_post_is_never_refunded_and_slot_two_is_the_last_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = piece("t02");
    let row = admitted(&pool, &key).await;
    let hour = Duration::from_secs(3600);
    let GrantOutcome::Granted(first) = grant(&pool, request(&key, row.revision, "a", hour))
        .await
        .unwrap()
    else {
        panic!("slot 1 grant");
    };
    assert_eq!(first.slot(), 1);
    // Holder `a` crashes before its POST; another holder may not take a slot while it is open.
    let current = revision(&pool, &key).await;
    let open = grant(&pool, request(&key, current, "b", hour))
        .await
        .unwrap();
    assert_eq!(open, GrantOutcome::Open { slot: 1 });
    assert!(settle_expired(&pool, &key).await.unwrap().is_empty());
    let expire = "UPDATE o_piece_attempts SET deadline = NOW() - INTERVAL '1 millisecond'
                   WHERE native_key = $1 AND result IS NULL";
    sqlx::query(expire)
        .bind("t02")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(settle_expired(&pool, &key).await.unwrap(), [1]);
    // The late holder cannot reopen or overwrite the settled slot.
    let late = settle(&pool, &first, AttemptResult::NotSent).await.unwrap();
    assert_eq!(late, None);

    let current = revision(&pool, &key).await;
    let GrantOutcome::Granted(second) = grant(&pool, request(&key, current, "b", hour))
        .await
        .unwrap()
    else {
        panic!("slot 2 grant");
    };
    assert_eq!(second.slot(), 2);
    sqlx::query(expire)
        .bind("t02")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(settle_expired(&pool, &key).await.unwrap(), [2]);
    let current = revision(&pool, &key).await;
    let capped = grant(&pool, request(&key, current, "c", hour))
        .await
        .unwrap();
    assert_eq!(capped, GrantOutcome::CapReached);
    let rows = attempts(&pool, &key).await.unwrap();
    let abandoned = Some(AttemptResult::Abandoned);
    assert_eq!(
        spent(&rows),
        [
            (0, Some(AttemptResult::Uncertain)),
            (1, abandoned),
            (2, abandoned)
        ]
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_new_holder_or_serial_reaches_the_same_row_and_a_failed_read_is_no_empty_row_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = piece("t10");
    // Each admission writes its row before its slot 0; holding the slot table overlaps the two.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE o_piece_attempts IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let racers = [("node-a", 5), ("node-b", 91)].map(|(node, serial)| {
        let (pool, new) = (pool.clone(), delivery(&key, node, serial));
        tokio::spawn(async move { admit(&pool, &new).await })
    });
    lock_waiters(&pool, 2).await;
    blocker.rollback().await.unwrap();
    let mut rows = Vec::new();
    for racer in racers {
        rows.push(racer.await.unwrap().unwrap());
    }
    let first = match &rows[..] {
        [AdmitOutcome::Admitted(row), AdmitOutcome::Existing(other)]
        | [AdmitOutcome::Existing(other), AdmitOutcome::Admitted(row)]
            if row == other =>
        {
            row.clone()
        }
        _ => panic!("one admission per piece: {rows:?}"),
    };
    let later = admit(&pool, &delivery(&key, "node-c", 140)).await.unwrap();
    assert_eq!(later, AdmitOutcome::Existing(first.clone()));
    assert_eq!(
        spent(&attempts(&pool, &key).await.unwrap()),
        [(0, Some(AttemptResult::Uncertain))]
    );
    let keys = admitted_keys(&pool, CHANNEL).await.unwrap();
    assert_eq!(keys, std::slice::from_ref(&key));
    assert!(admitted_keys(&pool, CHANNEL + 1).await.unwrap().is_empty());

    pool.close().await;
    assert!(matches!(load(&pool, &key).await, Err(LedgerError::Pg(_))));
    assert!(matches!(
        admitted_keys(&pool, CHANNEL).await,
        Err(LedgerError::Pg(_))
    ));
    let request = request(&key, first.revision, "node-b", Duration::from_secs(60));
    assert!(matches!(
        grant(&pool, request).await,
        Err(LedgerError::Pg(_))
    ));
    db.drop().await;
}

#[tokio::test]
async fn a_receipt_resolves_its_piece_once_and_never_moves_to_another_piece_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (key, other) = (piece("receipt"), piece("receipt-other"));
    let row = admitted(&pool, &key).await;
    admitted(&pool, &other).await;
    let receipt = |key: &PieceKey, author_id| Receipt {
        key: key.clone(),
        message_id: 88,
        author_id,
        slot: Some(1),
        method: ReceiptMethod::PostResponse,
    };
    let wrong = record_receipt(&pool, &receipt(&key, BOT + 1))
        .await
        .unwrap();
    assert_eq!(wrong, ReceiptOutcome::WrongAuthor);
    let stranger = record_receipt(&pool, &receipt(&piece("never"), BOT))
        .await
        .unwrap();
    assert_eq!(stranger, ReceiptOutcome::NotAdmitted);
    let recorded = record_receipt(&pool, &receipt(&key, BOT)).await.unwrap();
    assert_eq!(
        recorded,
        ReceiptOutcome::Recorded {
            revision: row.revision + 1
        }
    );
    assert_eq!(
        record_receipt(&pool, &receipt(&key, BOT)).await.unwrap(),
        ReceiptOutcome::Known
    );
    let moved = record_receipt(&pool, &receipt(&other, BOT)).await.unwrap();
    assert_eq!(moved, ReceiptOutcome::AttributedElsewhere(key.clone()));
    let current = revision(&pool, &key).await;
    let resolved = grant(&pool, request(&key, current, "a", Duration::from_secs(60))).await;
    assert_eq!(resolved.unwrap(), GrantOutcome::Resolved);
    pool.close().await;
    db.drop().await;
}
