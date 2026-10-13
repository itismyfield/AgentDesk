//! Existing Legacy barriers and effects composed into the dormant production supervisor ports.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use poise::serenity_prelude::{Channel, ChannelId, ChannelType, Http, MessageId};
use tokio::sync::OwnedMutexGuard;

use super::notice::DiscordNotice;
use crate::services::discord::commands::control::input_clear::LedgerClear;
use crate::services::discord::input_runtime::clear::Step;
use crate::services::discord::input_runtime::effects::{Deps, ProdEffects};
use crate::services::discord::input_runtime::fence::{self, Closing, Failure};
use crate::services::discord::input_runtime::intake::{
    self, Classification, InputDisposition, PolicyFacts, PreparedSource, Protection, TargetFacts,
    VoiceTarget,
};
use crate::services::discord::input_runtime::supervisor::Ports;
use crate::services::discord::input_runtime::supervisor::mapping::Probe;
use crate::services::discord::input_runtime::supervisor::source::Source;
use crate::services::discord::input_transition::Files;
use crate::services::discord::{SharedData, discord_io, dispatch_policy, inflight, settings};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::handover::{EnqueueOutcome, MoveEvidence, Reconciliation};
use crate::services::tui_input::ledger::Ledger;
use crate::services::tui_input::rows::Row;
use crate::services::tui_input::transition::{DeletePhase, Host, Input};

struct Resources {
    tmux: String,
    name: String,
    authorized: bool,
    active_sources: Vec<u64>,
    announce: Option<u64>,
}

pub(in crate::services::discord) struct RuntimePorts {
    shared: Arc<SharedData>,
    http: Arc<Http>,
    root: PathBuf,
    channel: u64,
    probe: Probe,
    notice: DiscordNotice,
    resources: Resources,
    frozen: Option<Arc<Closing>>,
}

fn invalid(reason: impl Into<String>) -> io::Error {
    io::Error::other(reason.into())
}

fn target(channel_id: u64) -> TargetFacts {
    TargetFacts {
        channel_id,
        protection: fence::channel_gate(channel_id).map_or(Protection::Unprotected, |gate| {
            Protection::Protected(gate.provider().clone())
        }),
    }
}

fn source_disposition(shared: &SharedData, source: &PreparedSource) -> Option<InputDisposition> {
    let input = source.input();
    let raw = input["original_text"].as_str()?;
    let kind = input["source_kind"].as_str()?;
    if matches!(kind, "live_message" | "raw_attachment") {
        if !matches!(input["message_kind"].as_str()?, "regular" | "inline_reply") {
            return Some(InputDisposition::Terminal);
        }
        if crate::services::discord::catch_up::handled_command::is_text_command(raw) {
            return Some(InputDisposition::Terminal);
        }
        if let Some(slash) = raw.strip_prefix('/') {
            let name = slash.split_whitespace().next().unwrap_or("");
            return shared.known_slash_commands.get().map(|commands| {
                if commands.contains(name) {
                    InputDisposition::Terminal
                } else {
                    InputDisposition::Prompt
                }
            });
        }
        return Some(InputDisposition::Prompt);
    }
    if !matches!(kind, "text_skill" | "slash_skill") {
        return None;
    }
    let raw = raw.trim_start();
    let raw = if crate::services::discord::catch_up::handled_command::is_text_command(raw)
        && raw.starts_with("<@")
    {
        raw.split_once('>')?.1.trim_start()
    } else {
        raw
    };
    let mut parts = raw.split_whitespace();
    let command = parts.next()?;
    let name = parts.next().unwrap_or("");
    if !matches!(command, "!skill" | "!cc" | "/skill" | "/cc") {
        return Some(InputDisposition::Unsupported);
    }
    if name.is_empty() {
        return Some(InputDisposition::Unsupported);
    }
    Some(
        if matches!(
            name,
            "clear" | "stop" | "pwd" | "health" | "status" | "inflight" | "help"
        ) {
            InputDisposition::Terminal
        } else {
            InputDisposition::Prompt
        },
    )
}

