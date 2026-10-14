//! Channel-kind authority for optional panel surfaces. It lives beside, not inside,
//! the per-turn panel state so turn clears and session resets keep a known DM.

use dashmap::DashMap;
#[cfg(test)]
use poise::serenity_prelude::Http;
use poise::serenity_prelude::{Channel, ChannelId};

/// Ordered so that merging keeps any known kind and lets Dm win over Guild.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::services::discord) enum PanelChannelKind {
    #[default]
    Unknown,
    Guild,
    Dm,
}

impl PanelChannelKind {
    fn from_dm_hint(dm_hint: Option<bool>) -> Self {
        match dm_hint {
            Some(true) => Self::Dm,
            Some(false) => Self::Guild,
            None => Self::Unknown,
        }
    }

    /// A failed or unsupported lookup is not an observation.
    fn from_lookup(channel: Option<&Channel>) -> Self {
        match channel {
            Some(Channel::Private(_)) => Self::Dm,
            Some(Channel::Guild(_)) => Self::Guild,
            _ => Self::Unknown,
        }
    }

    #[cfg(test)]
    fn suppression(self) -> Option<PanelSuppression> {
        match self {
            Self::Guild => None,
            Self::Dm => Some(PanelSuppression::Dm),
            Self::Unknown => Some(PanelSuppression::UnknownChannelKind),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum PanelSuppression {
    Dm,
    UnknownChannelKind,
}

/// Panel content, or why it is withheld; a withheld panel is never an empty string.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum PanelRender<T> {
    Visible(T),
    Suppressed(PanelSuppression),
}

/// Never cleared: a channel's kind outlives its turns and sessions.
#[derive(Debug, Default)]
pub(super) struct PanelChannelKinds(DashMap<ChannelId, PanelChannelKind>);

impl PanelChannelKinds {
    fn record(&self, channel_id: ChannelId, observed: PanelChannelKind) {
        if observed == PanelChannelKind::Unknown {
            return;
        }
        let mut kind = self.0.entry(channel_id).or_default();
        *kind = (*kind).max(observed);
    }

    #[cfg(test)]
    fn get(&self, channel_id: ChannelId) -> PanelChannelKind {
        self.0
            .get(&channel_id)
            .map_or(PanelChannelKind::Unknown, |kind| *kind)
    }
}

impl super::PlaceholderLiveEvents {
    /// Records the gateway DM hint and a successful lookup; absent ones record nothing.
    pub(in crate::services::discord) fn observe_panel_channel_kind(
        &self,
        channel_id: ChannelId,
        dm_hint: Option<bool>,
        lookup: Option<&Channel>,
    ) {
        let observed =
            PanelChannelKind::from_dm_hint(dm_hint).max(PanelChannelKind::from_lookup(lookup));
        self.panel_channel_kinds.record(channel_id, observed);
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn panel_channel_kind(
        &self,
        channel_id: ChannelId,
    ) -> PanelChannelKind {
        self.panel_channel_kinds.get(channel_id)
    }

    /// Renders optional panel content only for a known guild channel.
    #[cfg(test)]
    pub(in crate::services::discord) fn gate_panel_render<T>(
        &self,
        channel_id: ChannelId,
        render: impl FnOnce() -> T,
    ) -> PanelRender<T> {
        match self.panel_channel_kind(channel_id).suppression() {
            None => PanelRender::Visible(render()),
            Some(reason) => PanelRender::Suppressed(reason),
        }
    }

    /// Fills an unknown kind with one lookup under the request-anchor time cap.
    #[cfg(test)]
    pub(in crate::services::discord) async fn resolve_panel_channel_kind(
        &self,
        http: &Http,
        channel_id: ChannelId,
    ) -> PanelChannelKind {
        self.resolve_panel_channel_kind_within(
            http,
            channel_id,
            super::turn_anchor::CHANNEL_LOOKUP_TIMEOUT,
        )
        .await
    }

    /// No map guard is held across the await, and the kind is re-read after it.
    #[cfg(test)]
    async fn resolve_panel_channel_kind_within(
        &self,
        http: &Http,
        channel_id: ChannelId,
        limit: std::time::Duration,
    ) -> PanelChannelKind {
        if self.panel_channel_kind(channel_id) == PanelChannelKind::Unknown
            && let Ok(Ok(channel)) = tokio::time::timeout(limit, channel_id.to_channel(http)).await
        {
            self.observe_panel_channel_kind(channel_id, None, Some(&channel));
        }
        self.panel_channel_kind(channel_id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Json;
    use axum::http::{StatusCode, Uri};
    use poise::serenity_prelude::{self as serenity, Channel, ChannelId};
    use serde_json::{Value, json};

    use super::super::PlaceholderLiveEvents;
    use super::{PanelChannelKind, PanelRender, PanelSuppression};
    use crate::services::agent_protocol::StatusEvent;
    use crate::services::provider::ProviderKind;

    const REAL_USER_MSG_ID: u64 = 1_520_312_799_245_504_542;
    const LOOKUP_CAP: Duration = Duration::from_millis(200);

    #[derive(Clone, Copy)]
    enum Wire {
        Dm,
        Guild,
        PrivateThread,
        DelayedGuild,
        ServerError,
        Hang,
    }

    fn channel_json(id: &str, kind: u8) -> Value {
        json!({
            "id": id, "type": kind, "name": "panel-kind", "guild_id": "42000",
            "position": 0, "permission_overwrites": [], "nsfw": false, "parent_id": "42002",
            "recipients": [{"id": "42010", "username": "kind-test", "discriminator": "0", "avatar": null}],
            "thread_metadata": {"archived": false, "auto_archive_duration": 60,
                "archive_timestamp": "2026-09-22T00:00:00Z", "locked": false}
        })
    }

    fn channel(kind: u8) -> Channel {
        serde_json::from_value(channel_json("5708", kind)).expect("channel fixture")
    }

    /// Discord REST stand-in that answers every channel lookup the same way and counts them.
    async fn lookup_server(
        wire: Wire,
    ) -> (
        Arc<serenity::Http>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let app = axum::Router::new().fallback(move |uri: Uri| {
            seen.fetch_add(1, Ordering::SeqCst);
            async move {
                let id = uri
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let kind = match wire {
                    Wire::Dm => 1,
                    Wire::Guild => 0,
                    Wire::PrivateThread => 12,
                    Wire::DelayedGuild => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        0
                    }
                    Wire::ServerError => {
                        let body = json!({"code": 0, "message": "injected lookup failure"});
                        return (StatusCode::INTERNAL_SERVER_ERROR, Json(body));
                    }
                    Wire::Hang => return std::future::pending().await,
                };
                (StatusCode::OK, Json(channel_json(&id, kind)))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("panel-kind-test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (http, count, server)
    }

    fn must_not_render() -> String {
        panic!("a withheld panel must not be rendered")
    }

    #[tokio::test]
    async fn a_dm_hint_survives_failed_lookups_clears_and_a_new_session() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(5_708_001);
        let session = |key: &str| {
            events.set_session_panel_lifecycle_event(
                channel_id,
                Some(key),
                "session_resumed",
                &json!({ "provider_session_id": key, "tmux_reused": true }),
            )
        };
        assert!(session("AgentDesk-claude-dm#1"));
        // Intake: the gateway hint says DM while its own channel lookup failed.
        events.observe_panel_channel_kind(channel_id, Some(true), None);

        let (http, lookups, server) = lookup_server(Wire::ServerError).await;
        events
            .resolve_turn_request_anchor(&http, channel_id, Some(REAL_USER_MSG_ID))
            .await;
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            1,
            "the anchor lookup ran and failed"
        );
        assert_eq!(
            events.request_user_msg_id_for_test(channel_id),
            Some(REAL_USER_MSG_ID)
        );

        assert!(session("AgentDesk-claude-dm#2"));
        assert_eq!(
            events.request_user_msg_id_for_test(channel_id),
            None,
            "the new session reset the turn-scoped request anchor"
        );
        events.clear_channel_preserving_footer_residuals(channel_id);
        events.clear_channel(channel_id);
        server.abort();

        assert_eq!(events.panel_channel_kind(channel_id), PanelChannelKind::Dm);
        assert_eq!(
            events.gate_panel_render(channel_id, must_not_render),
            PanelRender::Suppressed(PanelSuppression::Dm)
        );
    }

    #[tokio::test]
    async fn a_failed_lookup_is_not_a_guild_observation() {
        let events = PlaceholderLiveEvents::default();
        let unknown = ChannelId::new(5_708_011);
        let anchored = ChannelId::new(5_708_012);
        let guild = ChannelId::new(5_708_013);
        let (http, lookups, server) = lookup_server(Wire::ServerError).await;

        events.observe_panel_channel_kind(unknown, None, None);
        assert_eq!(
            events
                .resolve_panel_channel_kind_within(&http, unknown, LOOKUP_CAP)
                .await,
            PanelChannelKind::Unknown
        );
        events
            .resolve_turn_request_anchor(&http, anchored, Some(REAL_USER_MSG_ID))
            .await;
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            2,
            "both lookups ran and failed"
        );
        assert_eq!(
            events.panel_channel_kind(anchored),
            PanelChannelKind::Unknown
        );
        assert_eq!(
            events.gate_panel_render(unknown, must_not_render),
            PanelRender::Suppressed(PanelSuppression::UnknownChannelKind)
        );

        events.observe_panel_channel_kind(guild, Some(false), None);
        assert_eq!(
            events
                .resolve_panel_channel_kind_within(&http, guild, LOOKUP_CAP)
                .await,
            PanelChannelKind::Guild
        );
        events.clear_channel(guild);
        server.abort();
        assert_eq!(
            events.gate_panel_render(guild, || "panel".to_string()),
            PanelRender::Visible("panel".to_string()),
            "a known guild keeps its panel through a failed lookup and a clear"
        );
    }

    fn dm_hint(events: &PlaceholderLiveEvents, id: ChannelId) {
        events.observe_panel_channel_kind(id, Some(true), None);
    }

    fn guild_lookup(events: &PlaceholderLiveEvents, id: ChannelId) {
        events.observe_panel_channel_kind(id, None, Some(&channel(0)));
    }

    fn failed_lookup(events: &PlaceholderLiveEvents, id: ChannelId) {
        events.observe_panel_channel_kind(id, None, None);
    }

    #[test]
    fn merged_observations_keep_dm_in_any_order() {
        type Observe = fn(&PlaceholderLiveEvents, ChannelId);
        let steps: [Observe; 3] = [dm_hint, guild_lookup, failed_lookup];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let events = PlaceholderLiveEvents::default();
            let id = ChannelId::new(5_708_021);
            for step in order {
                steps[step](&events, id);
            }
            assert_eq!(
                events.panel_channel_kind(id),
                PanelChannelKind::Dm,
                "{order:?}"
            );
        }

        let events = PlaceholderLiveEvents::default();
        let thread = ChannelId::new(5_708_022);
        events.observe_panel_channel_kind(thread, None, Some(&channel(12)));
        assert_eq!(
            events.panel_channel_kind(thread),
            PanelChannelKind::Guild,
            "a guild private thread is a guild channel"
        );
        let contradicted = ChannelId::new(5_708_023);
        events.observe_panel_channel_kind(contradicted, Some(false), Some(&channel(1)));
        assert_eq!(
            events.panel_channel_kind(contradicted),
            PanelChannelKind::Dm
        );
    }

    #[tokio::test]
    async fn the_resolver_looks_up_only_unknown_channels() {
        let events = PlaceholderLiveEvents::default();
        let known = ChannelId::new(5_708_031);
        events.observe_panel_channel_kind(known, Some(true), None);
        let (http, lookups, server) = lookup_server(Wire::Guild).await;
        assert_eq!(
            events.resolve_panel_channel_kind(&http, known).await,
            PanelChannelKind::Dm
        );
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            0,
            "a known kind needs no lookup"
        );
        server.abort();

        for (wire, expected) in [
            (Wire::Dm, PanelChannelKind::Dm),
            (Wire::PrivateThread, PanelChannelKind::Guild),
        ] {
            let (http, lookups, server) = lookup_server(wire).await;
            let unknown = ChannelId::new(5_708_032);
            let events = PlaceholderLiveEvents::default();
            assert_eq!(
                events.resolve_panel_channel_kind(&http, unknown).await,
                expected
            );
            assert_eq!(lookups.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_dm_hint_that_lands_during_a_lookup_outlives_its_expiry_and_its_answer() {
        for wire in [Wire::Hang, Wire::DelayedGuild] {
            let events = PlaceholderLiveEvents::default();
            let channel_id = ChannelId::new(5_708_041);
            let (http, lookups, server) = lookup_server(wire).await;
            let (resolved, ()) = tokio::join!(
                events.resolve_panel_channel_kind_within(&http, channel_id, LOOKUP_CAP),
                async {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    events.clear_channel(channel_id);
                    events.observe_panel_channel_kind(channel_id, Some(true), None);
                }
            );
            assert_eq!(lookups.load(Ordering::SeqCst), 1);
            assert_eq!(resolved, PanelChannelKind::Dm);
            assert_eq!(events.panel_channel_kind(channel_id), PanelChannelKind::Dm);
            server.abort();
        }
    }

    #[tokio::test]
    async fn the_request_anchor_lookup_records_the_kind_it_saw() {
        for (wire, kind, guild) in [
            (Wire::Dm, PanelChannelKind::Dm, "@me"),
            (Wire::PrivateThread, PanelChannelKind::Guild, "42000"),
        ] {
            let events = PlaceholderLiveEvents::default();
            let channel_id = ChannelId::new(5_708_051);
            let (http, _, server) = lookup_server(wire).await;
            events
                .resolve_turn_request_anchor(&http, channel_id, Some(REAL_USER_MSG_ID))
                .await;
            server.abort();
            assert_eq!(events.panel_channel_kind(channel_id), kind);
            let snapshot = events
                .status_by_channel
                .get(&channel_id)
                .expect("anchor state")
                .lock()
                .unwrap()
                .clone();
            let link = events
                .request_anchor_line(channel_id, &snapshot)
                .expect("request link");
            assert!(
                link.contains(&format!("/channels/{guild}/{}/", channel_id.get())),
                "{link}"
            );
        }
    }

    #[test]
    fn existing_renderers_ignore_the_recorded_kind() {
        let events = PlaceholderLiveEvents::default();
        let channel_id = ChannelId::new(5_708_061);
        events.push_status_event(
            channel_id,
            StatusEvent::ToolStart {
                name: "Bash".to_string(),
                args_summary: Some("cargo test".to_string()),
            },
        );
        let render = || {
            (
                events.render_status_panel(channel_id, &ProviderKind::Claude, 1_700_000_000),
                events.render_completion_footer(channel_id, &ProviderKind::Claude, "✅"),
            )
        };
        let before = render();
        assert!(!before.0.is_empty());
        for observed in [Some(true), Some(false)] {
            events.observe_panel_channel_kind(channel_id, observed, None);
            assert_eq!(
                render(),
                before,
                "the dormant kind must not change rendered bytes"
            );
        }
    }
}
