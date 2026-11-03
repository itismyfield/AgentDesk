//! Output provenance is observation metadata, never delivery or claim authority.
use super::{census_scope, current_census};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{InflightTurnState, PersistedHostLocator};
use crate::services::tui_o::{channel_policy::ClaimReason, cutover::BodySite};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    future::Future,
    sync::{LazyLock, Mutex},
};

#[derive(Clone)]
pub(in crate::services::discord) struct BodySource(Value, bool);

// Only non-nonce POST ACKs establish an original Legacy creator; the bounded proof is process-local.
static LEGACY_CREATES: LazyLock<Mutex<VecDeque<(u64, u64)>>> = LazyLock::new(Mutex::default);

pub(in crate::services::discord) fn note_legacy_created(channel: u64, message: u64) {
    if !current_census().is_some_and(|tag| tag["body_owner"] == "Legacy") {
        return;
    }
    let Ok(mut created) = LEGACY_CREATES.try_lock() else {
        return;
    };
    if !created.contains(&(channel, message)) {
        created.push_back((channel, message));
        if created.len() > 512 {
            created.pop_front();
        }
    }
}

pub(in crate::services::discord) fn created_by_legacy(channel: u64, message: u64) -> bool {
    LEGACY_CREATES
        .try_lock()
        .is_ok_and(|created| created.contains(&(channel, message)))
}

#[derive(Clone, Copy)]
pub(in crate::services::discord) enum BodyOffer {
    Body,
    Recovered,
    NonBody,
    Unknown,
    Refresh,
}

impl BodyOffer {
    pub(in crate::services::discord) fn body(carries_body: bool) -> Self {
        if carries_body {
            Self::Body
        } else {
            Self::NonBody
        }
    }

    pub(in crate::services::discord) fn refresh(
        carries_body: bool,
        legacy_body_confirmed: bool,
    ) -> Self {
        if !carries_body {
            Self::NonBody
        } else if legacy_body_confirmed {
            Self::Refresh
        } else {
            Self::Unknown
        }
    }

    pub(in crate::services::discord) fn recovered(carries_body: bool) -> Self {
        if carries_body {
            Self::Recovered
        } else {
            Self::NonBody
        }
    }
}

impl BodySource {
    pub(in crate::services::discord) fn inherited() -> Self {
        Self(
            json!({"binding_mode":"UNKNOWN", "actual_runtime":"unknown",
            "session_temperature":"unknown", "source_incarnation":null,
            "producer_host":null, "input_origin":"unknown", "provider":null,
            "body_owner":"Legacy"}),
            true,
        )
    }

    pub(in crate::services::discord) fn captured(tag: Option<Value>) -> Self {
        let tag = tag.filter(Value::is_object);
        let unknown = tag.as_ref().is_none_or(|tag| {
            tag["output_class"] == "UnknownBody"
                || !matches!(tag["binding_mode"].as_str(), Some("TUI" | "NON_TUI"))
                || tag["body_owner"] != "Legacy"
        });
        let mut source = Self(tag.unwrap_or_else(|| Self::inherited().0), false);
        let object = source
            .0
            .as_object_mut()
            .expect("source metadata is an object");
        object.remove("site");
        object.remove("output_class");
        if unknown {
            object.insert("output_class".into(), json!("UnknownBody"));
        }
        source
    }