async fn current_route(http: &Arc<Http>, channel: u64) -> Result<(u64, String, bool), ()> {
    let fetched = ChannelId::new(channel)
        .to_channel(http)
        .await
        .map_err(|_| ())?;
    let Channel::Guild(channel) = fetched else {
        return Ok((channel, String::new(), false));
    };
    if matches!(
        channel.kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    ) {
        let parent = channel.parent_id.ok_or(())?;
        let Channel::Guild(parent_channel) = parent.to_channel(http).await.map_err(|_| ())? else {
            return Err(());
        };
        return Ok((
            parent.get(),
            parent_channel.name,
            matches!(parent_channel.kind, ChannelType::Text | ChannelType::News),
        ));
    }
    Ok((
        channel.id.get(),
        channel.name,
        matches!(channel.kind, ChannelType::Text | ChannelType::News),
    ))
}

/// The live boundary and actor read the same current facts without borrowing the WAL.
pub(in crate::services::discord) async fn source_policy_facts(
    shared: &Arc<SharedData>,
    http: &Arc<Http>,
    source: &PreparedSource,
) -> PolicyFacts {
    use crate::services::discord::health::UtilityBotUserIdResolution;
    let identity = source.identity();
    let current = shared.settings.read().await.clone();
    let announce = if let Some(registry) = shared.health_registry() {
        registry
            .utility_bot_user_id_resolution(
                crate::services::discord::bot_role::UtilityBotRole::Announce,
            )
            .await
    } else {
        UtilityBotUserIdResolution::Unconfigured
    };
    let allowed_bot = current.allowed_bot_ids.contains(&identity.author_id)
        || announce.user_id() == Some(identity.author_id);
    let bot = source.input()["author_is_bot"].as_bool();
    let raw = source.input()["original_text"].as_str();
    let authorized = bot.and_then(|is_bot| {
        if announce == UtilityBotUserIdResolution::Unavailable && !allowed_bot {
            return None;
        }
        Some(
            allowed_bot
                || (!is_bot && discord_io::user_is_authorized(&current, identity.author_id)),
        )
    });
    let mut disposition = source_disposition(shared, source);
    if let Some((is_bot, text)) = bot.zip(raw)
        && (!dispatch_policy::is_allowed_turn_sender(
            &current.allowed_bot_ids,
            announce.user_id(),
            identity.author_id,
            is_bot,
            text,
        ) || dispatch_policy::has_non_turn_provenance(text))
    {
        disposition = Some(InputDisposition::Unsupported);
    }
    let route = current_route(http, identity.original_channel_id).await;
    let name = {
        let core = shared.core.lock().await;
        core.sessions
            .get(&ChannelId::new(identity.execution_channel_id))
            .filter(|session| session.channel_id == Some(identity.execution_channel_id))
            .and_then(|session| session.channel_name.clone())
    };
    let supported = route
        .as_ref()
        .ok()
        .and_then(|(channel, rest_name, supported_kind)| {
            name.as_ref().map(|name| {
                *supported_kind
                    && *channel == identity.execution_channel_id
                    && current.provider == shared.provider
                    && settings::validate_bot_channel_routing(
                        &current,
                        &shared.provider,
                        ChannelId::new(*channel),
                        Some(rest_name),
                        false,
                    )
                    .is_ok()
                    && settings::validate_bot_channel_routing(
                        &current,
                        &shared.provider,
                        ChannelId::new(*channel),
                        Some(name),
                        false,
                    )
                    .is_ok()
            })
        });
    let execution = match route {
        Ok((channel, _, _)) => target(channel),
        Err(()) => TargetFacts {
            channel_id: identity.execution_channel_id,
            protection: Protection::Unknown,
        },
    };
    let stored = if source.input()["source_kind"] == "slash_skill" {
        None
    } else {
        crate::voice::announce_meta::global_store().peek_clone(MessageId::new(source.key()))
    };
    let voice_shape = raw.is_some_and(|raw| {
        crate::voice::prompt::is_voice_transcript_announcement_candidate(raw)
            || crate::voice::prompt::is_readable_voice_transcript_announcement(raw)
    }) || source
        .input()
        .get("voice_announcement")
        .is_some_and(|value| !value.is_null())
        || stored.is_some();
    let voice_target = if source.input()["source_kind"] == "slash_skill" {
        VoiceTarget::NotApplicable
    } else if voice_shape {
        let announcement = if announce.user_id() != Some(identity.author_id) {
            None
        } else if let Some(pool) = &shared.pg_pool {
            crate::voice::announce_meta::load_voice_announcement_durable(
                pool,
                MessageId::new(source.key()),
            )
            .await
            .ok()
            .flatten()
        } else {
            stored
        };
        if let Some(announcement) = announcement {
            if announcement.transcript.trim().is_empty()
                || crate::voice::commands::parse_voice_command(&announcement.transcript).is_some()
            {
                disposition = Some(InputDisposition::Unsupported);
            }
            let same = source
                .input()
                .get("voice_announcement")
                .filter(|value| !value.is_null())
                .is_none_or(|value| {
                    serde_json::to_value(&announcement).is_ok_and(|fresh| &fresh == value)
                });
            if !same || announcement.control_channel_id == Some(0) {
                VoiceTarget::Unknown
            } else {
                match shared
                    .voice_barge_in
                    .announcement_target(
                        ChannelId::new(identity.original_channel_id),
                        &current.provider,
                    )
                    .await
                {
                    Ok(Some(channel)) => VoiceTarget::Resolved(target(channel.get())),
                    Ok(None) => VoiceTarget::NotApplicable,
                    Err(_) => VoiceTarget::Unknown,
                }
            }
        } else {
            VoiceTarget::Unknown
        }
    } else {
        match shared
            .voice_barge_in
            .text_reply_target(
                ChannelId::new(identity.original_channel_id),
                &current.provider,
            )
            .await
        {
            Ok(Some(channel)) => VoiceTarget::Resolved(target(channel.get())),
            Ok(None) => VoiceTarget::NotApplicable,
            Err(_) => VoiceTarget::Unknown,
        }
    };
    let final_announce = if let Some(registry) = shared.health_registry() {
        registry
            .utility_bot_user_id_resolution(
                crate::services::discord::bot_role::UtilityBotRole::Announce,
            )
            .await
    } else {
        UtilityBotUserIdResolution::Unconfigured
    };
    let latest = shared.settings.read().await.clone();
    let latest_name = {
        let core = shared.core.lock().await;
        core.sessions
            .get(&ChannelId::new(identity.execution_channel_id))
            .filter(|session| session.channel_id == Some(identity.execution_channel_id))
            .and_then(|session| session.channel_name.clone())
    };
    let changed = announce != final_announce
        || current.provider != latest.provider
        || current.agent != latest.agent
        || current.owner_user_id != latest.owner_user_id
        || current.allowed_user_ids != latest.allowed_user_ids
        || current.allow_all_users != latest.allow_all_users
        || current.allowed_bot_ids != latest.allowed_bot_ids
        || current.allowed_channel_ids != latest.allowed_channel_ids
        || current.require_mention_channel_ids != latest.require_mention_channel_ids
        || name != latest_name;
    #[cfg(test)]
    let changed = changed
        && !crate::services::discord::input_runtime::supervisor::mutant(
            "c2b_policy_change_after_lookup_approved",
        );
    PolicyFacts {
        provider: Some(current.provider),
        original: target(identity.original_channel_id),
        execution,
        voice_target: if changed {
            VoiceTarget::Unknown
        } else {
            voice_target
        },
        authorized: if changed { None } else { authorized },
        supported: if changed { None } else { supported },
        disposition,
        client_ready: true,
        admission_open: true,
    }
}

