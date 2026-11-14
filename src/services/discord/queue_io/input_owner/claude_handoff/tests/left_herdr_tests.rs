//! A journal left on a channel boot config no longer routes to Herdr never holds the queue forever.
use super::*;
use crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests;
use crate::services::discord::queue_io::input_owner::drive::{Drive, Handoff};
use serde_json::Value;

pub(super) struct Rig {
    pub(super) shared: Shared,
    pub(super) mailbox: ChannelMailboxHandle,
    pub(super) channel: Channel,
    pub(super) started: Arc<Mutex<Vec<u64>>>,
    pub(super) drive: Arc<Drive>,
    queue: (std::path::PathBuf, String),
    _root: crate::config::TestRuntimeRootGuard,
}
impl Drop for Rig {
    fn drop(&mut self) {
        set_owner_wake_for_tests(self.channel, None);
    }
}

/// A Claude channel's adopted owner with an empty queue and a drive whose start only records.
pub(super) async fn queued(channel: u64) -> Rig {
    let root = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let done = &shared.restart.reconcile_done;
    done.store(true, std::sync::atomic::Ordering::Release);
    let channel = Channel::new(channel);
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let queue = crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox
        .v2_adopt(queue.0.clone(), queue.1.clone())
        .await
        .unwrap();
    let started = Arc::new(Mutex::new(Vec::new()));
    let seen = started.clone();
    let idle: Handoff = Arc::new(move |front| {
        seen.lock().unwrap().push(front.1.message_id);
        Box::pin(async { Ok(()) })
    });
    let drive = crate::services::discord::router::message_handler::owner_drive_for_test(
        (mailbox.clone(), None),
        (ProviderKind::Claude, channel.get()),
        idle,
        Some(shared.clone()),
    );
    assert!(set_owner_wake_for_tests(channel, Some(Arc::new(|| ()))).is_none());
    Rig {
        shared,
        mailbox,
        channel,
        started,
        drive,
        queue,
        _root: root,
    }
}

/// An unmapped Claude channel whose queue holds `messages`, the first under a handoff journal.
pub(super) async fn rig(channel: u64, messages: &[u64], ended: bool) -> Rig {
    let rig = queued(channel).await;
    for id in messages {
        rig.enqueue(*id).await;
    }
    rig.begin(ended).await;
    rig
}

impl Rig {
    pub(super) async fn enqueue(&self, id: u64) {
        let item = original(self.channel.get(), id);
        let queued = self.mailbox.v2_request(move |slot| {
            let queued = slot.as_mut().unwrap().enqueue_fixture(&item);
            queued.map(drop).map_err(|e| format!("{e:?}"))
        });
        queued.await.unwrap();
    }
    /// Admits a routine, scheduled or HTTP start with the context its seam keeps.
    pub(super) async fn admit_kept(&self, id: u64, context: Value) {
        let (item, ticket) = (original(self.channel.get(), id), format!("kept-{id}"));
        let admitted = self.mailbox.v2_admit_ticket_with_context(item, ticket, context);
        assert!(admitted.await.is_ok(), "the kept start is admitted");
    }
    /// Opens the queue front's handoff journal, ended when its old pane is known closed.
    pub(super) async fn begin(&self, ended: bool) -> Value {
        let begun = self.mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            let journal = owner.begin_handoff(&front, "left".into(), "claude/t/h:left".into())?;
            if ended {
                owner.handoff_ended(&journal.id)?;
            }
            Ok(())
        });
        begun.await.unwrap();
        self.state().await["unfinished_handoff"].clone()
    }
    /// One pass of the drive's entry over the owner's current front.
    pub(super) async fn hand(&self) -> Result<(), String> {
        let front = self
            .mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().unwrap();
                let front = owner.front().ok_or("no front")?;
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        (self.drive.handoff)(front).await
    }
    pub(super) async fn state(&self) -> Value {
        self.mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap()
    }
    /// Lets the journal's persisted backoff deadline pass.
    pub(super) async fn expire(&self) {
        let expired = self.mailbox.v2_request(|slot| {
            let journal = slot.as_mut().unwrap().snapshot.unfinished_handoff.as_mut();
            journal.ok_or("no journal")?.retry_after_ms = 1;
            Ok(())
        });
        expired.await.unwrap();
    }
    /// A process restart: the owner is adopted again from its published queue.
    pub(super) async fn reboot(&self) {
        let (path, namespace) = self.queue.clone();
        self.mailbox.v2_adopt(path, namespace).await.unwrap();
    }
}

