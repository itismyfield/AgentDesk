//! A handoff whose old pane is not confirmed ended outlives the inputs it could not deliver: each
//! front ends on its own finite budget, and nothing starts before that pane ended.
use super::left_herdr_tests::{Rig, queued, rig};
use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::queue_io::input_owner::drive;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const KEY: &str = "claude/t/h:left";
const RETAINED: &str = "NotSent(\"owner_start_failed\")";

fn pin() -> Value {
    json!({"nonce":"n","server":1,"session":"$1","window":"@1","pane":"%1","pid":1,"birth":"b"})
}
/// A scheduled snapshot start as its seam keeps it: a session of its own, named by its label.
fn snapshot() -> Value {
    let metadata = json!({"agent_id":"agent8146","scheduled_message_id":"sm-8146",
        "scheduled_snapshot_session_label":"snap-8146"});
    let mut kept = metadata.clone();
    kept["metadata"] = metadata;
    (kept["synthetic_label"], kept["source"]) = (json!("snap-8146"), json!("scheduled_message"));
    kept
}
fn routine() -> Value {
    json!({"origin":"routine","source":"routine","is_dm":false,"metadata":{"routine_id":"r8146",
        "run_id":"run","agent_id":"agent8146","execution_strategy":"persistent","is_dm":false}})
}
fn waiting(rig: &Rig, journal: &Value) -> bool {
    let key = (rig.channel.get(), journal["id"].as_str().unwrap().to_string());
    WAITING.lock().unwrap().contains(&key)
}
/// Pins the journal's old pane, as a first close attempt does.
async fn pinned(rig: &Rig, pin: Value) -> Value {
    let pinned = rig.mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().unwrap();
        let (front, journal) = (owner.front().unwrap(), owner.handoff().unwrap());
        owner.handoff_pin(&journal.id, &front, pin)
    });
    assert!(pinned.await.unwrap());
    rig.state().await["unfinished_handoff"].clone()
}
/// Four counted holds of `front`, then the look that retains it with its notice owed.
async fn spend_budget(rig: &Rig, front: u64) {
    for failures in 1..=4u64 {
        assert_eq!(rig.hand().await.unwrap_err(), HELD);
        let state = rig.state().await;
        assert_eq!(state["unfinished_handoff"]["failures"], failures);
        assert_eq!(state["items"][0]["message_id"], front);
        rig.expire().await;
    }
    assert_eq!(rig.hand().await.unwrap_err(), RETAINED);
    let state = rig.state().await;
    let retained = state["retained_inputs"].as_array().unwrap().last().unwrap();
    assert_eq!(retained["input"]["original"]["message_id"], front);
    assert_eq!(retained["notice"]["kind"], "retained_retry_required");
}
/// The journal as custody keeps it: the same id and pin, unended, with no budget left running.
fn assert_custody(state: &Value, journal: &Value) {
    let kept = &state["unfinished_handoff"];
    assert_eq!(kept["id"], journal["id"]);
    assert_eq!(kept["pin"], journal["pin"]);
    assert_eq!(kept["ended"], false);
    let budget = (kept["failures"].as_u64(), kept["retry_after_ms"].as_u64());
    assert_eq!(budget, (Some(0), Some(0)), "{kept}");
    assert!(kept.get("spender").is_none(), "{kept}");
}
/// The first input retained on a refusing host, leaving its unended journal as custody.
async fn custody(channel: u64, first: u64) -> (Rig, Value) {
    let rig = rig(channel, &[first], false).await;
    let journal = pinned(&rig, pin()).await;
    spend_budget(&rig, first).await;
    assert_custody(&rig.state().await, &journal);
    (rig, journal)
}
async fn cancel(rig: &Rig, message: u64) {
    use crate::services::turn_orchestrator::QueuedCancelTarget::SingleSelection;
    let token = &rig.shared.token_hash;
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, token, None);
    let target = SingleSelection(MessageId::new(message));
    let cancelled = rig.mailbox.cancel_queued_primary_message(target, persistence);
    assert!(cancelled.await.removed.is_some());
}