    pub(in crate::services::discord) fn read_source(
        mut self,
        file: Option<(u64, u64)>,
        generation: i64,
        reset: u64,
    ) -> Self {
        self.0["read_source"] =
            json!({"file":file,"generation":generation,"reset_incarnation":reset});
        if file.is_some() && generation > 0 {
            let mut incarnation = self.0["source_incarnation"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            incarnation.insert("file".into(), json!(file));
            incarnation.insert("generation".into(), json!(generation));
            incarnation.insert("reset_incarnation".into(), json!(reset));
            self.0["source_incarnation"] = Value::Object(incarnation);
        } else if self.0["source_incarnation"]["hosted_record"].is_null() {
            self.0["source_incarnation"] = Value::Null;
        }
        self
    }

    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        let binding = match state
            .runtime_kind
            .filter(|_| !state.runtime_kind_unknown_on_disk)
        {
            Some(RuntimeHandoffKind::ClaudeTui | RuntimeHandoffKind::CodexTui) => "TUI",
            Some(RuntimeHandoffKind::LegacyTmuxWrapper | RuntimeHandoffKind::ProcessBackend) => {
                "NON_TUI"
            }
            None => "UNKNOWN",
        };
        let (runtime, host) = match state.host_locator.as_ref() {
            Some(PersistedHostLocator::Known(locator)) => (
                locator.host_kind.as_str(),
                locator.execution_node.as_deref(),
            ),
            _ => ("unknown", None),
        };
        let incarnation = match (
            state.hosted_record_id.filter(|id| *id > 0),
            state
                .hosted_execution_nonce
                .as_deref()
                .filter(|nonce| !nonce.trim().is_empty()),
        ) {
            (Some(record), Some(nonce)) => json!({"hosted_record":record,
                "execution_digest":hex::encode(Sha256::digest(nonce.as_bytes()))}),
            _ => match (
                state.tui_terminal_source_file_identity,
                state.tui_terminal_generation_mtime_ns,
            ) {
                (Some(file), Some(generation)) if generation > 0 => {
                    json!({"file":file,"generation":generation})
                }
                _ => Value::Null,
            },
        };
        let mut source = Self::inherited();
        source.0["binding_mode"] = json!(binding);
        source.0["actual_runtime"] = json!(runtime);
        source.0["producer_host"] = json!(host);
        source.0["source_incarnation"] = incarnation;
        source.0["input_origin"] = json!(state.turn_source);
        source.0["provider"] = json!(state.provider);
        source
    }

    pub(in crate::services::discord) fn reason(mut self, reason: ClaimReason) -> Self {
        self.0["reason"] = json!(reason);
        self
    }

    fn inherited_tag(self) -> Value {
        let mut tag = self.0;
        if self.1
            && let Some(Value::Object(outer)) = current_census()
        {
            tag.as_object_mut()
                .expect("source metadata is an object")
                .extend(outer);
        }
        tag
    }

    /// Carry saved source facts through an adapter before its transport chooses body or notice.
    pub(in crate::services::discord) async fn scope<T>(self, next: impl Future<Output = T>) -> T {
        census_scope(Some(self.inherited_tag()), next).await
    }

    pub(in crate::services::discord) async fn observe<T>(
        self,
        site: BodySite,
        offer: BodyOffer,
        next: impl Future<Output = T>,
    ) -> T {
        let mut tag = self.inherited_tag();
        let object = tag.as_object_mut().expect("source metadata is an object");
        object.entry("site").or_insert_with(|| json!(site));
        let class = match offer {
            BodyOffer::Body => "LegacyBody",
            BodyOffer::Recovered => "RecoveredLegacyBody",
            BodyOffer::NonBody => "NonBody",
            BodyOffer::Unknown => "UnknownBody",
            BodyOffer::Refresh => "LegacyBodyRefresh",
        };
        // Explicit notices mask an outer body; nested body helpers retain that mask.
        if matches!(offer, BodyOffer::NonBody) {
            object.insert("output_class".into(), json!(class));
        } else {
            object.entry("output_class").or_insert_with(|| json!(class));
        }
        census_scope(Some(tag), next).await
    }
}

