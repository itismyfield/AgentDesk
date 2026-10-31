use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct OutcomePorts {
    outcomes:
        Mutex<VecDeque<Result<router::HeadlessTurnStartOutcome, router::HeadlessTurnStartError>>>,
    starts: AtomicUsize,
    enqueues: AtomicUsize,
    inspections: AtomicUsize,
}

#[async_trait]
impl DeliveryPorts for OutcomePorts {
    async fn external_hold(&self) -> Option<ExternalHold> {
        None
    }

    async fn try_start(&self) -> StartAttempt {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.outcomes.lock().unwrap().pop_front().unwrap().into()
    }

    async fn mailbox_holder(&self) -> MailboxHolder {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        MailboxHolder::Nothing
    }

    async fn enqueue(&self) -> Result<String, String> {
        self.enqueues.fetch_add(1, Ordering::SeqCst);
        Ok("new-admission-must-not-be-created".into())
    }

    fn inject_mode(&self) -> InjectMode {
        InjectMode::Off
    }

    async fn try_inject(&self) -> InjectAttempt {
        panic!("injection is disabled")
    }
}

fn ports(outcome: router::HeadlessTurnStartOutcome, retry: bool) -> OutcomePorts {
    let mut outcomes = VecDeque::new();
    if retry {
        outcomes.push_back(Err(router::HeadlessTurnStartError::Conflict("busy".into())));
    }
    outcomes.push_back(Ok(outcome));
    OutcomePorts {
        outcomes: Mutex::new(outcomes),
        starts: AtomicUsize::new(0),
        enqueues: AtomicUsize::new(0),
        inspections: AtomicUsize::new(0),
    }
}

#[tokio::test]
async fn accepted_is_queued_with_the_same_turn_without_readmission_on_either_start() {
    for retry in [false, true] {
        let outcome = router::HeadlessTurnStartOutcome::accepted("discord:7:123".into());
        assert_eq!(outcome.status.as_str(), "accepted");
        assert_eq!(outcome.owned_session_key, None);
        let fake = ports(outcome, retry);
        assert_eq!(
            deliver_with_ports(&fake).await,
            Ok(HumanInputDelivery::Queued {
                turn_id: "discord:7:123".into(),
                reason: "accepted".into(),
                inject_veto: None,
            })
        );
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1 + usize::from(retry));
        assert_eq!(fake.inspections.load(Ordering::SeqCst), usize::from(retry));
        assert_eq!(fake.enqueues.load(Ordering::SeqCst), 0);
        assert!(fake.outcomes.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn started_and_consumed_preserve_the_legacy_delivery_on_either_start() {
    for retry in [false, true] {
        for outcome in [
            router::HeadlessTurnStartOutcome::started("discord:7:123".into(), Some("owned".into())),
            router::HeadlessTurnStartOutcome::consumed("discord:7:123".into()),
        ] {
            let fake = ports(outcome, retry);
            assert_eq!(
                deliver_with_ports(&fake).await,
                Ok(HumanInputDelivery::Started {
                    turn_id: "discord:7:123".into()
                })
            );
            assert_eq!(fake.enqueues.load(Ordering::SeqCst), 0);
            assert_eq!(fake.starts.load(Ordering::SeqCst), 1 + usize::from(retry));
        }
    }
}