#[tokio::test]
async fn ended_journal_on_a_channel_that_left_herdr_is_dropped_and_its_front_starts() {
    let rig = rig(8_146_101, &[8_000_000_008_146_101], true).await;
    assert_eq!(rig.hand().await, Ok(()));
    assert_eq!(*rig.started.lock().unwrap(), [8_000_000_008_146_101]);
    let state = rig.state().await;
    assert!(state.get("unfinished_handoff").is_none());
    assert_eq!(state["items"].as_array().unwrap().len(), 1);
    assert!(state["retained_inputs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn unended_journal_on_a_channel_that_left_herdr_spends_its_budget_then_retains_the_front() {
    budget_retains_the_front(8_146_102, false).await;
}

#[tokio::test]
async fn ended_journal_whose_mapped_host_refuses_spends_its_budget_then_retains_the_front() {
    let _host = refuse_host(8_146_104);
    budget_retains_the_front(8_146_104, true).await;
}

/// Four counted holds, each behind a doubling deadline; the fifth retains the front and the next
/// front takes the ordinary path.
async fn budget_retains_the_front(channel: u64, ended: bool) {
    let (first, second) = (8_000_000_008_146_201, 8_000_000_008_146_202);
    let rig = rig(channel, &[first, second], ended).await;
    for failures in 1..=4u64 {
        assert_eq!(rig.hand().await.unwrap_err(), HELD);
        // A wake before the persisted deadline holds again without spending the budget.
        assert_eq!(rig.hand().await.unwrap_err(), HELD);
        let state = rig.state().await;
        let journal = &state["unfinished_handoff"];
        assert_eq!(journal["failures"], failures);
        let wait = journal["retry_after_ms"].as_u64().unwrap() - now_ms();
        assert!(wait > 500 << failures && wait <= 1_000 << failures);
        assert_eq!(state["items"].as_array().unwrap().len(), 2);
        assert!(state["retained_inputs"].as_array().unwrap().is_empty());
        rig.mailbox
            .v2_request(|slot| {
                let journal = slot.as_mut().unwrap().snapshot.unfinished_handoff.as_mut();
                journal.unwrap().retry_after_ms = 1;
                Ok(())
            })
            .await
            .unwrap();
    }
    assert_eq!(
        rig.hand().await.unwrap_err(),
        "NotSent(\"owner_start_failed\")"
    );
    let state = rig.state().await;
    assert!(state.get("unfinished_handoff").is_none());
    let retained = state["retained_inputs"].as_array().unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0]["input"]["original"]["message_id"], first);
    assert_eq!(retained[0]["notice"]["kind"], "retained_retry_required");
    assert!(rig.started.lock().unwrap().is_empty());
    assert_eq!(rig.hand().await, Ok(()));
    assert_eq!(*rig.started.lock().unwrap(), [second]);
}

#[tokio::test]
async fn recovery_drops_an_ended_journal_on_a_channel_that_left_herdr() {
    let rig = rig(8_146_103, &[8_000_000_008_146_301], true).await;
    recover(rig.shared.clone(), rig.channel);
    for _ in 0..200 {
        if rig.state().await.get("unfinished_handoff").is_none() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("recovery kept the journal of a channel that left Herdr");
}

#[tokio::test]
async fn unended_journal_whose_mapped_host_refuses_keeps_custody_and_backs_off() {
    let _host = refuse_host(8_146_105);
    let rig = rig(8_146_105, &[8_000_000_008_146_501], false).await;
    for failures in 1..=6u64 {
        assert_eq!(rig.hand().await.unwrap_err(), HELD);
        assert_eq!(
            rig.state().await["unfinished_handoff"]["failures"],
            failures
        );
    }
    let state = rig.state().await;
    assert_eq!(state["items"].as_array().unwrap().len(), 1);
    assert!(state["retained_inputs"].as_array().unwrap().is_empty());
    assert!(rig.started.lock().unwrap().is_empty());
}