pub(in crate::services::discord) async fn forward<T>(next: impl Future<Output = T>) -> T {
    census_scope(current_census(), next).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[tokio::test]
    async fn source_metadata_keeps_wire_sites_unknowns_and_outer_provenance() {
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            77,
            None,
            1,
            2,
            3,
            "input".into(),
            Some("a-session-is-not-warm-proof".into()),
            None,
            None,
            None,
            0,
        );
        for (site, wire) in [
            (BodySite::WatcherTerminal, "W01"),
            (BodySite::WatcherDirectFallback, "W01a"),
            (BodySite::WatcherReplace, "W01b"),
            (BodySite::WatcherPostFallback, "W01c"),
            (BodySite::WatcherShortController, "W01d"),
            (BodySite::WatcherLongController, "W01e"),
            (BodySite::WatcherLongChunks, "W01f"),
            (BodySite::WatcherStreaming, "W02"),
            (BodySite::WatcherProviderGuard, "W02b"),
            (BodySite::FooterInitial, "FOOTER-INITIAL"),
            (BodySite::FooterStrip, "FOOTER-STRIP"),
            (BodySite::FooterRefresh, "FOOTER-REFRESH"),
            (BodySite::BridgeTerminal, "W10"),
            (BodySite::BridgeShortController, "W10b"),
            (BodySite::BridgeLongController, "W10c"),
            (BodySite::BridgeLongChunks, "W10d"),
            (BodySite::BridgePinnedTerminal, "W10e"),
            (BodySite::BridgeCancelled, "W11"),
            (BodySite::BridgeForeignTerminal, "W13"),
            (BodySite::BridgeStreaming, "W14"),
            (BodySite::BridgeRollover, "W14a"),
            (BodySite::BridgeRolloverRestore, "W14b"),
            (BodySite::BridgeBlockedRollover, "W14c"),
            (BodySite::OutputController, "W19"),
            (BodySite::RecoveryNotice, "W30"),
            (BodySite::RecoveryController, "W30a"),
            (BodySite::RecoveredTerminal, "W31"),
            (BodySite::RecoveryChunks, "W32"),
            (BodySite::HealthRecovery, "W33"),
            (BodySite::RestartHandoff, "W34"),
            (BodySite::RestartAnchor, "W35"),
        ] {
            let tag = BodySource::from_state(&row)
                .observe(site, BodyOffer::Body, async {
                    tokio::task::yield_now().await;
                    current_census().unwrap()
                })
                .await;
            assert_eq!(tag["site"], wire);
            assert_eq!(tag["binding_mode"], "UNKNOWN");
            assert_eq!(tag["actual_runtime"], "unknown");
            assert_eq!(tag["session_temperature"], "unknown");
            assert_eq!(tag["source_incarnation"], Value::Null);
        }
        row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        row.runtime_kind_unknown_on_disk = true;
        row.hosted_record_id = Some(1);
        row.hosted_execution_nonce = Some("".into());
        let tag = BodySource::from_state(&row)
            .scope(async { current_census().unwrap() })
            .await;
        assert_eq!(tag["binding_mode"], "UNKNOWN");
        assert_eq!(tag["source_incarnation"], Value::Null);
        row.runtime_kind_unknown_on_disk = false;
        row.tui_terminal_source_file_identity = Some((12, 34));
        row.tui_terminal_generation_mtime_ns = Some(123);
        let outer = BodySource::from_state(&row).reason(ClaimReason::Released);
        for (offer, expected) in [
            (BodyOffer::Recovered, "RecoveredLegacyBody"),
            (BodyOffer::NonBody, "NonBody"),
        ] {
            let tag = outer
                .clone()
                .observe(BodySite::RestartAnchor, offer, async {
                    BodySource::inherited()
                        .reason(ClaimReason::NoClaim)
                        .observe(BodySite::OutputController, BodyOffer::Body, async {
                            current_census().unwrap()
                        })
                        .await
                })
                .await;
            assert_eq!(tag["site"], "W35");
            assert_eq!(tag["reason"], "RELEASED");
            assert_eq!(tag["binding_mode"], "TUI");
            assert_eq!(
                tag["source_incarnation"],
                json!({"file":[12,34],"generation":123})
            );
            assert_eq!(tag["output_class"], expected);
        }
    }
}
