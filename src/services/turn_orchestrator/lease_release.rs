//! #5996 — the active-turn anchor release, extracted from the parent so the
//! terms I20 asks that retirement point to carry can arrive without growing a
//! registered giant (`scripts/giant_file_registry.toml`, `decision = "shrink"`,
//! `#4710`). The parent kept this frame inline; nothing about the release
//! changed in the move.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{
    ActiveTurnKind, ChannelMailboxState, pause_inbound_stall_for_turn,
    reset_watchdog_extension_state,
};
use crate::services::provider::{CancelToken, ProviderKind};

/// What the release site names when its call site carries no provider. A label
/// for an UNMEASURED term, never a provider value — see #5996 below.
const UNIDENTIFIED_RELEASE_PROVIDER: &str = "unidentified";

/// Drop the active-turn anchor, returning the token that turn owned. #5937 —
/// the window this turn held was never drain time, so it is discounted here
/// rather than at the turn end alone: `Clear` and a force `PurgeQueue` release
/// this same anchor, and missing them counts a long turn as a wedged drain.
///
/// #5996 — I20's FIRST named exception. This retirement point carried neither
/// channel nor provider, so it could not say WHICH anchor it retires; both now
/// arrive. Nothing here DECIDES on them, and the arrival closes nothing: I20's
/// discriminator (a durable completion witness, or a MEASURED unrelayed tail) is
/// not reachable from this frame. #6012 reordered the rowless arm to run AFTER
/// `sweep_coverage` and carry what it saw; the contract records that this did
/// not close the gap and that ORDERING ALONE never will. What fails is the
/// SCOPE of those numbers — they cover the INCARNATION, not this turn, because
/// `ObligationExtinction::ReceiptCovered` has no producer — so until a term
/// isolates the current turn the release stays unauthorized. `provider` is
/// `None` at a call site carrying no `QueuePersistenceContext`; that absence is
/// UNMEASURED and is named as such rather than defaulted to some provider.
pub(super) fn release_active_turn_anchor(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    provider: Option<&ProviderKind>,
) -> Option<Arc<CancelToken>> {
    let removed_token = state.cancel_token.take();
    tracing::debug!(
        channel_id = channel_id.get(),
        provider = provider.map_or(UNIDENTIFIED_RELEASE_PROVIDER, ProviderKind::as_str),
        released_token = removed_token.is_some(),
        "released the active-turn anchor without consulting progress evidence"
    );
    let held = state
        .turn_started_instant
        .filter(|_| removed_token.is_some());
    pause_inbound_stall_for_turn(state, held);
    state.active_request_owner = None;
    state.active_user_message_id = None;
    state.active_turn_nonce = None;
    // #3167 — clear the priority class with the rest of the active-turn anchor.
    state.active_turn_kind = ActiveTurnKind::default();
    state.recovery_started_at = None;
    state.turn_started_at = None;
    state.turn_started_instant = None;
    reset_watchdog_extension_state(state);
    removed_token
}

/// #5996 — the two terms I20 asks a retirement point to carry must ARRIVE at
/// `release_active_turn_anchor`, and the `None` provider must stay visible as
/// the unmeasured term it is. These tests pin the arrival only; nothing here
/// asserts a release was EARNED, because on today's operands none can be.
#[cfg(test)]
mod lease_release_identity_tests {
    use super::*;
    use super::super::*;
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buffer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Drive `body` to completion on a current-thread runtime and return what it
    /// logged. The runtime must be current-thread: the mailbox actor is a spawned
    /// task, and only there does it run on the thread whose dispatcher
    /// `with_default` replaced.
    fn captured_release_logs<F>(body: F) -> String
    where
        F: std::future::Future<Output = ()>,
    {
        let writer = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        tracing::subscriber::with_default(subscriber, || runtime.block_on(body));
        let bytes = writer
            .buffer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        String::from_utf8(bytes).expect("captured log is utf-8")
    }

    /// The literal the unmeasured assertions below pin. Deliberately NOT
    /// `format!("{UNIDENTIFIED_RELEASE_PROVIDER}")`: interpolating the constant
    /// moves the expectation along with it, so such an assertion cannot see the
    /// label being redefined. A mutation run proved that — defaulting the
    /// constant to `"claude"` left an interpolated assertion green.
    const UNMEASURED_FIELD: &str = "provider=\"unidentified\"";