#[tokio::test]
async fn refused_unended_handoff_retains_each_front_and_keeps_custody() {
    let (first, second) = (8_000_000_008_146_601, 8_000_000_008_146_602);
    let _host = refuse_host(8_146_106);
    let (rig, journal) = custody(8_146_106, first).await;
    rig.enqueue(second).await;
    spend_budget(&rig, second).await;
    let state = rig.state().await;
    assert_custody(&state, &journal);
    assert_eq!(state["retained_inputs"].as_array().unwrap().len(), 2);
    assert!(state["items"].as_array().unwrap().is_empty());
    assert!(rig.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn custody_survives_a_reboot_and_ends_the_old_pane_before_the_next_front_starts() {
    let (first, second) = (8_000_000_008_146_701, 8_000_000_008_146_702);
    let host = refuse_host(8_146_107);
    let (rig, journal) = custody(8_146_107, first).await;
    rig.reboot().await;
    assert_custody(&rig.state().await, &journal);
    recover(rig.shared.clone(), rig.channel);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiting(&rig, &journal), "an empty custody arms no timer");
    drop(host);
    let _host = herdr_host(8_146_107, KEY, false);
    rig.enqueue(second).await;
    assert_eq!(rig.hand().await, Ok(()));
    assert_eq!(*rig.started.lock().unwrap(), [second]);
    let state = rig.state().await;
    assert_eq!(state["unfinished_handoff"]["id"], journal["id"]);
    assert_eq!(state["unfinished_handoff"]["ended"], true);
    assert_eq!(state["retained_inputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn refused_handoff_stops_retrying_by_its_real_timer() {
    let first = 8_000_000_008_146_801;
    let _host = refuse_host(8_146_108);
    let rig = rig(8_146_108, &[first], false).await;
    let journal = rig.state().await["unfinished_handoff"].clone();
    let wakes = Arc::new(AtomicUsize::new(0));
    let (woken, driven) = (wakes.clone(), rig.drive.clone());
    // The owner's wake runs the real drive, as the runtime's wake does.
    let wake = Arc::new(move || {
        woken.fetch_add(1, Ordering::SeqCst);
        drive::wake(driven.clone());
    });
    set_owner_wake_for_tests(rig.channel, Some(wake));
    let failures = |state: &Value| state["unfinished_handoff"]["failures"].as_u64();
    let retained = |state: &Value| state["retained_inputs"].as_array().unwrap().len();
    drive::wake(rig.drive.clone());
    // The first hold's own two-second timer brings the second look.
    let mut state = rig.state().await;
    for _ in 0..80 {
        if failures(&state) == Some(2) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        state = rig.state().await;
    }
    assert_eq!(failures(&state), Some(2), "the timer alone looked again");
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    // Each later look is let through its deadline; the fifth ends the retries.
    for look in 3..=5u64 {
        rig.expire().await;
        drive::wake(rig.drive.clone());
        for _ in 0..200 {
            state = rig.state().await;
            if retained(&state) == 1 || failures(&state) == Some(look) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    assert_eq!(retained(&state), 1, "the budget ends the retries");
    assert_custody(&state, &journal);
    // The timer armed before the budget ended wakes once more, and nothing arms another.
    for _ in 0..120 {
        if !waiting(&rig, &journal) && wakes.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!waiting(&rig, &journal));
    assert_eq!(wakes.load(Ordering::SeqCst), 2, "an empty custody is quiet");
    assert_custody(&rig.state().await, &journal);
    assert!(rig.started.lock().unwrap().is_empty());
}

/// The owner's reserve as the launch hook makes it for a turn running under `logical`.
async fn reserve(rig: &Rig, logical: &str) -> Result<(), String> {
    let granted = rig.mailbox.v2_request(|slot| {
        let owner = slot.as_mut().unwrap();
        let front = owner.front().ok_or("no front")?;
        owner.grant_front(&front).map(|()| front)
    });
    let front = granted.await?;
    let claim = drive::claim_turn(&rig.mailbox, front.1.message_id, &front).await?;
    let claim = claim.ok_or("not granted")?;
    let attempt = json!({"nonce":"n-8146","provider":"claude","target":{"host":"herdr",
        "owner":logical,"root":"/test","execution_nonce":"e"},"source":{"kind":"pre_source",
        "expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let payload = json!({"frame":"f","digest":"d","attribution":{}});
    let parse = |e: serde_json::Error| e.to_string();
    let reserved = rig.mailbox.v2_request(move |slot| {
        let owner = slot.as_mut().unwrap();
        let attempt = serde_json::from_value(attempt).map_err(parse)?;
        let payload = serde_json::from_value(payload).map_err(parse)?;
        let at = (claim.front(), claim.number());
        owner.reserve_claimed(at, attempt, payload).map(drop)
    });
    reserved.await
}

#[tokio::test]
async fn scheduled_front_with_its_own_session_is_delivered_once_after_the_old_pane_ends() {
    let (first, scheduled) = (8_000_000_008_146_901, 8_000_000_008_146_902);
    let host = refuse_host(8_146_109);
    let (rig, _) = custody(8_146_109, first).await;
    drop(host);
    let _host = herdr_host(8_146_109, KEY, false);
    rig.admit_kept(scheduled, snapshot()).await;
    assert_eq!(rig.hand().await, Ok(()));
    assert_eq!(*rig.started.lock().unwrap(), [scheduled]);
    let state = rig.state().await;
    assert!(state.get("unfinished_handoff").is_none(), "{state}");
    // The start keeps the snapshot's own session, and the owner reserves it there.
    let build = crate::services::discord::adk_session::build_adk_session_key;
    let key = build(&rig.shared, rig.channel, &ProviderKind::Claude, Some("snap-8146")).await;
    let own = crate::services::discord::session_identity::SessionIdentity::parse(&key.unwrap());
    let own = own.unwrap().tmux_name;
    assert_ne!(own, "left");
    assert_eq!(reserve(&rig, &own).await, Ok(()));
    let state = rig.state().await;
    assert_eq!(
        state["head_injection"]["original"]["message_id"],
        scheduled
    );
    assert_eq!(state["retained_inputs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn scheduled_front_under_a_refusing_host_is_retained_without_starting() {
    let (first, scheduled) = (8_000_000_008_147_001, 8_000_000_008_147_002);
    let _host = refuse_host(8_146_110);
    let (rig, journal) = custody(8_146_110, first).await;
    rig.admit_kept(scheduled, snapshot()).await;
    spend_budget(&rig, scheduled).await;
    assert_custody(&rig.state().await, &journal);
    assert!(rig.started.lock().unwrap().is_empty());
    // With no journal the same start is not judged at all.
    let _free = refuse_host(8_146_111);
    let free = queued(8_146_111).await;
    free.admit_kept(scheduled, snapshot()).await;
    assert_eq!(free.hand().await, Ok(()));
    assert_eq!(*free.started.lock().unwrap(), [scheduled]);
    assert!(free.state().await.get("unfinished_handoff").is_none());
}

#[tokio::test]
async fn ended_handoff_whose_input_is_still_queued_counts_a_front_of_another_session() {
    let scheduled = 8_000_000_008_147_101;
    let _host = herdr_host(8_146_112, KEY, false);
    let rig = queued(8_146_112).await;
    rig.admit_kept(scheduled, snapshot()).await;
    assert_eq!(rig.begin(true).await["ended"], true);
    spend_budget(&rig, scheduled).await;
    assert!(rig.started.lock().unwrap().is_empty());
    assert!(rig.state().await.get("unfinished_handoff").is_none());
}

/// One refusal an unended journal cannot pass by itself ends its front on the journal's budget.
async fn refusal_spends_one_budget(channel: u64, _host: impl Sized, pin: Value) {
    let first = 8_000_000_000_000_000 + channel;
    let rig = rig(channel, &[first], false).await;
    let journal = pinned(&rig, pin).await;
    spend_budget(&rig, first).await;
    assert_custody(&rig.state().await, &journal);
    assert!(rig.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn every_refusal_an_unended_journal_cannot_pass_spends_one_budget() {
    use crate::db::dispatched_sessions::hosted_execution::{self as hosted, HostedObservation};
    // The channel's session key cannot be resolved.
    let hosts = crate::config::session_hosts::force_for_test(Some("n"), &[(8_146_113, "n")]);
    refusal_spends_one_budget(8_146_113, hosts, pin()).await;
    // The host names another session than the journal's.
    let other = herdr_host(8_146_114, "claude/t/h:other", false);
    refusal_spends_one_budget(8_146_114, other, pin()).await;
    // The host's session key names no session.
    let nameless = herdr_host(8_146_115, "nameless", false);
    refusal_spends_one_budget(8_146_115, nameless, pin()).await;
    // The saved pin cannot be read.
    let unread = herdr_host(8_146_116, KEY, false);
    refusal_spends_one_budget(8_146_116, unread, json!({"pane":"%0"})).await;
    // A Bound row stands while the old pane is not known ended.
    let owner = hosted::tests::owner("8146117");
    let record = hosted::tests::record(&owner, "bound-nonce", HostedState::Bound);
    let bound = force_host(8_146_117, KEY.into(), HostedObservation::known_fixture(record));
    refusal_spends_one_budget(8_146_117, bound, pin()).await;
    // The old pane's close keeps failing.
    let closing = herdr_host(8_146_118, KEY, true);
    refusal_spends_one_budget(8_146_118, closing, pin()).await;
}

#[tokio::test]
async fn unjudgeable_front_without_a_journal_is_retained_after_one_retry() {
    let (first, second) = (8_000_000_008_147_201, 8_000_000_008_147_202);
    let _host = herdr_host(8_146_119, "nameless", false);
    let rig = queued(8_146_119).await;
    rig.enqueue(first).await;
    rig.enqueue(second).await;
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    let state = rig.state().await;
    assert_eq!(state["items"].as_array().unwrap().len(), 2);
    assert!(state["retained_inputs"].as_array().unwrap().is_empty());
    assert_eq!(rig.hand().await.unwrap_err(), RETAINED);
    let state = rig.state().await;
    let retained = state["retained_inputs"].as_array().unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0]["input"]["original"]["message_id"], first);
    assert_eq!(retained[0]["notice"]["kind"], "retained_retry_required");
    assert_eq!(state["items"][0]["message_id"], second);
    assert!(state.get("unfinished_handoff").is_none());
    assert!(rig.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_cancelled_front_takes_its_budget_with_it() {
    let (first, second, third, last) = (
        8_000_000_008_147_301,
        8_000_000_008_147_302,
        8_000_000_008_147_303,
        8_000_000_008_147_304,
    );
    let _host = refuse_host(8_146_120);
    let (rig, journal) = custody(8_146_120, first).await;
    for id in [second, third, last] {
        rig.enqueue(id).await;
    }
    for _ in 0..4 {
        assert_eq!(rig.hand().await.unwrap_err(), HELD);
        rig.expire().await;
    }
    // Cancelling a later input leaves the spending front's count alone.
    cancel(&rig, last).await;
    assert_eq!(rig.state().await["unfinished_handoff"]["failures"], 4);
    cancel(&rig, second).await;
    assert_custody(&rig.state().await, &journal);
    rig.reboot().await;
    assert_custody(&rig.state().await, &journal);
    spend_budget(&rig, third).await;
    assert_custody(&rig.state().await, &journal);
    assert!(rig.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_reboot_mid_backoff_resumes_the_same_budget() {
    let (first, second) = (8_000_000_008_147_401, 8_000_000_008_147_402);
    let _host = refuse_host(8_146_121);
    let (rig, journal) = custody(8_146_121, first).await;
    rig.enqueue(second).await;
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    rig.expire().await;
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    rig.reboot().await;
    let saved = rig.state().await["unfinished_handoff"].clone();
    assert_eq!(saved["id"], journal["id"]);
    assert_eq!(saved["failures"], 2);
    assert!(saved["retry_after_ms"].as_u64().unwrap() > now_ms());
    recover(rig.shared.clone(), rig.channel);
    for _ in 0..40 {
        if waiting(&rig, &journal) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waiting(&rig, &journal), "the saved deadline is awaited again");
    // A look before that deadline neither starts nor spends.
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    assert_eq!(rig.state().await["unfinished_handoff"], saved);
    for failures in 3..=4u64 {
        rig.expire().await;
        assert_eq!(rig.hand().await.unwrap_err(), HELD);
        assert_eq!(rig.state().await["unfinished_handoff"]["failures"], failures);
    }
    rig.expire().await;
    assert_eq!(rig.hand().await.unwrap_err(), RETAINED);
    assert_custody(&rig.state().await, &journal);
    assert!(rig.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelling_every_waiting_input_keeps_an_unended_handoff() {
    let (first, second, later) = (
        8_000_000_008_147_501,
        8_000_000_008_147_502,
        8_000_000_008_147_503,
    );
    let _host = refuse_host(8_146_122);
    let rig = rig(8_146_122, &[first, second], false).await;
    let journal = pinned(&rig, pin()).await;
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    let cleared = (rig.mailbox)
        .v2_request(|slot| Ok(slot.as_mut().unwrap().clear_waiting()?.queued.len()));
    assert_eq!(cleared.await, Ok(2));
    let state = rig.state().await;
    assert!(state["items"].as_array().unwrap().is_empty());
    assert_custody(&state, &journal);
    // A routine start that follows is judged under the custody instead of starting.
    rig.admit_kept(later, routine()).await;
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    let state = rig.state().await;
    assert_eq!(state["unfinished_handoff"]["id"], journal["id"]);
    assert_eq!(state["unfinished_handoff"]["failures"], 1);
    assert!(rig.started.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelling_the_handoff_input_keeps_its_unended_journal() {
    let (first, second) = (8_000_000_008_147_601, 8_000_000_008_147_602);
    let _host = refuse_host(8_146_123);
    let rig = rig(8_146_123, &[first, second], false).await;
    let journal = pinned(&rig, pin()).await;
    assert_eq!(rig.hand().await.unwrap_err(), HELD);
    cancel(&rig, first).await;
    assert_custody(&rig.state().await, &journal);
    // An ended journal owes no pane, and leaves with its input as before.
    let _ended = refuse_host(8_146_124);
    let ended = super::left_herdr_tests::rig(8_146_124, &[first, second], true).await;
    cancel(&ended, first).await;
    assert!(ended.state().await.get("unfinished_handoff").is_none());
}
