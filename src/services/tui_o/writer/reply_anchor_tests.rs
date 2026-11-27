use super::*;
use crate::services::tui_o::writer::deliver::Step;
use crate::services::tui_o::writer::input_facts::native::Projection;
use crate::services::tui_o::writer::input_facts::native::tests::{Fixture, close, queue, user};
use crate::services::tui_o::writer::pieces::{Derived, PieceWork};
use crate::services::tui_o::writer::tests::{Harness, unit};
use serde_json::{Value, json};
use sha2::Digest;

const A: u64 = 9001;
const B: u64 = 9002;

fn absorbed(body: &str) -> Value {
    let mut row = queue("remove", body);
    row["reason"] = json!("absorbed_mid_turn");
    row
}

fn command(id: &str, body: &str) -> Value {
    json!({"type":"attachment","uuid":id,"attachment":{"type":"queued_command","prompt":body,"commandMode":"prompt","origin":{"kind":"human"}}})
}

fn answer(id: &str) -> Value {
    json!({"type":"assistant","uuid":id,"apiBlockIndex":0,"message":{"id":id,"content":[{"type":"text","text":id}]}})
}

/// Appends rows and returns the byte range the last one occupies.
fn append(f: &Fixture, rows: &[Value]) -> std::ops::Range<u64> {
    f.append(&rows[..rows.len() - 1]);
    let start = std::fs::metadata(&f.binding.source.path).unwrap().len();
    f.append(&rows[rows.len() - 1..]);
    start..std::fs::metadata(&f.binding.source.path).unwrap().len()
}

/// The registrations as a restart reads them, each ticket naming its own Discord message.
fn registrations(
    f: &Fixture,
    anchors: &[(&str, u64)],
) -> Vec<super::super::input_facts::native::Registration> {
    let mut registrations = f.history.native_evidence_registrations().unwrap();
    for registration in &mut registrations {
        let nonce = registration.ticket.key["attempt"]["nonce"]
            .as_str()
            .unwrap()
            .to_owned();
        let id = anchors
            .iter()
            .find(|(n, _)| *n == nonce)
            .map(|(_, id)| *id)
            .unwrap();
        registration.ticket.key["identity"]["primary"] = json!(id);
    }
    registrations
}

fn settle(
    f: &Fixture,
    anchors: &Anchors,
    registrations: &[super::super::input_facts::native::Registration],
) {
    let bytes = std::fs::read(&f.binding.source.path).unwrap();
    let hash = hex::encode(sha2::Sha256::digest(&bytes));
    let (_, projection) = Projection::reopen(f.binding.clone(), bytes.len() as u64, &hash).unwrap();
    let projection = projection.unwrap();
    // As `offer_native`: every receipt is recorded, one per pass, before the runs settle.
    let mut registrations = registrations.to_vec();
    while let Some((namespace, ticket, row)) =
        projection.receipts(&registrations).first().map(|r| {
            let (namespace, ticket) = r.identity();
            (
                namespace.to_owned(),
                ticket.clone(),
                r.record().1.to_owned(),
            )
        })
    {
        let owner = registrations.iter_mut();
        let mut owner = owner.filter(|r| r.namespace == namespace && r.ticket == ticket);
        owner.next().unwrap().receipts.push(row);
    }
    let runs = projection.memberships(&registrations).unwrap();
    anchors.settle(
        &f.binding.source,
        (bytes.len() as u64, String::new()),
        || Ok(runs),
    );
}

fn piece(f: &Fixture, key: &str, range: std::ops::Range<u64>) -> Derived {
    Derived::Piece(PieceWork {
        output_origin: None,
        unit_key: unit(key),
        index: 0,
        payload: key.into(),
        notice_origin: None,
        reply: Some(ReplyRef::At(Coord {
            source: f.binding.source.clone(),
            range,
        })),
    })
}

