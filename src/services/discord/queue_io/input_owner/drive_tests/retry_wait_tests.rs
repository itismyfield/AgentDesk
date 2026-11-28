//! A restarted owner keeps its front's retry wait: the drive grants nothing before the due, and
//! the deadline that pass arms is the wake that hands the front on.
use super::*;
use crate::services::turn_orchestrator::RetryWaitCause;
use adk_test_support::test_support::wait::{WAIT_CAP, await_within};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_retry_wait_grants_nothing_until_its_armed_due_wakes_the_drive() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 7_272_798;
    let now = || chrono::Utc::now().timestamp_millis().max(0) as u64;
    {
        let registry = ChannelMailboxRegistry::default();
        let mailbox = owner(&registry, channel, 1).await;
        let entered = mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            assert!(owner.grant_front(&front)?);
            let grant = owner.grant_number_of(&front).ok_or("no grant")?;
            let cause = RetryWaitCause::PendingUnstarted { nonce: "n".into() };
            // The first wait is 30 s: entered 25 s ago, it falls due 5 s from now.
            owner.enter_retry_wait_granted((&front, grant), cause, now() - 25_000)
        });
        assert!(entered.await.unwrap().is_some());
    }
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(format!("claude/input-owner-drive/{channel}.json"));
    let registry = ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let history = crate::services::tui_input::human_frame::history::v2::Registry::runtime();
    let at = ("claude", channel, now());
    let queue = (path, "input-owner".to_string());
    crate::services::discord::queue_io::input_owner::recovery::recover(
        &mailbox,
        queue,
        history.unwrap(),
        None,
        at,
    )
    .await
    .unwrap();
    let handed = Arc::new(AtomicUsize::new(0));
    let counted = handed.clone();
    let before = move |_: &Front| {
        counted.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>
    };
    let (pass, mut passes) = tokio::sync::mpsc::unbounded_channel();
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff: handoff(mailbox.clone(), channel, before, |_| Completion::Submitted),
        settled: Some(Arc::new(move || {
            let _ = pass.send(());
            Box::pin(async {})
        })),
    });
    wake(drive);
    await_within(passes.recv(), WAIT_CAP).await.unwrap();
    assert_eq!(
        handed.load(Ordering::SeqCst),
        0,
        "re-adopted, still waiting"
    );
    for _ in 0..750 {
        if handed.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(handed.load(Ordering::SeqCst), 1, "the armed due woke it");
}
