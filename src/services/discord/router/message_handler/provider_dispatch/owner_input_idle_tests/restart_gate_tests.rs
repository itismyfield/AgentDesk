//! A warm Source is usable before and after the actual owner drive holds a restart front.
use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support as wakes;
use crate::services::discord::queue_io::input_owner::{drive, restart_gate::Gate};
use crate::test_support::wait::recv_within;
use poise::serenity_prelude::ChannelId;
use std::sync::atomic::{AtomicUsize, Ordering};

enum Event {
    Front(Box<(Front, Settled)>),
    Pass,
}

struct WakeScope(ChannelId, Option<Arc<dyn Fn() + Send + Sync>>);
impl Drop for WakeScope {
    fn drop(&mut self) {
        wakes::set_owner_wake_for_tests(self.0, self.1.take());
    }
}

struct BindingScope(String);
impl Drop for BindingScope {
    fn drop(&mut self) {
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&self.0);
    }
}

struct AnswerScope(Answers);
impl Drop for AnswerScope {
    fn drop(&mut self) {
        if self.0.thread.is_some() {
            self.0.finish();
        }
    }
}

fn next_front(events: &std::sync::mpsc::Receiver<Event>) -> (Front, Settled) {
    match recv_within(events, WAIT_CAP) {
        Event::Front(front) => *front,
        Event::Pass => panic!("the owner pass ended without handing its expected front"),
    }
}

fn pass(events: &std::sync::mpsc::Receiver<Event>) {
    assert!(
        matches!(recv_within(events, WAIT_CAP), Event::Pass),
        "an excluded front reached the idle handoff"
    );
}

