//! #5996 — the active-turn anchor release, moved verbatim out of a registered giant.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::{ActiveTurnKind, ChannelMailboxState, pause_inbound_stall_for_turn};
use crate::services::provider::{CancelToken, ProviderKind};

/// Rendering only, never the discriminator: `ProviderKind::Unsupported::as_str`
/// returns whatever provider string a config supplied, so an agent named
/// `unidentified` renders identically. No consumer reads either term yet;
/// `provider_measured` is the one a future consumer must read.
const UNIDENTIFIED_RELEASE_PROVIDER: &str = "unidentified";

/// Drop the active-turn anchor, returning the token that turn owned. #5937 —
/// `Clear` and a force `PurgeQueue` release this same anchor. #5996 — the
/// channel and the CALL SITE's provider (never the retiring turn's) now arrive
/// here; nothing DECIDES on them, and I20 still does not authorize the release.
pub(super) fn release_active_turn_anchor(
    state: &mut ChannelMailboxState,
    channel_id: ChannelId,
    provider: Option<&ProviderKind>,
) -> Option<Arc<CancelToken>> {
    let removed_token = state.cancel_token.take();
    tracing::debug!(
        channel_id = channel_id.get(),
        provider_measured = provider.is_some(),
        provider = provider.map_or(UNIDENTIFIED_RELEASE_PROVIDER, ProviderKind::as_str),
        released_token = removed_token.is_some(),
        "active-turn anchor release ran without consulting progress evidence"
    );
    let held = state
        .turn_started_instant
        .filter(|_| removed_token.is_some());
    pause_inbound_stall_for_turn(state, held);
    state.active_request_owner = None;
    state.active_user_message_id = None;
    state.active_absorbed_source_ids.clear();
    state.active_turn_nonce = None;
    // #3167 — clear the priority class with the rest of the active-turn anchor.
    state.active_turn_kind = ActiveTurnKind::default();
    state.recovery_started_at = None;
    state.turn_started_at = None;
    state.turn_started_instant = None;
    removed_token
}

/// #5996 — these pin the ARRIVAL of I20's two terms, never that a release was earned.
#[cfg(test)]
mod lease_release_identity_tests {
    use super::super::*;
    use super::*;
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
        crate::logging::test_capture::pin_callsite_interest();
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

    /// The STRUCTURAL term. Unlike `UNMEASURED_FIELD` — which any config can
    /// forge through `ProviderKind::Unsupported` — nothing in the provider
    /// domain can reach this one, so it is what these tests treat as
    /// authoritative for "was a provider measured at all".
    const UNMEASURED_TERM: &str = "provider_measured=false";
    const MEASURED_TERM: &str = "provider_measured=true";

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
        assert!(
            logs.contains(MEASURED_TERM),
            "a carried provider must read as measured: {logs}"
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
            logs.contains(UNMEASURED_TERM),
            "the unmeasured term must be carried structurally, not only rendered: {logs}"
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
            logs.contains(UNMEASURED_TERM),
            "the unmeasured term must be carried structurally, not only rendered: {logs}"
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
            logs.contains(UNMEASURED_TERM),
            "the unmeasured term must be carried structurally, not only rendered: {logs}"
        );
        assert!(
            !logs.contains("provider=\"claude\""),
            "the missing provider must not be defaulted to one: {logs}"
        );
    }

    /// #5996 — an adversarial review of this lane found that
    /// `UNIDENTIFIED_RELEASE_PROVIDER` sits INSIDE the measured domain:
    /// `ProviderKind::Unsupported(String)` is built from config and DB provider
    /// strings, and `as_str` hands that string straight back. An agent
    /// configured as `unidentified` therefore renders exactly like a call site
    /// that carried nothing. This pins that the collision is real AND that the
    /// structural term still separates the two, so the consumer a later lane
    /// wires will not have to decide a retirement on a forgeable string.
    #[test]
    fn a_provider_named_like_the_sentinel_stays_distinguishable_from_unmeasured() {
        let forged = ProviderKind::Unsupported(UNIDENTIFIED_RELEASE_PROVIDER.to_string());
        assert_eq!(
            forged.as_str(),
            UNIDENTIFIED_RELEASE_PROVIDER,
            "this test proves nothing unless the rendering collision is real"
        );

        let channel_id = ChannelId::new(5_996_009);
        let measured = captured_release_logs(async move {
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            assert!(
                handle
                    .try_start_turn(
                        Arc::new(CancelToken::new()),
                        UserId::new(5996),
                        MessageId::new(9),
                    )
                    .await
            );
            let cleared = handle
                .clear(QueuePersistenceContext::new(&forged, "l1-forged", None))
                .await;
            assert!(cleared.removed_token.is_some());
        });

        assert!(
            measured.contains(UNMEASURED_FIELD),
            "the rendered field is expected to collide; that is the defect: {measured}"
        );
        assert!(
            measured.contains(MEASURED_TERM),
            "a measured provider must say so structurally even when it renders \
             like the sentinel: {measured}"
        );
        assert!(
            !measured.contains(UNMEASURED_TERM),
            "a carried provider must never read as unmeasured: {measured}"
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
        assert!(
            logs.contains(MEASURED_TERM),
            "a carried provider must read as measured: {logs}"
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
        assert!(
            logs.contains(MEASURED_TERM),
            "a carried provider must read as measured: {logs}"
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
        }
    }
}
