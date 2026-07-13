use super::*;

pub(in crate::services::discord) fn synthetic_thread_channel_name(
    parent_name: &str,
    channel_id: ChannelId,
) -> String {
    format!("{parent_name}-t{}", channel_id.get())
}

pub(super) fn is_synthetic_thread_channel_name(channel_name: &str, channel_id: ChannelId) -> bool {
    channel_name.ends_with(&format!("-t{}", channel_id.get()))
}

pub(super) fn choose_restore_channel_name(
    existing_channel_name: Option<&str>,
    live_channel_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<String>)>,
    channel_id: ChannelId,
) -> Option<String> {
    if let Some(existing_name) = existing_channel_name
        && is_synthetic_thread_channel_name(existing_name, channel_id)
    {
        return Some(existing_name.to_string());
    }

    if let Some((parent_id, parent_name)) = thread_parent {
        let parent_name = parent_name.unwrap_or_else(|| parent_id.get().to_string());
        return Some(synthetic_thread_channel_name(&parent_name, channel_id));
    }

    live_channel_name
        .or(existing_channel_name)
        .map(ToOwned::to_owned)
}

pub(in crate::services::discord) fn resolve_is_dm_channel(
    dm_hint: Option<bool>,
    live_channel_lookup_says_dm: bool,
) -> bool {
    // Prefer the gateway-provided DM hint when available so a transient
    // Discord channel lookup failure cannot disable DM default-agent fallback.
    dm_hint.unwrap_or(live_channel_lookup_says_dm)
}