impl RuntimePorts {
    pub(crate) async fn new(
        shared: Arc<SharedData>,
        http: Arc<Http>,
        root: PathBuf,
        channel: u64,
        probe: Probe,
    ) -> io::Result<Self> {
        let resources = Self::resources(&shared, &root, channel).await?;
        Ok(Self {
            shared,
            notice: DiscordNotice::new(http.clone(), channel),
            http,
            root,
            channel,
            probe,
            resources,
            frozen: None,
        })
    }

    pub(crate) fn provider(&self) -> &ProviderKind {
        &self.shared.provider
    }

    pub(crate) fn session(&self) -> &str {
        &self.resources.tmux
    }

    async fn policy_facts(&self, source: &Source) -> PolicyFacts {
        let facts = match PreparedSource::from_material(
            source.key(),
            source.identity().clone(),
            source.input().clone(),
        ) {
            Ok(prepared) => source_policy_facts(&self.shared, &self.http, &prepared).await,
            Err(_) => PolicyFacts {
                provider: None,
                original: target(source.identity().original_channel_id),
                execution: target(source.identity().execution_channel_id),
                voice_target: VoiceTarget::Unknown,
                authorized: None,
                supported: None,
                disposition: None,
                client_ready: true,
                admission_open: true,
            },
        };
        #[cfg(test)]
        let facts = {
            let mut facts = facts;
            if crate::services::discord::input_runtime::supervisor::mutant(
                "c2_cached_source_policy",
            ) {
                facts.authorized = Some(self.resources.authorized);
            }
            facts
        };
        facts
    }

