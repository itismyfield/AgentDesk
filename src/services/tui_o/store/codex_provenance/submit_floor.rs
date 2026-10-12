//! Dormant submit coordinates; reading a floor never grants input or delivery permission.

use super::{CodexEpisodeSpan, CodexProvenance, ExecutionProofRef, TurnEpisodeRef, whole};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::same_file;
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::codex_verified::provenance::permission::NativeSubmitFloor;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedLaunch {
    #[serde(deserialize_with = "strict_context")]
    pub context: BindingContext,
    pub episode: TurnEpisodeRef,
}

impl PreparedLaunch {
    fn valid(&self, channel: u64) -> bool {
        let (context, episode) = (&self.context, &self.episode);
        channel != 0
            && context.schema == 1
            && context.provider == "codex"
            && context.source_policy.as_deref() == Some("verified")
            && context.channel_id == Some(channel)
            && matches!(context.launch_mode.as_str(), "fresh" | "resume")
            && !context.execution_nonce.trim().is_empty()
            && !context.tmux_session.trim().is_empty()
            && !context.owner_runtime_root.trim().is_empty()
            && context
                .first_prompt_digest
                .as_deref()
                .is_some_and(|digest| digest.strip_prefix("sha256:").is_some_and(hash))
            && episode.channel_id == channel
            && episode.user_message_id != 0
            && episode.request_owner_id != 0
            && !episode.turn_nonce.trim().is_empty()
            && match context.launch_mode.as_str() {
                "fresh" => context.expected_native_session_id.is_none(),
                _ => context
                    .expected_native_session_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty()),
            }
    }
}

fn strict_context<'de, D: serde::Deserializer<'de>>(reader: D) -> Result<BindingContext, D::Error> {
    let value = serde_json::Value::deserialize(reader)?;
    let context: BindingContext =
        serde_json::from_value(value.clone()).map_err(serde::de::Error::custom)?;
    let model = serde_json::to_value(&context).map_err(serde::de::Error::custom)?;
    if !known_fields(&value, &model) {
        return Err(serde::de::Error::custom("unknown prepared context field"));
    }
    Ok(context)
}

pub(super) fn known_fields(read: &serde_json::Value, model: &serde_json::Value) -> bool {
    use serde_json::Value::{Array, Object};
    match (read, model) {
        (Object(read), Object(model)) => read.iter().all(|(key, value)| {
            model
                .get(key)
                .is_some_and(|expected| known_fields(value, expected))
        }),
        (Array(read), Array(model)) => read.iter().zip(model).all(|(a, b)| known_fields(a, b)),
        _ => true,
    }
}

fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeFloor {
    pub launch: PreparedLaunch,
    pub execution: ExecutionProofRef,
    pub canonical_source: SourceId,
    pub through: u64,
    pub prefix_hash: String,
    pub generation_mtime_ns: i64,
}

