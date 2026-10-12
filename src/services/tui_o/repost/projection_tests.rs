//! Shared receipts read before a channel's actor starts: a receipted rejection neither stops the
//! writer nor sends its approval again, and the local ledger and PostgreSQL rows stay as they were.

use sqlx::PgPool;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::repost::config::{RepostConfig, RepostSwitch};
use crate::services::tui_o::repost::identity::{
    IDENTITY_VERSION, PieceKey, SPLIT_VERSION, payload_sha256,
};
use crate::services::tui_o::repost::io::RepostHttp;
use crate::services::tui_o::repost::io::probe::evidence::RunScope;
use crate::services::tui_o::repost::o_piece_attempts::AttemptResult;
use crate::services::tui_o::repost::o_piece_delivery::{
    self, AdmitOutcome, AdmittedBy, NewDelivery, Receipt, ReceiptMethod, ReceiptOutcome,
};
use crate::services::tui_o::repost::projection::ReceiptOverlay;
use crate::services::tui_o::repost::runner::{ChannelTurn, Runner};
use crate::services::tui_o::shadow::tap::TuiOConfig;

const PAYLOAD: &str = "receipted payload";
const BLOCKED: WriterAlarm = WriterAlarm::Blocked { status: 403 };

/// How another home left a piece in PostgreSQL.
#[derive(Clone, Copy)]
struct Shared {
    payload: &'static str,
    identity_version: i32,
    receipt: bool,
}

const RECEIPTED: Shared = Shared {
    payload: PAYLOAD,
    identity_version: IDENTITY_VERSION,
    receipt: true,
};

/// Another home's admission of `native` and, when asked, its recorded receipt.
async fn shared(pool: &PgPool, native: &str, shared: Shared, message_id: u64) -> PieceKey {
    let key = PieceKey::new(unit(native), 0).unwrap();
    let new = NewDelivery {
        key: key.clone(),
        payload: shared.payload.into(),
        payload_sha256: payload_sha256(shared.payload),
        identity_version: shared.identity_version,
        split_version: SPLIT_VERSION,
        original_anchor: 40,
        sender_id: BOT,
        admitted_by: AdmittedBy::Uncertain,
        origin_serial: 7,
        origin_node: "node-b".into(),
        original: AttemptResult::Uncertain,
        prior_retries: 0,
        failure: None,
    };
    let admitted = o_piece_delivery::admit(pool, &new).await.unwrap();
    assert!(matches!(admitted, AdmitOutcome::Admitted(_)));
    if shared.receipt {
        let receipt = Receipt {
            key: key.clone(),
            message_id,
            author_id: BOT,
            slot: Some(1),
            method: ReceiptMethod::Marker,
        };
        let recorded = o_piece_delivery::record_receipt(pool, &receipt).await;
        assert!(matches!(recorded.unwrap(), ReceiptOutcome::Recorded { .. }));
    }
    key
}

/// `native` prepared at `serial` with [`PAYLOAD`] and refused 403, optionally approved again.
fn seed_rejection(harness: &Harness, native: &str, serial: u64, approve: bool) {
    let mut channel = harness.channel();
    let anchor_id = channel.ledger().anchor();
    let prepared = LedgerEntry::Prepared {
        serial,
        unit_key: unit(native),
        piece_index: 0,
        payload: PAYLOAD.into(),
        anchor_id,
        epoch: 1,
    };
    channel.append_ledger(prepared).unwrap();
    let refused = LedgerEntry::Rejected {
        serial,
        status: 403,
    };
    channel.append_ledger(refused).unwrap();
    drop(channel);
    if approve {
        let resume = harness
            .store
            .record_operator_resume(CHANNEL, serial, "operator", "r");
        resume.unwrap();
    }
}

fn repost_config(enabled: bool, listed: &[u64]) -> RepostConfig {
    RepostConfig {
        enabled,
        channels: listed.iter().copied().collect(),
    }
}

/// The production re-post turn over `pool`; it never opens a transport.
fn receipt_turn(root: &Path, pool: PgPool, repost: RepostConfig) -> ChannelTurn<RepostHttp> {
    let boot = TuiOConfig {
        repost,
        ..TuiOConfig::default()
    };
    let switch = RepostSwitch::new(Some(&boot), watch::channel(None).1);
    let run = RunScope {
        holder: "node-a".into(),
        run: "d4d".into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    };
    let sidecar = root.join("repost").join(format!("{CHANNEL}.jsonl"));
    let runner = Runner::new(switch, pool, BOT, run);
    ChannelTurn::new(runner, Box::new(|| None), CHANNEL, sidecar)
}

fn unreachable_pool() -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(200))
        .connect_lazy("postgres://agentdesk@127.0.0.1:1/none")
        .unwrap()
}

