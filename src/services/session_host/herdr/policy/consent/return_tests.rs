use super::super::{ApplyResult, Question};
use super::*;

const L1: [u8; 32] = [1; 32];
const L2: [u8; 32] = [2; 32];

fn question(id: &str) -> Question {
    let mut question = super::super::tests::question(id);
    question.intent.store = "mailbox".into();
    question
}

fn applied(store: &ConsentStore, held: &Consent) -> Consent {
    let accepted = store.decide(held, 1, true, 101).unwrap();
    let receipt = ApplyResult {
        before: Some([1; 32]),
        after: Some([4; 32]),
        backup: None,
        detail: None,
    };
    store.record_apply(&accepted, receipt).unwrap()
}

struct Sink(Vec<String>);
#[async_trait::async_trait]
impl CustodySink for Sink {
    async fn release(&mut self, _: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        self.0.push(intent.id.clone());
        Ok(())
    }
    async fn wake(&mut self, _: &SettlementKey, _: &IntentRef) -> Result<(), String> {
        Ok(())
    }
}

// A returned input stays held, never releases, and closes only after adoption of its binding.
#[tokio::test]
async fn returned_input_stays_held_never_releases_and_closes_only_after_adoption() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let (first, second) = (question("first"), question("second"));
    let (a, b) = (first.intent.clone(), second.intent.clone());
    store.hold(first, 100).unwrap();
    let held = store.hold(second, 100).unwrap().into_consent();
    let returned = store
        .settle(&held, &[(a.clone(), Resolution::Returned { target: L2 })])
        .unwrap();
    let returned = store
        .settle(
            &returned,
            &[(a.clone(), Resolution::Returned { target: L1 })],
        )
        .expect("a reservation follows the latest refused rollback");
    assert_eq!(returned.unresolved_intents(), vec![a.clone(), b.clone()]);
    for refused in [
        vec![(a.clone(), Resolution::Released)],
        vec![(a.clone(), Resolution::Closed { target: L1 })],
        vec![(a.clone(), Resolution::Adopted { target: L2 })],
        vec![
            (a.clone(), Resolution::Adopted { target: L1 }),
            (a.clone(), Resolution::Closed { target: L2 }),
        ],
    ] {
        assert!(store.settle(&returned, &refused).is_err(), "{refused:?}");
    }
    assert_eq!(
        store.load(&returned.key).unwrap().as_ref(),
        Some(&returned),
        "a refused batch writes none of its transitions"
    );
    let applied = applied(&store, &returned);
    assert_eq!(applied.releasable_intents(), vec![b.clone()]);
    let mut sink = Sink(Vec::new());
    let released = release(&store, &applied, &mut sink).await.unwrap();
    let replayed = release(&store, &released, &mut sink).await.unwrap();
    assert_eq!(
        sink.0,
        vec![b.id.clone()],
        "receipt replay never releases a return"
    );
    assert_eq!(replayed.unresolved_intents(), vec![a.clone()]);
    let closed = store
        .settle(
            &replayed,
            &[
                (a.clone(), Resolution::Adopted { target: L1 }),
                (a.clone(), Resolution::Closed { target: L1 }),
            ],
        )
        .unwrap();
    assert!(closed.unresolved_intents().is_empty());
    for reopen in [
        Resolution::Returned { target: L1 },
        Resolution::Discarded,
        Resolution::Released,
    ] {
        assert!(store.settle(&closed, &[(a.clone(), reopen)]).is_err());
    }
    let reloaded = ConsentStore::new(Some(temp.path().join("consents")));
    assert_eq!(reloaded.load(&closed.key).unwrap(), Some(closed));
}

// Clear still discards an open return; release never does.
#[test]
fn clear_discards_an_open_return_and_release_cannot() {
    for adopted in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = ConsentStore::new(Some(temp.path().join("consents")));
        let held = store.hold(question("first"), 100).unwrap().into_consent();
        let a = held.intents[0].clone();
        let mut open = vec![(a.clone(), Resolution::Returned { target: L1 })];
        if adopted {
            open.push((a.clone(), Resolution::Adopted { target: L1 }));
        }
        let returned = store.settle(&held, &open).unwrap();
        assert!(
            store
                .settle(&returned, &[(a.clone(), Resolution::Released)])
                .is_err()
        );
        let cleared = store.acknowledge_discard(&returned, &a).unwrap();
        assert_eq!(cleared.resolution(&a), Some(Resolution::Discarded));
        assert!(cleared.unresolved_intents().is_empty());
    }
}

// Applied retry asks for a new generation only for awaiting input on another preview.
#[test]
fn applied_retry_reconsents_only_awaiting_input_on_another_preview() {
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let held = store.hold(question("first"), 100).unwrap().into_consent();
    let applied = applied(&store, &held);
    let done = store
        .acknowledge_discard(&applied, &held.intents[0])
        .unwrap();
    assert!(!done.needs_new_generation());
    let mut newer = question("second");
    newer.preview.revision = 2;
    newer.preview.chain_hash = [9; 32];
    let b = newer.intent.clone();
    let mixed = store.hold(newer, 102).unwrap().into_consent();
    assert!(mixed.needs_new_generation());
    let returned = store
        .settle(&mixed, &[(b.clone(), Resolution::Returned { target: L1 })])
        .unwrap();
    assert!(!returned.needs_new_generation());
    let mut retry = question("second");
    retry.preview = returned.preview.clone();
    let retried = store.retry(retry, 103).unwrap();
    assert_eq!(
        (retried.state, retried.nonce),
        (State::Applied, returned.nonce),
        "a returned input never opens a new consent generation"
    );
}