    async fn resources(shared: &SharedData, root: &Path, channel: u64) -> io::Result<Resources> {
        if channel == 0
            || !matches!(shared.provider, ProviderKind::Claude | ProviderKind::Codex)
            || fence::population_root().as_deref() != Some(root)
        {
            return Err(invalid("runtime identity or population root mismatch"));
        }
        let name = {
            let core = shared.core.lock().await;
            let session = core
                .sessions
                .get(&ChannelId::new(channel))
                .ok_or_else(|| invalid("input runtime session missing"))?;
            if session.channel_id != Some(channel) {
                return Err(invalid("input runtime session names another channel"));
            }
            session
                .channel_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| invalid("input runtime channel name missing"))?
        };
        let authorized = {
            let current = shared.settings.read().await;
            if current.provider != shared.provider {
                return Err(invalid("input runtime settings provider mismatch"));
            }
            settings::validate_bot_channel_routing(
                &current,
                &shared.provider,
                ChannelId::new(channel),
                Some(&name),
                false,
            )
            .map(|()| true)
            .map_err(|reason| invalid(reason.to_string()))?
        };
        let mut active_sources = Vec::new();
        if let Some(mailbox) = shared.mailbox_peek(ChannelId::new(channel)) {
            let snapshot = mailbox
                .try_snapshot()
                .await
                .map_err(|_| invalid("input runtime mailbox unreadable"))?;
            let sources = snapshot
                .active_user_message_id
                .into_iter()
                .chain(snapshot.active_absorbed_source_ids)
                .chain(snapshot.pending_user_dispatch)
                .chain(snapshot.pending_user_dispatch_source_ids);
            active_sources.extend(sources.map(|id| id.get()));
        }
        if let Some(row) = inflight::load_inflight_state_read_only_result(&shared.provider, channel)
            .map_err(invalid)?
        {
            if row.channel_id != channel || row.provider_kind().as_ref() != Some(&shared.provider) {
                return Err(invalid("input runtime inflight identity mismatch"));
            }
            active_sources.push(row.user_msg_id);
            active_sources.extend(row.source_message_ids);
        }
        active_sources.retain(|id| *id != 0);
        active_sources.sort_unstable();
        active_sources.dedup();
        Ok(Resources {
            tmux: shared.provider.build_tmux_session_name(&name),
            name,
            authorized,
            active_sources,
            announce: dispatch_policy::resolve_announce_bot_user_id(shared).await,
        })
    }
}

impl Ports for RuntimePorts {
    type Clear = LedgerClear;
    type Move = RuntimeMove;

    fn source_policy<'a>(&'a mut self, source: &'a Source) -> Step<'a, bool> {
        Box::pin(async move {
            let prepared = match PreparedSource::from_material(
                source.key(),
                source.identity().clone(),
                source.input().clone(),
            ) {
                Ok(prepared) => prepared,
                Err(_) => return false,
            };
            intake::classify(&self.policy_facts(source).await, &prepared) == Classification::Target
        })
    }

    fn mapping(&self) -> Probe {
        self.probe.clone()
    }

