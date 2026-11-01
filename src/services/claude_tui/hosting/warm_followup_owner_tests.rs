//! The owner's idle Claude submit after its observer: a retired session notifies, never resends.
use super::*;

/// A session that died before any output reaches the turn's relay as a notice that nothing
/// was resent, and the owner's turn ends in error; an observed end passes through unnoticed.
#[test]
fn a_retired_owner_session_notifies_without_resending() {
    let (sender, receiver) = std::sync::mpsc::channel();
    let retired = ClaudeTuiWarmFollowupSubmitOutcome::FallThroughRecreate;
    assert!(owner_settles(retired, &sender).is_err());
    let sent: Vec<_> = receiver.try_iter().collect();
    assert!(
        matches!(&sent[..], [StreamMessage::Text { content }, StreamMessage::Done { .. }]
            if content.contains(OWNER_RETIRED_NOTICE)),
        "{sent:?}"
    );
    let ended = ClaudeTuiWarmFollowupSubmitOutcome::Terminal(Ok(()));
    assert_eq!(owner_settles(ended, &sender), Ok(()));
    assert!(receiver.try_recv().is_err());
}
