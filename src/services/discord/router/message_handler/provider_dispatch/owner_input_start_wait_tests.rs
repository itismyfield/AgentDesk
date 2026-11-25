//! How a read start waits: Ready goes on, an unrecoverable execution is settled after claim, and an
//! unrecovered or unclear read is watched by the same observation.
use super::*;

#[tokio::test]
async fn a_read_start_goes_on_is_refused_or_is_watched_by_its_own_observation() {
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_422_101);
    let context = serde_json::json!({"origin":"headless","source":"wake-watch"});
    let ready = wait(Observed::Native(Screen::Ready), &shared, channel, &context);
    assert!(matches!(ready, super::super::Wait::Go));
    let refused = wait(
        Observed::Reject("owner differs".into()),
        &shared,
        channel,
        &context,
    );
    assert!(matches!(refused, super::super::Wait::Reject(reason) if reason == "owner differs"));
    for observed in [
        Observed::Unrecovered("session transition busy".into()),
        Observed::Native(Screen::Unknown),
        Observed::Native(Screen::Blocked),
    ] {
        let watched = wait(observed, &shared, channel, &context);
        assert!(matches!(watched, super::super::Wait::Watch(_)));
    }
}

#[test]
fn an_unrecovered_read_watches_as_unknown_and_a_refusal_wakes_its_judgement() {
    assert_eq!(
        Observed::Unrecovered("inflight unread".into()).native(),
        Some(Screen::Unknown)
    );
    assert_eq!(Observed::Reject("owner differs".into()).native(), None);
    assert_eq!(
        Observed::Native(Screen::Ready).native(),
        Some(Screen::Ready)
    );
}