/// A Discord input absorbed into a TUI-opened turn, with or without its queued command, gets the
/// merged answer as a reply, one joined mark, and no answer of its own.
#[tokio::test]
async fn joined_input_gets_the_merged_answer_in_every_absorption_order() {
    for order in 0..3 {
        let f = Fixture::new();
        f.register("b", "ours");
        f.append(&[
            user("external", "direct TUI input"),
            queue("enqueue", "ours"),
        ]);
        match order {
            0 => f.append(&[absorbed("ours"), command("command", "ours")]),
            1 => f.append(&[command("command", "ours"), absorbed("ours")]),
            _ => f.append(&[absorbed("ours")]),
        }
        let first = append(&f, &[answer("one")]);
        let last = append(&f, &[answer("two"), close()]);
        let harness = Harness::new();
        harness.gate.acquired();
        let (mut writer, anchors) = (harness.writer(), Anchors::default());
        writer.attach_reply_anchors(anchors.clone());
        settle(&f, &anchors, &registrations(&f, &[("b", B)]));
        assert_eq!(writer.deliver(&piece(&f, "first", first)).await, Step::Done);
        assert_eq!(writer.deliver(&piece(&f, "last", last)).await, Step::Done);
        assert_eq!(harness.port.posts(), ["first", "last"], "order={order}");
        assert_eq!(
            *harness.port.reply_to.lock().unwrap(),
            [Some(B), Some(B)],
            "order={order}"
        );
        assert_eq!(*harness.port.joined.lock().unwrap(), [B], "order={order}");
    }
}

/// A row equal to the frame only after trimming, or with an image beside its text, has no exact
/// receipt, so its answer posts without a reference and nothing is marked.
#[tokio::test]
async fn class_hash_match_without_exact_receipt_binds_nothing() {
    let image =
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}});
    let mixed = json!({"type":"user","uuid":"u","sessionId":"parent",
        "message":{"content":[{"type":"text","text":"ours"}, image]}});
    for (case, row) in [user("u", " ours "), mixed].into_iter().enumerate() {
        let f = Fixture::new();
        f.register("b", "ours");
        let at = append(&f, &[row, answer("one")]);
        let harness = Harness::new();
        harness.gate.acquired();
        let (mut writer, anchors) = (harness.writer(), Anchors::default());
        writer.attach_reply_anchors(anchors.clone());
        settle(&f, &anchors, &registrations(&f, &[("b", B)]));
        assert_eq!(writer.deliver(&piece(&f, "one", at)).await, Step::Done);
        assert_eq!(
            *harness.port.reply_to.lock().unwrap(),
            [None],
            "case={case}"
        );
        assert!(
            harness.port.joined.lock().unwrap().is_empty(),
            "case={case}"
        );
    }
}

/// A dequeued input is not absorbed: an unrelated prompt opening the next turn binds nothing,
/// while the input's own receipted prompt opens its turn as the reply without a joined mark.
#[tokio::test]
async fn dequeued_input_binds_only_its_own_receipted_prompt() {
    for (case, (prompt, reply)) in [
        (user("foreign", "unrelated input"), None),
        (user("u-b", "ours"), Some(B)),
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::new();
        f.register("b", "ours");
        f.append(&[queue("enqueue", "ours"), queue("dequeue", "")]);
        let at = append(&f, &[prompt, answer("one")]);
        let harness = Harness::new();
        harness.gate.acquired();
        let (mut writer, anchors) = (harness.writer(), Anchors::default());
        writer.attach_reply_anchors(anchors.clone());
        settle(&f, &anchors, &registrations(&f, &[("b", B)]));
        assert_eq!(writer.deliver(&piece(&f, "one", at)).await, Step::Done);
        assert_eq!(
            *harness.port.reply_to.lock().unwrap(),
            [reply],
            "case={case}"
        );
        assert!(
            harness.port.joined.lock().unwrap().is_empty(),
            "case={case}"
        );
    }
}