impl NativeFloor {
    pub(crate) fn capture(
        launch: PreparedLaunch,
        execution: ExecutionProofRef,
        cursor: &Cursor,
        generation_mtime_ns: i64,
    ) -> Result<Self, &'static str> {
        let floor = NativeSubmitFloor::capture_before_submit(&execution, cursor)?;
        let (source, through, prefix_hash) = floor.checkpoint();
        let captured = Self {
            launch,
            execution,
            canonical_source: source.clone(),
            through,
            prefix_hash: prefix_hash.to_owned(),
            generation_mtime_ns,
        };
        captured
            .valid(captured.launch.episode.channel_id)
            .then_some(captured)
            .ok_or("native_floor_identity")
    }

    fn valid(&self, channel: u64) -> bool {
        let context = &self.launch.context;
        self.launch.valid(channel)
            && whole(channel, &self.launch.episode, &self.execution)
            && self.execution.proof_seq != 0
            && self.execution.execution_nonce == context.execution_nonce
            && self.execution.owner_runtime_root == context.owner_runtime_root
            && self.execution.tmux_session == context.tmux_session
            && context
                .expected_native_session_id
                .as_ref()
                .is_none_or(|id| id == &self.execution.source.session_id)
            && same_file(&self.canonical_source, &self.execution.source)
            && hash(&self.prefix_hash)
            && self.generation_mtime_ns > 0
    }

    pub(super) fn names(&self, span: &CodexEpisodeSpan) -> bool {
        let mut episode = span.episode.clone();
        episode.native_turn_id = self.launch.episode.native_turn_id.clone();
        span.execution == self.execution
            && episode == self.launch.episode
            && span.start >= self.through
            && span.generation_mtime_ns == self.generation_mtime_ns
            && span.end.is_none_or(|end| end > span.start)
            && (self.launch.episode.native_turn_id.is_empty()
                || self.launch.episode.native_turn_id == span.episode.native_turn_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum SubmitFloor {
    PreparedLaunch(Box<PreparedLaunch>),
    NativeFloor(Box<NativeFloor>),
}

impl SubmitFloor {
    pub(super) fn launch(&self) -> &PreparedLaunch {
        match self {
            Self::PreparedLaunch(launch) => launch,
            Self::NativeFloor(floor) => &floor.launch,
        }
    }

    pub(super) fn same_episode(&self, other: &Self) -> bool {
        let (a, b) = (&self.launch().episode, &other.launch().episode);
        (a.channel_id, a.user_message_id, &a.turn_nonce)
            == (b.channel_id, b.user_message_id, &b.turn_nonce)
    }

    pub(super) fn retains(&self, old: &Self) -> bool {
        self == old || (matches!(old, Self::PreparedLaunch(_)) && self.launch() == old.launch())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Schema2 {
    pub schema: u32,
    pub channel: u64,
    pub spans: Vec<CodexEpisodeSpan>,
    pub denies: Vec<super::CodexEpisodeDeny>,
    pub floors: Vec<SubmitFloor>,
}

impl Schema2 {
    pub(super) fn base(&self) -> CodexProvenance {
        CodexProvenance {
            schema: super::SCHEMA,
            channel: self.channel,
            spans: self.spans.clone(),
            denies: self.denies.clone(),
        }
    }

    pub(super) fn valid(&self, channel: u64) -> bool {
        self.schema == 2 && self.channel == channel && super::valid(channel, &self.base())
            && self.floors.iter().enumerate().all(|(index, floor)| {
                let valid = match floor {
                    SubmitFloor::PreparedLaunch(launch) => launch.valid(channel),
                    SubmitFloor::NativeFloor(floor) => floor.valid(channel),
                };
                valid && self.floors[..index].iter().all(|old| !floor.same_episode(old))
                    && self.spans.iter().filter(|span| {
                        span.episode.user_message_id == floor.launch().episode.user_message_id
                            && span.episode.turn_nonce == floor.launch().episode.turn_nonce
                    }).all(|span| matches!(floor, SubmitFloor::NativeFloor(native) if native.names(span)))
            })
    }
}

pub(super) fn read(store: &super::ChannelStore) -> Option<super::Sidecar> {
    let value = super::durable::read_json::<serde_json::Value>(
        &store.dir.join(super::CODEX_PROVENANCE_FILE),
    )
    .ok()??;
    if value["schema"] != 2 || value.get("floors").is_none() {
        return None;
    }
    if store.io_failure().is_some() {
        return Some(super::Sidecar::Unavailable("codex_provenance_failed"));
    }
    let document = match serde_json::from_value::<Schema2>(value.clone()) {
        Ok(document) => document,
        Err(_) => return Some(super::Sidecar::Unavailable("codex_provenance_unreadable")),
    };
    if !known_fields(&value, &serde_json::to_value(&document).ok()?) {
        return Some(super::Sidecar::Unavailable("codex_provenance_unreadable"));
    }
    if document.channel != store.init.channel {
        return Some(super::Sidecar::Unavailable("codex_provenance_identity"));
    }
    if !document.valid(store.init.channel) {
        return Some(super::Sidecar::Unavailable("codex_provenance_conflict"));
    }
    Some(super::Sidecar::Present(super::ValidatedCodexProvenance(
        document.base(),
        Some(document),
    )))
}

/// Only the store's successful publication and validated read-back constructs this value.
#[derive(Debug)]
pub(crate) struct DurableClosedEpisode {
    span: CodexEpisodeSpan,
}

impl DurableClosedEpisode {
    pub(super) fn new(span: CodexEpisodeSpan) -> Self {
        Self { span }
    }

    pub(crate) fn span(&self) -> &CodexEpisodeSpan {
        &self.span
    }
}

#[cfg(unix)]
#[path = "submit_floor_tests.rs"]
pub(crate) mod tests;