/// Resolve the channel name and parent category name for a Discord channel.
///
/// `cache` is an optional optimization: when present (leader-side), category
/// names are looked up via the in-memory guild cache and avoid an extra REST
/// hop. Worker-side callers without a live shard pass `None` and pay the
/// REST fallback at line ~978 instead. Correctness is identical either way.
pub(in crate::services::discord) async fn resolve_channel_category(
    http: &Arc<serenity::http::Http>,
    cache: Option<&Arc<serenity::cache::Cache>>,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = cache.and_then(|c| {
            c.guild(gc.guild_id).and_then(|guild| {
                guild
                    .channels
                    .get(&parent_id)
                    .map(|parent_ch| parent_ch.name.clone())
            })
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
        None
    };
    (ch_name, cat_name)
}

pub(in crate::services::discord) async fn validate_live_channel_routing_with_dm_hint(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
    is_dm_hint: Option<bool>,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    let (live_is_dm, channel_name, thread_parent) =
        resolve_live_channel_routing_metadata(&ctx.http, channel_id).await;
    let is_dm = is_dm_hint.unwrap_or(live_is_dm);
    settings::validate_bot_channel_routing_with_thread_parent(
        settings,
        provider,
        channel_id,
        channel_name.as_deref(),
        thread_parent
            .as_ref()
            .map(|(parent_id, parent_name)| (*parent_id, parent_name.as_deref())),
        is_dm,
    )
}

pub(in crate::services::discord) async fn validate_live_channel_routing(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    validate_live_channel_routing_with_dm_hint(ctx, provider, settings, channel_id, None).await
}

pub(in crate::services::discord) async fn provider_handles_channel(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> bool {
    validate_live_channel_routing(ctx, provider, settings, channel_id)
        .await
        .is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum RuntimeChannelBindingStatus {
    Owned,
    Unowned,
    Unknown,
}

/// Whether a missing child session may be created from a parent path.
///
/// Denial variants remain distinct so an unavailable metadata lookup cannot be
/// mistaken for the narrowly approved legacy unowned-session escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ParentBootstrapDisposition {
    InheritedBinding,
    DirectSessionEscape,
    DenyDirect,
    DenyOptedOut,
    DenyUnowned,
    DenyUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct RuntimeChannelIdentity {
    channel_id: serenity::model::id::ChannelId,
    channel_name: Option<String>,
}

impl RuntimeChannelIdentity {
    fn new(channel_id: serenity::model::id::ChannelId, channel_name: Option<String>) -> Self {
        Self {
            channel_id,
            channel_name,
        }
    }

    pub(in crate::services::discord) fn channel_id(&self) -> serenity::model::id::ChannelId {
        self.channel_id
    }

    pub(in crate::services::discord) fn channel_name(&self) -> Option<&str> {
        self.channel_name.as_deref()
    }
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) enum RuntimeBindingAuthority {
    Direct {
        identity: RuntimeChannelIdentity,
        payload: settings::ConfiguredBindingPayload,
    },
    InheritedParent {
        identity: RuntimeChannelIdentity,
        payload: settings::ConfiguredBindingPayload,
    },
}

impl RuntimeBindingAuthority {
    pub(in crate::services::discord) fn payload(&self) -> &settings::ConfiguredBindingPayload {
        match self {
            Self::Direct { payload, .. } | Self::InheritedParent { payload, .. } => payload,
        }
    }

    pub(in crate::services::discord) fn identity(&self) -> &RuntimeChannelIdentity {
        match self {
            Self::Direct { identity, .. } | Self::InheritedParent { identity, .. } => identity,
        }
    }

    pub(in crate::services::discord) fn is_direct(&self) -> bool {
        matches!(self, Self::Direct { .. })
    }

    pub(in crate::services::discord) fn is_inherited_parent(&self) -> bool {
        matches!(self, Self::InheritedParent { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeMetadataState {
    Complete,
    Unknown,
    DirectMessage,
}

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct RuntimeChannelBindingResolution {
    live_child: RuntimeChannelIdentity,
    parent: Option<RuntimeChannelIdentity>,
    authority: Option<RuntimeBindingAuthority>,
    parent_inheritance_opted_out: bool,
    metadata_state: RuntimeMetadataState,
}

impl RuntimeChannelBindingResolution {
    pub(in crate::services::discord) fn direct_message(
        channel_id: serenity::model::id::ChannelId,
    ) -> Self {
        let identity = RuntimeChannelIdentity::new(channel_id, None);
        Self {
            live_child: identity.clone(),
            parent: None,
            authority: Some(RuntimeBindingAuthority::Direct {
                identity,
                payload: settings::ConfiguredBindingPayload {
                    role: None,
                    workspace: None,
                    thread_inherit: true,
                },
            }),
            parent_inheritance_opted_out: false,
            metadata_state: RuntimeMetadataState::DirectMessage,
        }
    }

    pub(in crate::services::discord) fn status(&self) -> RuntimeChannelBindingStatus {
        if self.authority.is_some() {
            RuntimeChannelBindingStatus::Owned
        } else if self.metadata_state == RuntimeMetadataState::Complete {
            RuntimeChannelBindingStatus::Unowned
        } else {
            RuntimeChannelBindingStatus::Unknown
        }
    }

    pub(in crate::services::discord) fn authority_channel_id(
        &self,
    ) -> serenity::model::id::ChannelId {
        self.authority
            .as_ref()
            .map(|authority| authority.identity().channel_id())
            .unwrap_or_else(|| self.live_child.channel_id())
    }

    pub(in crate::services::discord) fn authority_identity(
        &self,
    ) -> Option<&RuntimeChannelIdentity> {
        self.authority
            .as_ref()
            .map(RuntimeBindingAuthority::identity)
    }

    pub(in crate::services::discord) fn live_child(&self) -> &RuntimeChannelIdentity {
        &self.live_child
    }

    pub(in crate::services::discord) fn parent(&self) -> Option<&RuntimeChannelIdentity> {
        self.parent.as_ref()
    }

    pub(in crate::services::discord) fn authority(&self) -> Option<&RuntimeBindingAuthority> {
        self.authority.as_ref()
    }

    pub(in crate::services::discord) fn configured_workspace(&self) -> Option<&str> {
        self.authority()
            .and_then(|authority| authority.payload().workspace.as_deref())
    }

    pub(in crate::services::discord) fn thread_parent_tuple(
        &self,
    ) -> Option<(serenity::model::id::ChannelId, Option<&str>)> {
        self.parent()
            .map(|parent| (parent.channel_id(), parent.channel_name()))
    }

    pub(in crate::services::discord) fn is_direct_message(&self) -> bool {
        self.metadata_state == RuntimeMetadataState::DirectMessage
    }

    pub(in crate::services::discord) fn parent_bootstrap_disposition(
        &self,
    ) -> ParentBootstrapDisposition {
        match self.authority() {
            Some(RuntimeBindingAuthority::Direct { .. }) => ParentBootstrapDisposition::DenyDirect,
            Some(RuntimeBindingAuthority::InheritedParent { .. }) => {
                ParentBootstrapDisposition::InheritedBinding
            }
            None if self.parent_inheritance_opted_out => ParentBootstrapDisposition::DenyOptedOut,
            None if self.status() == RuntimeChannelBindingStatus::Unknown => {
                ParentBootstrapDisposition::DenyUnknown
            }
            None => ParentBootstrapDisposition::DenyUnowned,
        }
    }

    pub(in crate::services::discord) fn parent_bootstrap_disposition_with_direct_session_escape(
        &self,
        direct_session_escape_approved: bool,
    ) -> ParentBootstrapDisposition {
        let disposition = self.parent_bootstrap_disposition();
        if direct_session_escape_approved && disposition == ParentBootstrapDisposition::DenyUnowned
        {
            ParentBootstrapDisposition::DirectSessionEscape
        } else {
            disposition
        }
    }
}

pub(in crate::services::discord) fn validate_runtime_channel_binding_for_bot(
    settings_snapshot: &DiscordBotSettings,
    provider: &ProviderKind,
    resolution: &RuntimeChannelBindingResolution,
    is_dm: bool,
) -> Result<(), settings::BotChannelRoutingGuardFailure> {
    if resolution.status() != RuntimeChannelBindingStatus::Owned {
        return Err(settings::BotChannelRoutingGuardFailure::ChannelNotAllowed);
    }
    let identity = resolution
        .authority_identity()
        .unwrap_or_else(|| resolution.live_child());
    let role_binding = resolution
        .authority()
        .and_then(|authority| authority.payload().role.as_ref());
    settings::validate_bot_channel_routing_with_captured_binding(
        settings_snapshot,
        provider,
        identity.channel_id(),
        identity.channel_name(),
        identity.channel_name(),
        is_dm,
        role_binding,
    )
}

pub(in crate::services::discord) fn runtime_channel_binding_owned_by_bot(
    settings_snapshot: &DiscordBotSettings,
    provider: &ProviderKind,
    resolution: &RuntimeChannelBindingResolution,
    is_dm: bool,
) -> bool {
    validate_runtime_channel_binding_for_bot(settings_snapshot, provider, resolution, is_dm).is_ok()
}

pub(in crate::services::discord) fn classify_live_bot_channel_routing_status(
    settings_snapshot: &DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: serenity::model::id::ChannelId,
    is_dm: bool,
    live_child_name: Option<&str>,
    thread_parent: Option<(serenity::model::id::ChannelId, Option<&str>)>,
) -> RuntimeChannelBindingStatus {
    if !is_dm && live_child_name.is_none() {
        return RuntimeChannelBindingStatus::Unknown;
    }
    match settings::validate_bot_channel_routing_with_thread_parent(
        settings_snapshot,
        provider,
        channel_id,
        live_child_name,
        thread_parent,
        is_dm,
    ) {
        Ok(()) => RuntimeChannelBindingStatus::Owned,
        Err(reason) if !reason.orphans_inflight_on_restart() => {
            RuntimeChannelBindingStatus::Unknown
        }
        Err(_) => RuntimeChannelBindingStatus::Unowned,
    }
}

pub(in crate::services::discord) async fn resolve_live_bot_channel_routing_status(
    http: &Arc<serenity::Http>,
    settings_snapshot: &DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: serenity::model::id::ChannelId,
) -> RuntimeChannelBindingStatus {
    let (is_dm, live_child_name, thread_parent) =
        resolve_live_channel_routing_metadata(http, channel_id).await;
    classify_live_bot_channel_routing_status(
        settings_snapshot,
        provider,
        channel_id,
        is_dm,
        live_child_name.as_deref(),
        thread_parent
            .as_ref()
            .map(|(parent_id, parent_name)| (*parent_id, parent_name.as_deref())),
    )
}

fn classify_runtime_channel_binding_status(
    direct_child_bound: bool,
    is_thread: bool,
    inherited_parent_bound: bool,
    thread_inheritance_enabled: bool,
) -> RuntimeChannelBindingStatus {
    if direct_child_bound {
        RuntimeChannelBindingStatus::Owned
    } else if is_thread && inherited_parent_bound && thread_inheritance_enabled {
        RuntimeChannelBindingStatus::Owned
    } else {
        RuntimeChannelBindingStatus::Unowned
    }
}

fn classify_runtime_channel_binding_from_captured_metadata(
    live_child: RuntimeChannelIdentity,
    direct_payload: Option<settings::ConfiguredBindingPayload>,
    thread_parent: Option<(
        RuntimeChannelIdentity,
        settings::RuntimeConfiguredBindingDecision,
    )>,
    metadata_state: RuntimeMetadataState,
) -> RuntimeChannelBindingResolution {
    let parent = thread_parent.as_ref().map(|(identity, _)| identity.clone());
    if let Some(payload) = direct_payload {
        let identity = live_child.clone();
        return RuntimeChannelBindingResolution {
            live_child,
            parent,
            authority: Some(RuntimeBindingAuthority::Direct { identity, payload }),
            parent_inheritance_opted_out: false,
            metadata_state,
        };
    }

    let parent_inheritance_opted_out = thread_parent
        .as_ref()
        .is_some_and(|(_, decision)| decision.thread_inherit == Some(false));
    let authority = thread_parent.and_then(|(identity, decision)| {
        decision
            .payload
            .filter(|payload| payload.thread_inherit)
            .map(|payload| RuntimeBindingAuthority::InheritedParent { identity, payload })
    });
    RuntimeChannelBindingResolution {
        live_child,
        parent,
        authority,
        parent_inheritance_opted_out,
        metadata_state,
    }
}

fn classify_runtime_channel_binding_from_live_metadata(
    channel_id: serenity::model::id::ChannelId,
    channel_name: Option<&str>,
    thread_parent: Option<(serenity::model::id::ChannelId, Option<&str>)>,
) -> RuntimeChannelBindingResolution {
    let direct_payload = settings::resolve_runtime_configured_binding(channel_id, channel_name);
    let parent = thread_parent.map(|(parent_id, parent_name)| {
        (
            RuntimeChannelIdentity::new(parent_id, parent_name.map(ToOwned::to_owned)),
            settings::resolve_runtime_configured_binding_decision(parent_id, parent_name),
        )
    });
    classify_runtime_channel_binding_from_captured_metadata(
        RuntimeChannelIdentity::new(channel_id, channel_name.map(ToOwned::to_owned)),
        direct_payload,
        parent,
        RuntimeMetadataState::Complete,
    )
}

/// The smallest live Discord metadata surface needed by runtime binding.
/// Keeping this independent of Serenity lets tests execute the same async
/// resolver and deterministically exercise lookup failures and ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RuntimeChannelMetadata {
    DirectMessage,
    Guild {
        name: String,
        is_thread: bool,
        parent_id: Option<serenity::model::id::ChannelId>,
    },
    Other,
}

#[async_trait::async_trait]
trait RuntimeChannelMetadataLookup: Sync {
    async fn lookup_channel(
        &self,
        channel_id: serenity::model::id::ChannelId,
    ) -> Result<RuntimeChannelMetadata, ()>;
}

struct SerenityRuntimeChannelMetadataLookup<'a> {
    http: &'a Arc<serenity::Http>,
}

#[async_trait::async_trait]
impl RuntimeChannelMetadataLookup for SerenityRuntimeChannelMetadataLookup<'_> {
    async fn lookup_channel(
        &self,
        channel_id: serenity::model::id::ChannelId,
    ) -> Result<RuntimeChannelMetadata, ()> {
        match channel_id.to_channel(self.http).await.map_err(|_| ())? {
            serenity::model::channel::Channel::Private(_) => {
                Ok(RuntimeChannelMetadata::DirectMessage)
            }
            serenity::model::channel::Channel::Guild(channel) => {
                Ok(RuntimeChannelMetadata::Guild {
                    name: channel.name,
                    is_thread: crate::utils::discord::is_thread_channel_type(channel.kind),
                    parent_id: channel.parent_id,
                })
            }
            _ => Ok(RuntimeChannelMetadata::Other),
        }
    }
}

async fn resolve_runtime_channel_binding_resolution_with_lookup(
    lookup: &impl RuntimeChannelMetadataLookup,
    channel_id: serenity::model::id::ChannelId,
) -> RuntimeChannelBindingResolution {
    let Ok(channel) = lookup.lookup_channel(channel_id).await else {
        // Resolve the complete payload only after the metadata await. Mixing a
        // pre-await strict role with a post-await pinned workspace would create
        // an authority payload which never existed in one config generation.
        let direct_payload = settings::resolve_runtime_configured_binding(channel_id, None);
        return classify_runtime_channel_binding_from_captured_metadata(
            RuntimeChannelIdentity::new(channel_id, None),
            direct_payload,
            None,
            RuntimeMetadataState::Unknown,
        );
    };

    match channel {
        RuntimeChannelMetadata::DirectMessage => {
            RuntimeChannelBindingResolution::direct_message(channel_id)
        }
        RuntimeChannelMetadata::Guild {
            name,
            is_thread,
            parent_id,
        } => {
            let live_child = RuntimeChannelIdentity::new(channel_id, Some(name.clone()));
            let direct_payload =
                settings::resolve_runtime_configured_binding(channel_id, Some(&name));
            if direct_payload.is_some() {
                return classify_runtime_channel_binding_from_captured_metadata(
                    live_child,
                    direct_payload,
                    None,
                    RuntimeMetadataState::Complete,
                );
            }
            if !is_thread {
                return classify_runtime_channel_binding_from_captured_metadata(
                    live_child,
                    None,
                    None,
                    RuntimeMetadataState::Complete,
                );
            }
            let Some(parent_id) = parent_id else {
                return classify_runtime_channel_binding_from_captured_metadata(
                    live_child,
                    None,
                    None,
                    RuntimeMetadataState::Complete,
                );
            };
            let (parent_name, parent_lookup_complete) = match lookup.lookup_channel(parent_id).await
            {
                Ok(RuntimeChannelMetadata::Guild { name, .. }) => (Some(name), true),
                Ok(_) => (None, true),
                Err(()) => (None, false),
            };

            // Re-check direct authority after the parent await. A live config
            // reload may have bound the child while metadata was in flight; a
            // parent must never outrank that new direct barrier.
            let direct_payload =
                settings::resolve_runtime_configured_binding(channel_id, Some(&name));
            if direct_payload.is_some() {
                return classify_runtime_channel_binding_from_captured_metadata(
                    live_child,
                    direct_payload,
                    None,
                    RuntimeMetadataState::Complete,
                );
            }
            let parent_decision = settings::resolve_runtime_configured_binding_decision(
                parent_id,
                parent_name.as_deref(),
            );
            classify_runtime_channel_binding_from_captured_metadata(
                live_child,
                None,
                Some((
                    RuntimeChannelIdentity::new(parent_id, parent_name),
                    parent_decision,
                )),
                if parent_lookup_complete {
                    RuntimeMetadataState::Complete
                } else {
                    RuntimeMetadataState::Unknown
                },
            )
        }
        RuntimeChannelMetadata::Other => {
            let direct_payload = settings::resolve_runtime_configured_binding(channel_id, None);
            classify_runtime_channel_binding_from_captured_metadata(
                RuntimeChannelIdentity::new(channel_id, None),
                direct_payload,
                None,
                RuntimeMetadataState::Complete,
            )
        }
    }
}

pub(in crate::services::discord) async fn resolve_runtime_channel_binding_resolution(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> RuntimeChannelBindingResolution {
    let lookup = SerenityRuntimeChannelMetadataLookup { http };
    resolve_runtime_channel_binding_resolution_with_lookup(&lookup, channel_id).await
}

pub(in crate::services::discord) async fn resolve_runtime_channel_binding_status(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> RuntimeChannelBindingStatus {
    resolve_runtime_channel_binding_resolution(http, channel_id)
        .await
        .status()
}

#[cfg(test)]
#[path = "channel_routing/runtime_binding_tests.rs"]
mod runtime_binding_tests;

#[cfg(test)]
mod routing_authority_tests {
    use super::*;

    #[test]
    fn runtime_binding_status_matrix_preserves_child_then_inherited_parent_authority() {
        assert_eq!(
            classify_runtime_channel_binding_status(true, true, false, false),
            RuntimeChannelBindingStatus::Owned,
            "direct child binding remains authoritative even when parent inheritance is off"
        );
        assert_eq!(
            classify_runtime_channel_binding_status(false, true, true, true),
            RuntimeChannelBindingStatus::Owned,
            "unbound typed thread inherits an enabled parent"
        );
        assert_eq!(
            classify_runtime_channel_binding_status(false, true, true, false),
            RuntimeChannelBindingStatus::Unowned,
            "threadInherit=false blocks parent authority"
        );
        assert_eq!(
            classify_runtime_channel_binding_status(false, true, false, true),
            RuntimeChannelBindingStatus::Unowned,
            "an enabled but unbound parent cannot create authority"
        );
        assert_eq!(
            classify_runtime_channel_binding_status(false, false, true, true),
            RuntimeChannelBindingStatus::Unowned,
            "non-thread channels never inherit a parent"
        );
    }
}

/// If `channel_id` is a Discord thread, return the parent channel ID and name.
/// For non-thread channels, returns `None`.
pub(in crate::services::discord) async fn resolve_thread_parent(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> Option<(serenity::model::id::ChannelId, Option<String>)> {
    let channel = channel_id.to_channel(http).await.ok()?;
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return None;
    };
    if !crate::utils::discord::is_thread_channel_type(gc.kind) {
        return None;
    }
    let parent_id = gc.parent_id?;
    let parent_name = if let Ok(parent_ch) = parent_id.to_channel(http).await {
        match parent_ch {
            serenity::model::channel::Channel::Guild(pg) => Some(pg.name.clone()),
            _ => None,
        }
    } else {
        None
    };
    Some((parent_id, parent_name))
}

pub(in crate::services::discord) async fn resolve_live_channel_routing_metadata(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> (
    bool,
    Option<String>,
    Option<(serenity::model::id::ChannelId, Option<String>)>,
) {
    let Ok(channel) = channel_id.to_channel(http).await else {
        return (false, None, None);
    };
    match channel {
        serenity::model::channel::Channel::Private(_) => (true, None, None),
        serenity::model::channel::Channel::Guild(channel) => {
            let child_name = Some(channel.name);
            let thread_parent = if crate::utils::discord::is_thread_channel_type(channel.kind) {
                if let Some(parent_id) = channel.parent_id {
                    let parent_name = match parent_id.to_channel(http).await {
                        Ok(serenity::model::channel::Channel::Guild(parent)) => Some(parent.name),
                        _ => None,
                    };
                    Some((parent_id, parent_name))
                } else {
                    None
                }
            } else {
                None
            };
            (false, child_name, thread_parent)
        }
        _ => (false, None, None),
    }
}