    #[test]
    fn the_unmeasured_label_is_the_literal_these_tests_pin() {
        assert_eq!(
            format!("provider=\"{UNIDENTIFIED_RELEASE_PROVIDER}\""),
            UNMEASURED_FIELD,
            "the production label drifted from the literal the assertions pin"
        );
    }

    /// The `Some` provider term reaches `finalize_turn_state` through the
    /// `FinishTurn` message, which hands it a context of its own.
    #[test]
    fn finish_turn_release_names_the_channel_and_the_carried_provider() {
        let channel_id = ChannelId::new(5_996_001);
        let logs = captured_release_logs(async move {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            assert!(
                handle
                    .try_start_turn(
                        Arc::new(CancelToken::new()),
                        UserId::new(5996),
                        MessageId::new(1),
                    )
                    .await
            );
            let finished = handle
                .finish_turn(QueuePersistenceContext::new(
                    &ProviderKind::Codex,
                    "l1-carried",
                    None,
                ))
                .await;
            assert!(finished.removed_token.is_some());
        });

        assert!(
            logs.contains(&format!("channel_id={}", channel_id.get())),
            "the release must name the channel it retires: {logs}"
        );
        assert!(
            logs.contains("provider=\"codex\""),
            "the release must name the provider its call site carried: {logs}"
        );
    }

    #[test]
    fn hard_stop_release_without_persistence_names_the_provider_unmeasured() {
        let logs = captured_release_logs(async {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(ChannelId::new(5_996_002));
            assert!(
                handle
                    .try_start_turn(
                        Arc::new(CancelToken::new()),
                        UserId::new(5996),
                        MessageId::new(2),
                    )
                    .await
            );
            assert!(handle.hard_stop().await.removed_token.is_some());
        });

        assert!(
            logs.contains(UNMEASURED_FIELD),
            "a call site with no persistence context carries no provider: {logs}"
        );
        assert!(
            !logs.contains("provider=\"claude\""),
            "the missing provider must not be defaulted to one: {logs}"
        );
    }

    /// A claim that DID carry a context still retires unmeasured, and this lane
    /// pins that rather than papering over it. `ChannelMailboxMsg::TryStartTurn`
    /// never assigns `state.last_persistence` — it spends the context on the
    /// active-source purge alone — so a later `HardStop` reads `None`. The
    /// `None` population is therefore WIDER than "a mailbox that never
    /// persisted": it is "a mailbox that took no queue-persisting message before
    /// the stop". A lane wiring the I20 violation record must count this arm as
    /// an UNREAD provider term, never as one attributed to the claim.
    #[test]
    fn hard_stop_after_a_persistence_carrying_claim_still_names_the_provider_unmeasured() {
        let logs = captured_release_logs(async {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(ChannelId::new(5_996_007));
            assert!(
                handle
                    .try_start_turn_with_persistence(
                        Arc::new(CancelToken::new()),
                        UserId::new(5996),
                        MessageId::new(7),
                        QueuePersistenceContext::new(&ProviderKind::Codex, "l1-claim-only", None),
                    )
                    .await
                    .started
            );
            assert!(handle.hard_stop().await.removed_token.is_some());
        });

        assert!(
            logs.contains(UNMEASURED_FIELD),
            "the claim's context never reaches the release: {logs}"
        );
        assert!(
            !logs.contains("provider=\"codex\""),
            "the claim's provider must not be inferred at the release: {logs}"
        );
    }

    #[test]
    fn finish_cancelled_turn_release_without_persistence_names_the_provider_unmeasured() {
        let logs = captured_release_logs(async {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(ChannelId::new(5_996_003));
            let token = Arc::new(CancelToken::new());
            assert!(
                handle
                    .try_start_turn(token.clone(), UserId::new(5996), MessageId::new(3))
                    .await
            );
            token
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(handle.finish_cancelled_turn().await.removed_token.is_some());
        });

        assert!(
            logs.contains(UNMEASURED_FIELD),
            "the second no-persistence call site carries no provider either: {logs}"
        );
        assert!(
            !logs.contains("provider=\"claude\""),
            "the missing provider must not be defaulted to one: {logs}"
        );
    }