/// Polls in real time until `done` or about ten seconds.
async fn until(mut done: impl FnMut() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// What a recovered writer came to: its posts, the stop its constructor decided, the next local
/// serial and whether the approval at serial 0 was consumed.
type Started = (Vec<String>, Option<WriterAlarm>, u64, Option<Option<u64>>);

/// Starts the actor over a writer built with the receipts its production turn read, appends one
/// row and reports what that start came to.
async fn start_writer(
    harness: &Harness,
    path: &Path,
    pool: PgPool,
    repost: RepostConfig,
) -> Started {
    let mut hook = receipt_turn(harness._runtime.path(), pool, repost);
    let receipts = <ChannelTurn<RepostHttp> as RepostHook<Writer>>::receipts(&mut hook).await;
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let lease = Arc::clone(&harness.lease);
    let alarms = harness.alarms.clone();
    let writer =
        ChannelWriter::with_receipts(harness.channel(), gate, port, lease, alarms, receipts)
            .with_repost(Some(Box::new(hook)));
    let initially = writer.stop_cause().map(|cause| cause.alarm.clone());
    let (stop, task) = spawn(writer);
    append(path, &row("m1", "after"));
    until(|| {
        task.is_finished()
            || harness
                .port
                .posts()
                .last()
                .is_some_and(|last| last == "after")
    })
    .await;
    let _ = stop.send(true);
    task.await.unwrap();
    let channel = harness.channel();
    let ledger = channel.ledger();
    let approval = ledger.approval(0).map(|approval| approval.consumed_serial);
    (
        harness.port.posts(),
        initially,
        ledger.next_serial(),
        approval,
    )
}

/// A receipted 403 starts the writer with no POST of it; no receipt, a failed read, another payload
/// or identity version keep the stop; off, the approval goes once. The ledger is only appended to.
#[tokio::test]
async fn d4d_receipt_overlay_judges_the_recovered_writer_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let other_payload = Shared {
        payload: "another payload",
        ..RECEIPTED
    };
    let other_version = Shared {
        identity_version: IDENTITY_VERSION + 1,
        ..RECEIPTED
    };
    let none = Shared {
        receipt: false,
        ..RECEIPTED
    };
    let rows = [
        ("w-approved", Some(RECEIPTED), true),
        ("w-blocked", Some(RECEIPTED), false),
        ("w-missing", Some(none), false),
        ("w-down", None, false),
        ("w-payload", Some(other_payload), false),
        ("w-version", Some(other_version), false),
        ("w-off", Some(RECEIPTED), true),
    ];
    let mut observed = Vec::new();
    for (id, (native, stored, approve)) in (6_000..).zip(rows) {
        if let Some(stored) = stored {
            shared(&pool, native, stored, id).await;
        }
        let (harness, path, _) = switched_over(b"");
        seed_rejection(&harness, native, 0, approve);
        let before = std::fs::read(ledger_path(&harness)).unwrap();
        harness.gate.acquired();
        let reachable = if stored.is_some() {
            pool.clone()
        } else {
            unreachable_pool()
        };
        let repost = repost_config(native != "w-off", &[CHANNEL]);
        let started = start_writer(&harness, &path, reachable, repost).await;
        let after = std::fs::read(ledger_path(&harness)).unwrap();
        assert!(
            after.starts_with(&before),
            "{native}: the ledger is only appended to"
        );
        observed.push((native, started));
    }
    let blocked = |approval| (vec![], Some(BLOCKED), 1, approval);
    let after = vec!["after".to_owned()];
    let sent = vec![PAYLOAD.to_owned(), "after".to_owned()];
    let expected = [
        ("w-approved", (after.clone(), None, 2, Some(None))),
        ("w-blocked", (after, None, 2, None)),
        ("w-missing", blocked(None)),
        ("w-down", blocked(None)),
        ("w-payload", blocked(None)),
        ("w-version", blocked(None)),
        ("w-off", (sent, None, 3, Some(Some(1)))),
    ];
    assert_eq!(observed, expected);
    pool.close().await;
    db.drop().await;
}

/// Off, with an empty allowlist or for an unlisted channel, the production turn answers
/// `Disabled` before PostgreSQL; the same unreachable pool reads as `Unknown` once listed.
#[tokio::test]
async fn d4d_receipt_overlay_reads_nothing_off_or_unlisted() {
    let configs = [
        (repost_config(false, &[CHANNEL]), "disabled"),
        (repost_config(true, &[]), "disabled"),
        (repost_config(true, &[OTHER]), "disabled"),
        (repost_config(true, &[CHANNEL]), "unknown"),
    ];
    let root = tempfile::tempdir().unwrap();
    for (repost, expected) in configs {
        let mut hook = receipt_turn(root.path(), unreachable_pool(), repost);
        let read = <ChannelTurn<RepostHttp> as RepostHook<Writer>>::receipts(&mut hook).await;
        let read = match read {
            ReceiptOverlay::Disabled => "disabled",
            ReceiptOverlay::Known { .. } => "known",
            ReceiptOverlay::Unknown(_) => "unknown",
        };
        assert_eq!(read, expected);
    }
    assert!(!root.path().join("repost").exists(), "no sidecar either");
}