/// Uniqueness failures and an ordinary withdrawal bind no input, so the answer has no reference.
#[test]
fn ambiguous_or_withdrawn_inputs_bind_nothing() {
    let cases: [(&[&str], &[Value]); 3] = [
        (&["b", "c"], &[queue("enqueue", "ours"), absorbed("ours")]),
        (
            &["b"],
            &[
                queue("enqueue", "ours"),
                queue("enqueue", "ours"),
                absorbed("ours"),
            ],
        ),
        (&["b"], &[queue("enqueue", "ours"), queue("remove", "ours")]),
    ];
    for (case, (nonces, rows)) in cases.iter().enumerate() {
        let f = Fixture::new();
        for nonce in *nonces {
            f.register(nonce, "ours");
        }
        f.append(&[user("external", "direct TUI input")]);
        f.append(rows);
        let at = append(&f, &[answer("one")]);
        let anchors = Anchors::default();
        settle(&f, &anchors, &registrations(&f, &[("b", B), ("c", A)]));
        let coord = Coord {
            source: f.binding.source.clone(),
            range: at,
        };
        assert_eq!(anchors.resolve(&coord), Reply::Unbound, "case={case}");
    }
}

/// An untagged idle opener and a tagged input absorbed into its turn: the answer replies to the
/// opener even when it is derived before analysis settles, and only the joined input is marked.
#[tokio::test]
async fn untagged_opener_keeps_the_reply_when_its_answer_arrives_first() {
    let f = Fixture::new();
    f.register("a", "hello A");
    f.register("b", "ours");
    let at = append(
        &f,
        &[
            user("ua", "hello A"),
            queue("enqueue", "ours"),
            absorbed("ours"),
            answer("one"),
        ],
    );
    let harness = Harness::new();
    harness.gate.acquired();
    let (mut writer, anchors) = (harness.writer(), Anchors::default());
    writer.attach_reply_anchors(anchors.clone());
    anchors.track(&f.binding.source);
    assert_eq!(
        writer.deliver(&piece(&f, "answer", at.clone())).await,
        Step::LeaseBusy
    );
    assert!(harness.port.posts().is_empty());
    settle(&f, &anchors, &registrations(&f, &[("a", A), ("b", B)]));
    assert_eq!(writer.deliver(&piece(&f, "answer", at)).await, Step::Done);
    assert_eq!(*harness.port.reply_to.lock().unwrap(), [Some(A)]);
    assert_eq!(*harness.port.joined.lock().unwrap(), [B]);
    assert_eq!(writer.store().ledger().reply_to(0), Some(A));
}

/// After a restart the anchor comes back: from a persisted receipt when no binding was recorded,
/// and from the recorded binding even when the registrations no longer name it.
#[tokio::test]
async fn restart_recovers_the_same_anchor_before_and_after_its_record() {
    let f = Fixture::new();
    f.register("a1", "same");
    f.register("a2", "same");
    let at = append(&f, &[user("u1", "same"), answer("one")]);
    let mut persisted = registrations(&f, &[("a1", A), ("a2", B)]);
    persisted[0].receipts.push("u1".into());
    let harness = Harness::new();
    harness.gate.acquired();
    let (mut writer, anchors) = (harness.writer(), Anchors::default());
    writer.attach_reply_anchors(anchors.clone());
    settle(&f, &anchors, &persisted);
    assert_eq!(
        writer.deliver(&piece(&f, "receipted", at.clone())).await,
        Step::Done
    );
    assert_eq!(*harness.port.reply_to.lock().unwrap(), [Some(A)]);
    drop(writer);
    let (mut restarted, anchors) = (harness.writer(), Anchors::default());
    restarted.attach_reply_anchors(anchors.clone());
    settle(&f, &anchors, &[]);
    assert_eq!(
        restarted.deliver(&piece(&f, "recorded", at)).await,
        Step::Done
    );
    assert_eq!(*harness.port.reply_to.lock().unwrap(), [Some(A), Some(A)]);
}