    /// The `Clear` and force-`PurgeQueue` arms release the same anchor outside
    /// `finalize_turn_state`, so each needs its own carry. Both run with an
    /// EMPTY queue: `save_channel_queue` then only removes a file no synthetic
    /// channel owns, so these assert on the release alone and never depend on
    /// which persistence root is in effect.
    #[test]
    fn clear_arm_release_names_the_channel_and_the_message_provider() {
        let channel_id = ChannelId::new(5_996_005);
        let logs = captured_release_logs(async move {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            assert!(
                handle
                    .try_start_turn(
                        Arc::new(CancelToken::new()),
                        UserId::new(5996),
                        MessageId::new(5),
                    )
                    .await
            );
            let cleared = handle
                .clear(QueuePersistenceContext::new(
                    &ProviderKind::Gemini,
                    "l1-clear",
                    None,
                ))
                .await;
            assert!(cleared.removed_token.is_some());
        });

        assert!(
            logs.contains(&format!("channel_id={}", channel_id.get())),
            "the Clear arm must name the channel it retires: {logs}"
        );
        assert!(
            logs.contains("provider=\"gemini\""),
            "the Clear arm carries the provider its message named: {logs}"
        );
    }

    #[test]
    fn force_purge_arm_release_names_the_channel_and_the_message_provider() {
        let channel_id = ChannelId::new(5_996_006);
        let logs = captured_release_logs(async move {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let token = Arc::new(CancelToken::new());
            assert!(
                handle
                    .try_start_turn(token.clone(), UserId::new(5996), MessageId::new(6))
                    .await
            );
            token
                .cancelled
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let purged = handle
                .purge_queue(
                    QueuePersistenceContext::new(&ProviderKind::Qwen, "l1-purge", None),
                    true,
                )
                .await;
            assert!(purged.cleared_active_anchor);
        });

        assert!(
            logs.contains(&format!("channel_id={}", channel_id.get())),
            "the force-purge arm must name the channel it retires: {logs}"
        );
        assert!(
            logs.contains("provider=\"qwen\""),
            "the force-purge arm carries the provider its message named: {logs}"
        );
    }

    /// The carry decides nothing. Both provider terms must leave the anchor in
    /// the identical state, or this lane has smuggled a judgement into L1.
    #[test]
    fn release_clears_the_same_anchor_whether_or_not_a_provider_is_carried() {
        fn anchored() -> ChannelMailboxState {
            ChannelMailboxState {
                cancel_token: Some(Arc::new(CancelToken::new())),
                active_request_owner: Some(UserId::new(5996)),
                active_user_message_id: Some(MessageId::new(4)),
                active_turn_nonce: Some("nonce".to_string()),
                active_turn_kind: ActiveTurnKind::Background,
                turn_started_at: Some(Utc::now()),
                turn_started_instant: Some(Instant::now()),
                recovery_started_at: Some(Instant::now()),
                ..Default::default()
            }
        }

        let channel_id = ChannelId::new(5_996_004);
        let mut carried = anchored();
        let mut uncarried = anchored();

        assert!(
            release_active_turn_anchor(&mut carried, channel_id, Some(&ProviderKind::Claude))
                .is_some()
        );
        assert!(release_active_turn_anchor(&mut uncarried, channel_id, None).is_some());

        for (label, state) in [("carried", &carried), ("uncarried", &uncarried)] {
            assert!(state.cancel_token.is_none(), "{label}");
            assert!(state.active_request_owner.is_none(), "{label}");
            assert!(state.active_user_message_id.is_none(), "{label}");
            assert!(state.active_turn_nonce.is_none(), "{label}");
            assert_eq!(state.active_turn_kind, ActiveTurnKind::default(), "{label}");
            assert!(state.turn_started_at.is_none(), "{label}");
            assert!(state.turn_started_instant.is_none(), "{label}");
            assert!(state.recovery_started_at.is_none(), "{label}");
            assert!(state.watchdog_deadline_override.is_none(), "{label}");
        }
    }
}