    fn freeze(&mut self, closing: Arc<Closing>) -> Step<'_, Result<(), Failure>> {
        Box::pin(async move {
            if closing.channel() != self.channel || closing.provider() != self.provider() {
                return Err(Failure::StalePermit);
            }
            let persistence = crate::services::discord::queue_dispatch::persistence_context(
                &self.shared,
                self.provider(),
                ChannelId::new(self.channel),
            );
            let frozen = self
                .shared
                .freeze_legacy_queue(closing.clone(), persistence)
                .await?;
            // Release the existing session guard before the clear port takes that same lock.
            drop(frozen);
            self.resources = Self::resources(&self.shared, &self.root, self.channel)
                .await
                .map_err(|_| Failure::Persistence)?;
            self.frozen = Some(closing);
            Ok(())
        })
    }

    fn clear(&mut self) -> Step<'_, Option<(Self::Clear, OwnedMutexGuard<()>)>> {
        Box::pin(async move {
            let host = LedgerClear::for_resume(
                &self.http,
                &self.shared,
                self.provider(),
                ChannelId::new(self.channel),
            )
            .await
            .ok()?;
            let guard = self
                .shared
                .acquire_session_transition(ChannelId::new(self.channel))
                .await
                .ok()?;
            Some((host, guard))
        })
    }

    fn transition(&mut self, closing: Arc<Closing>) -> io::Result<Self::Move> {
        if self
            .frozen
            .as_ref()
            .is_none_or(|frozen| !Arc::ptr_eq(frozen, &closing))
            || fence::population_root().as_ref() != Some(&self.root)
            || self.shared.pg_pool.is_none()
        {
            return Err(invalid("runtime freeze, root or Postgres unavailable"));
        }
        let effects = ProdEffects::new(Deps {
            provider: self.shared.provider.clone(),
            channel: self.channel,
            tmux_session: self.resources.tmux.clone(),
            token_hash: self.shared.token_hash.clone(),
            authorized: self.resources.authorized,
            active_sources: self.resources.active_sources.clone(),
            pool: self.shared.pg_pool.clone(),
            closing: closing.clone(),
        })?;
        let files = Files::frozen(
            &self.root,
            self.shared.provider.clone(),
            self.channel,
            closing,
            effects,
        )?;
        Ok(RuntimeMove {
            files,
            shared: self.shared.clone(),
            channel: self.channel,
            name: self.resources.name.clone(),
            announce: self.resources.announce,
        })
    }

    fn take_notices(host: &mut Self::Move) -> Vec<(Option<u64>, &'static str)> {
        host.files.effects_mut().take_notices()
    }

    fn notice(&mut self, text: String) -> Step<'_, bool> {
        Box::pin(self.notice.send(text))
    }
}

pub(crate) struct RuntimeMove {
    files: Files<ProdEffects>,
    shared: Arc<SharedData>,
    channel: u64,
    name: String,
    announce: Option<u64>,
}

impl Host for RuntimeMove {
    fn collect(&mut self, ledger: &Ledger) -> io::Result<Vec<Input>> {
        self.files.collect(ledger)
    }
    fn evidence(&mut self, input: &Input) -> io::Result<MoveEvidence> {
        self.files.evidence(input)
    }
    fn pin_input(&mut self, ledger: &Ledger, input: &mut Input) -> io::Result<()> {
        self.files.pin_input(ledger, input)
    }
    fn delete(&mut self, phase: DeletePhase) -> io::Result<()> {
        self.files.delete(phase)
    }
    fn start_actor(&mut self) -> io::Result<()> {
        self.files.start_actor()
    }
    fn reconciliation(&mut self, key: u64, row: &Row) -> io::Result<Reconciliation> {
        self.files.reconciliation(key, row)
    }
    fn notice(&mut self, key: Option<u64>, reason: &'static str) -> io::Result<()> {
        self.files.notice(key, reason)
    }

    fn enqueue(&mut self, key: u64, row: &Row) -> io::Result<EnqueueOutcome> {
        let current = self
            .shared
            .settings
            .try_read()
            .map_err(|_| invalid("input handback settings unreadable"))?;
        if current.provider != self.shared.provider {
            return Err(invalid("input handback provider changed"));
        }
        settings::validate_bot_channel_routing(
            &current,
            &self.shared.provider,
            ChannelId::new(self.channel),
            Some(&self.name),
            false,
        )
        .map_err(|reason| invalid(reason.to_string()))?;
        let input = row.input.get("legacy_input").unwrap_or(&row.input);
        let author = input["author_id"]
            .as_u64()
            .filter(|id| *id != 0)
            .ok_or_else(|| invalid("input handback author missing"))?;
        let authorized = if input["author_is_bot"].as_bool().unwrap_or(false) {
            dispatch_policy::is_allowed_turn_sender(
                &current.allowed_bot_ids,
                self.announce,
                author,
                true,
                input["text"]
                    .as_str()
                    .ok_or_else(|| invalid("input handback text missing"))?,
            )
        } else {
            discord_io::user_is_authorized(&current, author)
        };
        if !authorized {
            return Ok(EnqueueOutcome::Rejected);
        }
        drop(current);
        self.files.enqueue(key, row)
    }
}

#[cfg(test)]
#[path = "ports_tests.rs"]
mod tests;
