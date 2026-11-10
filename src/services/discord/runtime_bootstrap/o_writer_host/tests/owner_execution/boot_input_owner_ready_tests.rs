//! Ready routes an installed owner's wake independently of the legacy busy injection gate.
use super::*;
use crate::services::discord::queue_io::{self, cancel_backstop_test_support as wakes};

struct OwnerWakeCapture(serenity::ChannelId);
impl Drop for OwnerWakeCapture {
    fn drop(&mut self) {
        assert!(wakes::set_owner_wake_for_tests(self.0, None).is_some());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn writer_boot_ready_wakes_busy_v2_owner_with_legacy_queue_gate_closed_pg() {
    scenario(
        "boot_input_owner_ready_tests::writer_boot_ready_wakes_busy_v2_owner_with_legacy_queue_gate_closed_pg",
        73_369_551,
        "mac-a",
        |f| Box::pin(async move {
            let cap = Duration::from_secs(5);
            let channel = serenity::ChannelId::new(f.channel);
            let mailbox = f.shared.mailbox(channel);
            let readiness = host::process_readiness();
            assert!(!readiness.accepts(f.channel));
            assert!(!crate::services::discord::health::InjectOrigin::queued_head_ready(
                &ProviderKind::Claude, channel,
            ), "the legacy queued paste lane must be closed in this regression");

            assert_eq!(queue_io::input_ingress::ADOPTS.scope(true,
                queue_io::input_ingress::adopt(&f.shared, &ProviderKind::Claude, channel),
            ).await, Ok(true));
            assert_eq!(queue_io::input_ingress::installed(&mailbox).await, Ok(true));
            let old_message = serenity::MessageId::new(f.message - 1);
            mailbox.restore_active_turn(
                Arc::new(crate::services::provider::CancelToken::new()),
                serenity::UserId::new(7), old_message,
            ).await;
            let active = crate::services::discord::mailbox_snapshot(&f.shared, channel).await;
            assert_eq!(active.active_user_message_id, Some(old_message));
            assert!(active.cancel_token.is_some());
            assert!(!active.active_turn_kind.is_background(),
                "a real foreground turn must keep the generic idle route closed");

            // Record ready levels at the existing owner-wake seam; no pump/native effect is run.
            let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
            let read = readiness.clone();
            assert!(wakes::set_owner_wake_for_tests(channel, Some(Arc::new(move || {
                send.send(read.accepts(channel.get())).unwrap();
            }))).is_none());
            let capture = OwnerWakeCapture(channel);

            queue_io::input_ingress::ADOPTS.scope(true, f.live(ProviderKind::Claude))
                .await.unwrap();
            // Consume the eager Accepted wake before allowing the writer to become ready.
            assert_eq!(tokio::time::timeout(cap, events.recv()).await.unwrap(), Some(false),
                "the eager ingress wake is observed but cannot count as the ready wake");
            while let Ok(ready) = events.try_recv() {
                assert!(!ready, "the writer semaphore has not been released");
            }
            let kept = mailbox.v2_request(|slot| slot.as_ref()
                .ok_or_else(|| "V2 missing".to_string())?.json()).await.unwrap();
            assert_eq!(kept["items"].as_array().unwrap().len(), 1);
            assert_eq!(kept["items"][0]["message_id"], f.message);
            assert!(kept["head_injection"].is_null());
            assert_eq!(f.effects.load(Ordering::SeqCst), 0);
            assert_eq!(f.checkpoints.load(Ordering::SeqCst), 1);

            // Keep the foreground turn active: readiness alone must now route an owner wake.
            f.release.add_permits(1);
            assert_eq!(tokio::time::timeout(cap, events.recv()).await
                .expect("ready must wake the installed busy owner without the legacy gate"),
                Some(true));
            assert!(readiness.accepts(f.channel));
            assert!(matches!(events.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)),
                "one rising ready edge emits one wake after the eager events were drained");
            let active = crate::services::discord::mailbox_snapshot(&f.shared, channel).await;
            assert_eq!(active.active_user_message_id, Some(old_message),
                "the test must not open the idle route by ending the active turn");
            assert!(active.cancel_token.is_some());
            assert_eq!(f.effects.load(Ordering::SeqCst), 0,
                "this test proves wake routing, not a physical busy-pane write");
            drop(capture);
        }),
    ).await;
}
