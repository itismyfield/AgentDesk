//! Existing Legacy barriers and effects composed into the dormant production supervisor ports.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, Http};
use tokio::sync::OwnedMutexGuard;

use super::notice::DiscordNotice;
use crate::services::discord::commands::control::input_clear::LedgerClear;
use crate::services::discord::input_runtime::clear::Step;
use crate::services::discord::input_runtime::effects::{Deps, ProdEffects};
use crate::services::discord::input_runtime::fence::{self, Closing, Failure};
use crate::services::discord::input_runtime::intake::{
    self, Classification, InputDisposition, PolicyFacts, PreparedSource, Protection, Refusal,
    TargetFacts, VoiceTarget,
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
        let identity = source.identity();
        let current = self.shared.settings.read().await.clone();
        let announce = dispatch_policy::resolve_announce_bot_user_id(&self.shared).await;
        let bot = source.input()["author_is_bot"].as_bool();
        let raw = source.input()["original_text"].as_str();
        let allowed_bot = current.allowed_bot_ids.contains(&identity.author_id)
            || announce == Some(identity.author_id);
        let authorized = bot.map(|is_bot| {
            allowed_bot || (!is_bot && discord_io::user_is_authorized(&current, identity.author_id))
        });
        #[cfg(test)]
        let authorized = if crate::services::discord::input_runtime::supervisor::mutant(
            "c2_cached_source_policy",
        ) {
            Some(self.resources.authorized)
        } else {
            authorized
        };
        let disposition = bot.zip(raw).map(|(is_bot, text)| {
            if dispatch_policy::is_allowed_turn_sender(
                &current.allowed_bot_ids,
                announce,
                identity.author_id,
                is_bot,
                text,
            ) {
                InputDisposition::Prompt
            } else {
                InputDisposition::Unsupported
            }
        });
        let name = {
            let core = self.shared.core.lock().await;
            core.sessions
                .get(&ChannelId::new(self.channel))
                .filter(|session| session.channel_id == Some(self.channel))
                .and_then(|session| session.channel_name.clone())
        };
        let supported = name.filter(|name| !name.trim().is_empty()).map(|name| {
            identity.execution_channel_id == self.channel
                && current.provider == self.shared.provider
                && settings::validate_bot_channel_routing(
                    &current,
                    &self.shared.provider,
                    ChannelId::new(self.channel),
                    Some(&name),
                    false,
                )
                .is_ok()
        });
        let target = |channel| TargetFacts {
            channel_id: channel,
            protection: fence::channel_gate(channel).map_or(Protection::Unprotected, |gate| {
                Protection::Protected(gate.provider().clone())
            }),
        };
        PolicyFacts {
            provider: Some(current.provider),
            original: target(identity.original_channel_id),
            execution: target(identity.execution_channel_id),
            // The current voice accessor cannot distinguish an absent route from a failed lookup.
            voice_target: VoiceTarget::Unknown,
            authorized,
            supported,
            disposition,
            client_ready: true,
            admission_open: true,
        }
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

    fn source_policy<'a>(&'a mut self, source: &'a Source) -> Step<'a, Classification> {
        Box::pin(async move {
            let prepared = match PreparedSource::from_material(
                source.key(),
                source.identity().clone(),
                source.input().clone(),
            ) {
                Ok(prepared) => prepared,
                Err(_) => return Classification::Rejected(Refusal::Provenance),
            };
            intake::classify(&self.policy_facts(source).await, &prepared)
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