#[test]
fn a_restart_gate_holds_a_warm_owner_front_until_drop_resumes_the_same_ticket() {
    let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
    let channel = ChannelId::new(8_128_311);
    let rig = rig(
        "oi-restart-gate-warm",
        channel.get(),
        "normal",
        view("", false),
        IDLE,
    );
    let _binding = BindingScope(rig.name.clone());
    let shared = crate::services::discord::make_shared_data_for_tests();
    assert_eq!(shared.provider, ProviderKind::Claude);
    assert!(!rig.runtime.block_on(
        crate::services::discord::router::intake_dispatch::writer_boot_held(
            &ProviderKind::Claude,
            channel.get(),
        )
    ));
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    rehydrate(&rig, &session);
    let (events, inbox) = std::sync::mpsc::channel::<Event>();
    let events = Arc::new(std::sync::Mutex::new(events));
    let sent = events.clone();
    let idle: Handoff = Arc::new(move |front| {
        let (settled, answer) = tokio::sync::oneshot::channel();
        let sent = sent
            .lock()
            .unwrap()
            .send(Event::Front(Box::new((front, settled))));
        Box::pin(async move {
            sent.map_err(|e| e.to_string())?;
            answer.await.map_err(|e| e.to_string())?
        })
    });
    let mut owner = owner_drive(
        (rig.mailbox.clone(), None),
        (ProviderKind::Claude, channel.get()),
        idle,
        Some(shared.clone()),
    );
    // Observe a completed pass after its original settlement, preserving that callback.
    let previous = owner.settled.clone();
    Arc::get_mut(&mut owner).unwrap().settled = Some(Arc::new(move || {
        let (previous, events) = (previous.clone(), events.clone());
        Box::pin(async move {
            if let Some(previous) = previous {
                previous().await;
            }
            let _ = events.lock().unwrap().send(Event::Pass);
        })
    }));
    let wake_count = Arc::new(AtomicUsize::new(0));
    let (resume, count, runtime) = (
        owner.clone(),
        wake_count.clone(),
        rig.runtime.handle().clone(),
    );
    let _wake = WakeScope(
        channel,
        wakes::set_owner_wake_for_tests(
            channel,
            Some(Arc::new(move || {
                let _entered = runtime.enter();
                count.fetch_add(1, Ordering::SeqCst);
                drive::wake(resume.clone());
            })),
        ),
    );
    let mut answering = AnswerScope(answers(&rig));
    rig.wake(&owner);
    let first = next_front(&inbox);
    assert_eq!(first.0.1.message_id, rig.original.message_id);
    let (result, said) = idle_turn(
        &rig,
        (&session, elsewhere.path()),
        answering.0.token(),
        first,
        |_| None,
    );
    assert_eq!(result, Ok(()), "warm Source control failed");
    assert!(said.iter().any(|m| matches!(m,
        StreamMessage::Text { content } if content.contains("answer 1"))));
    pass(&inbox);
    assert_eq!(enters(&rig.renderer), 1, "the warm control sent one Enter");
    let first_bind = rig.bind();
    assert!(matches!(first_bind, TurnBind::Settled(_)), "{first_bind:?}");
    let second: PendingQueueItem = serde_json::from_value(json!({
        "author_id":7,"message_id":rig.original.message_id + 1,"channel_id":rig.channel,
        "source_message_ids":[rig.original.message_id + 1],"queued_generation":1,
        "merge_consecutive":false,"text":"question kept during restart"
    }))
    .unwrap();
    let ticket = AdmissionTicket::new();
    let admitted = rig
        .runtime
        .block_on(admit(&rig.mailbox, &second, ticket.clone()));
    assert!(
        matches!(
            admitted,
            crate::services::discord::queue_io::input_owner::admission::Admission::Accepted(_)
        ),
        "{admitted:?}"
    );
    rehydrate(&rig, &session);
    let gate = Gate::begin(&shared, channel).unwrap();
    let before = events_for_keys(&rig);
    rig.wake(&owner);
    // A pass, rather than a timed absence, proves the actual guard returned without handoff.
    pass(&inbox);
    assert_eq!(events_for_keys(&rig), before, "restart sent a physical key");
    assert_eq!(enters(&rig.renderer), 1);
    assert!(
        inbox.try_recv().is_err(),
        "the excluded front was handed twice"
    );
    let held = rig
        .runtime
        .block_on(rig.mailbox.v2_request(|slot| {
            let owner = slot.as_ref().ok_or("V2 not adopted")?;
            Ok((owner.front(), owner.front_outstanding()))
        }))
        .unwrap();
    let front = held.0.expect("the same original stays in owner custody");
    assert_eq!(front.0.as_deref(), Some(ticket.as_str()));
    assert_eq!(
        serde_json::to_value(&front.1).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert!(!held.1, "the guard must withdraw the front grant");
    assert_eq!(
        rig.runtime
            .block_on(exact_turn(&rig.mailbox, &ticket, &second))
            .unwrap(),
        TurnBind::NotArmed
    );
    assert_eq!(wake_count.load(Ordering::SeqCst), 0);
    drop(gate);
    assert_eq!(
        wake_count.load(Ordering::SeqCst),
        1,
        "GateDrop supplies the only resume wake"
    );
    let resumed = next_front(&inbox);
    assert_eq!(resumed.0.0.as_deref(), Some(ticket.as_str()));
    assert_eq!(
        serde_json::to_value(&resumed.0.1).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    let (result, said) = idle_turn(
        &rig,
        (&session, elsewhere.path()),
        answering.0.token(),
        resumed,
        |_| None,
    );
    assert_eq!(result, Ok(()));
    assert!(said.iter().any(|m| matches!(m,
        StreamMessage::Text { content } if content.contains("answer 2"))));
    pass(&inbox);
    answering.0.check();
    assert_eq!(
        events_for_keys(&rig),
        (2, 2),
        "each original is entered once"
    );
    assert_eq!(
        rig.bind(),
        first_bind,
        "restart did not replay the prior ticket"
    );
    let bound = rig
        .runtime
        .block_on(exact_turn(&rig.mailbox, &ticket, &second))
        .unwrap();
    assert!(matches!(bound, TurnBind::Settled(_)), "{bound:?}");
    let snapshot = rig.snapshot();
    assert!(
        snapshot["items"].as_array().unwrap().is_empty(),
        "{snapshot}"
    );
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert!(inbox.try_recv().is_err(), "there is no third delivery");
}

fn events_for_keys(rig: &Rig) -> (usize, usize) {
    let keys = events(&rig.renderer);
    (
        keys.iter().filter(|key| *key == "paste").count(),
        keys.iter().filter(|key| *key == "Enter").count(),
    )
}
