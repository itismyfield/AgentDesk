use super::{ChannelMailboxHandle, InputOrigin, ProviderKind, START_WAITS};
use crate::services::discord::health::external_turn_hold_for_start;
use crate::services::turn_orchestrator::{ChannelMailboxSnapshot, MailboxUnreachable};

const TARGET: &str = "agentdesk::input_owner::idle_wait";

pub(super) async fn active_turn(
    mailbox: &ChannelMailboxHandle,
    origin: InputOrigin,
    provider: &ProviderKind,
    channel: u64,
    message: u64,
) -> Result<bool, MailboxUnreachable> {
    // One mailbox read supplies the decision and the identity of the actor that held it.
    let diagnose =
        !origin.joins_running_turn() && tracing::enabled!(target: TARGET, tracing::Level::INFO);
    let snapshot = if diagnose {
        Some(mailbox.try_snapshot().await?)
    } else {
        None
    };
    let active = match snapshot.as_ref() {
        Some(turn) => turn.cancel_token.is_some(),
        None => mailbox.has_active_turn().await?,
    };
    if active {
        if let Some(turn) = snapshot.as_ref() {
            note(channel, Some(message), "active_turn", Some(turn));
        }
        return Ok(true);
    }
    // Without a registry this read considers only a durable external holder.
    let external = origin.is_person()
        && external_turn_hold_for_start(None, provider, channel)
            .await
            .is_some();
    if external && diagnose {
        note(channel, Some(message), "external_row_or_rest", None);
    }
    Ok(external)
}

pub(super) fn refusal(channel: u64, message: u64, site: &'static str) -> String {
    note(channel, Some(message), site, None);
    START_WAITS.into()
}

pub(super) fn routed_refusal(channel: u64, message: Option<u64>, reason: &str) -> String {
    let error = format!("NotSent({reason:?})");
    if error == START_WAITS {
        note(channel, message, "route_busy", None);
    }
    error
}

fn note(
    channel: u64,
    message: Option<u64>,
    site: &'static str,
    active: Option<&ChannelMailboxSnapshot>,
) {
    let user_msg_id = active.and_then(|turn| turn.active_user_message_id.map(|id| id.get()));
    let nonce = active.and_then(|turn| turn.active_turn_nonce.as_deref());
    let kind = active.map(|turn| turn.active_turn_kind);
    let started_at = active.and_then(|turn| turn.turn_started_at.as_ref());
    tracing::info!(
        target: TARGET,
        channel_id = channel,
        message_id = message,
        site,
        error = START_WAITS,
        ?user_msg_id,
        ?nonce,
        ?kind,
        ?started_at,
        "owner start waits for idle"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::inflight::{InflightTurnState, TurnSource, save_inflight_state};
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn owner_start_wait_site_events_keep_the_wire_error_and_active_identity() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let registry = ChannelMailboxRegistry::default();
        let mailbox = registry.handle(ChannelId::new(31));
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "fixture-episode".into(),
        )));
        mailbox
            .restore_active_turn(token, UserId::new(7), MessageId::new(41))
            .await;

        crate::logging::test_capture::pin_callsite_interest();
        let log = Log::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let _dispatch = tracing::subscriber::set_default(subscriber);
        let sites = [
            "boot_ready",
            "row_or_rest",
            "transcript_busy",
            "native_wait",
            "screen",
            "unnamed_target",
            "late_screen",
        ];
        for site in sites {
            assert_eq!(refusal(31, 51, site), START_WAITS);
        }
        assert_eq!(
            routed_refusal(31, Some(51), "synthetic_waits_idle"),
            START_WAITS
        );
        assert_eq!(
            routed_refusal(31, None, "remote_placement"),
            "NotSent(\"remote_placement\")"
        );
        assert!(
            active_turn(
                &mailbox,
                InputOrigin::Routine,
                &ProviderKind::Claude,
                31,
                51
            )
            .await
            .unwrap()
        );
        assert!(
            active_turn(
                &mailbox,
                InputOrigin::Discord,
                &ProviderKind::Claude,
                31,
                51
            )
            .await
            .unwrap()
        );
        let idle = registry.handle(ChannelId::new(32));
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            32,
            None,
            7,
            42,
            43,
            "external fixture".into(),
            Some("external-session".into()),
            None,
            None,
            None,
            0,
        );
        row.turn_source = TurnSource::ExternalInput;
        save_inflight_state(&row).unwrap();
        assert!(!idle.has_active_turn().await.unwrap());
        assert!(
            active_turn(&idle, InputOrigin::Decision, &ProviderKind::Claude, 32, 51)
                .await
                .unwrap()
        );
        assert!(
            active_turn(&idle, InputOrigin::Discord, &ProviderKind::Claude, 32, 51)
                .await
                .unwrap()
        );

        let bytes = log.0.lock().unwrap();
        let captured = std::str::from_utf8(&bytes).unwrap();
        let events: Vec<_> = captured
            .lines()
            .filter(|line| line.contains("owner start waits for idle"))
            .collect();
        assert_eq!(events.len(), sites.len() + 3);
        for (line, site) in events.iter().zip(sites.into_iter().chain([
            "route_busy",
            "active_turn",
            "external_row_or_rest",
        ])) {
            assert!(line.contains(&format!("site=\"{site}\"")), "{line}");
            assert!(
                line.contains("message_id=51") && line.contains("synthetic_waits_idle"),
                "{line}"
            );
        }
        let active = events[sites.len() + 1];
        assert!(active.contains("user_msg_id=Some(41)"), "{active}");
        assert!(
            active.contains("nonce=Some(\"fixture-episode\")"),
            "{active}"
        );
        assert!(active.contains("kind=Some(UserOrAgent)"), "{active}");
        assert!(active.contains("started_at=Some("), "{active}");
        let external = events.last().unwrap();
        assert!(external.contains("user_msg_id=None"), "{external}");
    }
}
