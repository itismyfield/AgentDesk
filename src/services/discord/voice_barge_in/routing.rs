use super::*;

impl VoiceBargeInRuntime {
    pub(super) async fn resolve_effective_foreground_config(
        &self,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
    ) -> EffectiveVoiceForegroundConfig {
        let config = self.cached_config().await;
        let mut provider = config.voice.foreground.provider.trim().to_string();
        if provider.is_empty() {
            provider = crate::voice::config::DEFAULT_FOREGROUND_PROVIDER.to_string();
        }
        let mut model = config.voice.foreground.model.trim().to_string();
        if model.is_empty() {
            model = crate::voice::config::DEFAULT_FOREGROUND_MODEL.to_string();
        }
        let mut max_chars = normalized_foreground_max_chars(config.voice.foreground.max_chars);
        let mut timeout_ms = normalized_foreground_timeout_ms(config.voice.foreground.timeout_ms);

        if let Some(agent) = config.agents.iter().find(|agent| {
            agent_voice_matches_channel(agent, source_channel_id)
                || agent_text_channel_matches(agent, target_channel_id)
                || agent_text_channel_matches(agent, source_channel_id)
        }) {
            if let Some(value) = agent
                .voice
                .foreground
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                provider = value.to_string();
            }
            if let Some(value) = agent
                .voice
                .foreground
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                model = value.to_string();
            }
            if let Some(value) = agent.voice.foreground.max_chars {
                max_chars = normalized_foreground_max_chars(value);
            }
            if let Some(value) = agent.voice.foreground.timeout_ms {
                timeout_ms = normalized_foreground_timeout_ms(value);
            }
        }

        EffectiveVoiceForegroundConfig {
            provider,
            model,
            max_chars,
            timeout_ms,
        }
    }

    pub(super) async fn resolve_voice_background_channel_for_source(
        &self,
        source_channel_id: ChannelId,
    ) -> Option<ChannelId> {
        if let Some(route) = self
            .channels
            .active_voice_routes
            .get(&source_channel_id.get())
        {
            return Some(route.channel_id);
        }
        let config = self.cached_config().await;
        config
            .agents
            .iter()
            .find_map(|agent| agent_voice_background_channel_for(agent, source_channel_id))
    }

    pub(super) async fn active_barge_in_mailbox_channel(
        &self,
        shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
    ) -> Option<ChannelId> {
        let routed_channel_id = self
            .channels
            .active_voice_routes
            .get(&source_channel_id.get())
            .map(|entry| entry.value().channel_id);
        if let Some(channel_id) = routed_channel_id {
            if super::mailbox_has_active_turn(shared, channel_id).await {
                return Some(channel_id);
            }
        }
        if super::mailbox_has_active_turn(shared, source_channel_id).await {
            return Some(source_channel_id);
        }
        None
    }

    /// Reverse lookup: given a background text channel, find the foreground
    /// voice channel that should hear the spoken summary.
    ///
    /// #2236: in multi-agent setups, multiple `AgentDef` entries can map the
    /// same background channel (either intentionally — shared workspace — or
    /// by misconfiguration). The previous implementation silently picked the
    /// FIRST matching agent. This is a fail-closed reverse lookup:
    ///
    /// 1. If a current active voice route matches the background channel,
    ///    that route wins (runtime state is always more specific than
    ///    config). When `expected_agent_id` is provided, prefer an active
    ///    route whose agent matches.
    /// 2. Otherwise, scan the config for agents whose
    ///    `agent_voice_channel_for_background` resolves the background
    ///    channel. If exactly one matches, return it. If multiple match,
    ///    require `expected_agent_id` to disambiguate; if absent or no
    ///    config agent matches by id, log a warn and return None
    ///    (fail-closed — rather than speak into the wrong agent's voice
    ///    channel).
    pub(in crate::services::discord) async fn voice_channel_for_background(
        &self,
        background_channel_id: ChannelId,
        expected_agent_id: Option<&str>,
    ) -> Option<ChannelId> {
        let active_matches: Vec<(u64, String)> = self
            .channels
            .active_voice_routes
            .iter()
            .filter(|entry| entry.value().channel_id == background_channel_id)
            .map(|entry| (*entry.key(), entry.value().agent_id.clone()))
            .collect();
        match active_matches.len() {
            0 => {}
            1 => return Some(ChannelId::new(active_matches[0].0)),
            _ => {
                if let Some(agent_id) = expected_agent_id {
                    if let Some((source, _)) = active_matches
                        .iter()
                        .find(|(_, route_agent)| route_agent == agent_id)
                    {
                        return Some(ChannelId::new(*source));
                    }
                    tracing::warn!(
                        event = "voice_background_active_route_agent_mismatch",
                        background_channel_id = background_channel_id.get(),
                        expected_agent_id = %agent_id,
                        candidate_agents = ?active_matches
                            .iter()
                            .map(|(_, agent)| agent.as_str())
                            .collect::<Vec<_>>(),
                        "multiple active voice routes share the same background channel but none match the expected agent_id; refusing to pick silently"
                    );
                    return None;
                }
                tracing::warn!(
                    event = "voice_background_multi_active_route_no_disambiguator",
                    background_channel_id = background_channel_id.get(),
                    candidate_agents = ?active_matches
                        .iter()
                        .map(|(_, agent)| agent.as_str())
                        .collect::<Vec<_>>(),
                    "multiple active voice routes share the same background channel and dispatch carried no agent_id; refusing to pick silently"
                );
                return None;
            }
        }
        let config = self.cached_config().await;
        let mut matches: Vec<(String, ChannelId)> = config
            .agents
            .iter()
            .filter_map(|agent| {
                agent_voice_channel_for_background(agent, background_channel_id)
                    .map(|voice_channel| (agent.id.clone(), voice_channel))
            })
            .collect();
        match matches.len() {
            0 => None,
            1 => Some(matches.remove(0).1),
            _ => {
                let candidate_agents: Vec<&str> = matches
                    .iter()
                    .map(|(agent_id, _)| agent_id.as_str())
                    .collect();
                if let Some(expected) = expected_agent_id {
                    if let Some((_, voice_channel)) =
                        matches.iter().find(|(agent_id, _)| agent_id == expected)
                    {
                        tracing::warn!(
                            event = "voice_background_multi_agent_disambiguated",
                            background_channel_id = background_channel_id.get(),
                            expected_agent_id = %expected,
                            candidate_agents = ?candidate_agents,
                            "multiple config agents share the same background channel; disambiguated by dispatch agent_id"
                        );
                        return Some(*voice_channel);
                    }
                    tracing::warn!(
                        event = "voice_background_multi_agent_unknown_id",
                        background_channel_id = background_channel_id.get(),
                        expected_agent_id = %expected,
                        candidate_agents = ?candidate_agents,
                        "dispatch agent_id does not match any config agent claiming this background channel; refusing to pick silently"
                    );
                    None
                } else {
                    tracing::warn!(
                        event = "voice_background_multi_agent_no_disambiguator",
                        background_channel_id = background_channel_id.get(),
                        candidate_agents = ?candidate_agents,
                        "multiple config agents claim the same background channel and dispatch carried no agent_id; refusing to pick silently (#2236 fail-closed)"
                    );
                    None
                }
            }
        }
    }

    pub(super) async fn resolve_voice_turn_target(
        &self,
        _shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
        transcript: &str,
    ) -> VoiceTurnTargetResolution {
        let config = self.cached_config().await;
        if let Some((agent_id, target_channel_id)) = config.agents.iter().find_map(|agent| {
            agent_voice_background_channel_for(agent, source_channel_id)
                .map(|channel_id| (agent.id.clone(), channel_id))
        }) {
            self.bind_routed_voice_context(source_channel_id, target_channel_id);
            self.channels.active_voice_routes.insert(
                source_channel_id.get(),
                ActiveVoiceRoute {
                    agent_id,
                    channel_id: target_channel_id,
                    updated_at: Instant::now(),
                },
            );
            return VoiceTurnTargetResolution::Target {
                channel_id: target_channel_id,
                transcript: transcript.trim().to_string(),
            };
        }

        if super::settings::resolve_role_binding(source_channel_id, None).is_some() {
            return VoiceTurnTargetResolution::Target {
                channel_id: source_channel_id,
                transcript: transcript.trim().to_string(),
            };
        }

        if !voice_lobby_accepts_source_channel(&config.voice, source_channel_id) {
            tracing::debug!(
                source_channel_id = source_channel_id.get(),
                lobby_channel_id = config.voice.lobby_channel_id.as_deref(),
                "voice source channel is not role-bound or configured as voice lobby"
            );
            return VoiceTurnTargetResolution::Ignored;
        }

        let active_context = self
            .channels
            .active_voice_routes
            .get(&source_channel_id.get())
            .map(|entry| VoiceActiveAgentContext {
                agent_id: entry.agent_id.clone(),
                channel_id: entry.channel_id.get(),
                updated_at: entry.updated_at,
            });
        let now = Instant::now();
        match resolve_voice_lobby_route(&config, transcript, active_context.as_ref(), now) {
            Ok(VoiceLobbyRouteDecision::Routed(route)) => {
                let remaining = route.remaining_transcript.trim();
                if remaining.is_empty() {
                    return VoiceTurnTargetResolution::NeedsAgent;
                }
                let target_channel_id = ChannelId::new(route.channel_id);
                self.bind_routed_voice_context(source_channel_id, target_channel_id);
                self.channels.active_voice_routes.insert(
                    source_channel_id.get(),
                    ActiveVoiceRoute {
                        agent_id: route.agent_id,
                        channel_id: target_channel_id,
                        updated_at: now,
                    },
                );
                VoiceTurnTargetResolution::Target {
                    channel_id: target_channel_id,
                    transcript: remaining.to_string(),
                }
            }
            Ok(VoiceLobbyRouteDecision::ContinueActive {
                agent_id,
                channel_id,
                transcript,
            }) => {
                let target_channel_id = ChannelId::new(channel_id);
                self.bind_routed_voice_context(source_channel_id, target_channel_id);
                self.channels.active_voice_routes.insert(
                    source_channel_id.get(),
                    ActiveVoiceRoute {
                        agent_id,
                        channel_id: target_channel_id,
                        updated_at: now,
                    },
                );
                VoiceTurnTargetResolution::Target {
                    channel_id: target_channel_id,
                    transcript,
                }
            }
            Ok(VoiceLobbyRouteDecision::NeedAgent) => VoiceTurnTargetResolution::NeedsAgent,
            Err(error) => {
                // F12 (#2046): 매 utterance 마다 같은 collision 으로 warn 이
                // 쏟아지는 것을 막기 위해 normalized signature 단위로 1회만 warn.
                let signature = error.normalized.clone();
                let first_time = if let Ok(mut guard) = self.alias_collision_signature.lock() {
                    if guard.as_deref() == Some(&signature) {
                        false
                    } else {
                        *guard = Some(signature.clone());
                        true
                    }
                } else {
                    true
                };
                if first_time {
                    tracing::warn!(
                        error = %error,
                        source_channel_id = source_channel_id.get(),
                        normalized = %signature,
                        "voice lobby routing disabled: alias collision detected (suppressed until alias changes)"
                    );
                } else {
                    tracing::debug!(
                        error = %error,
                        source_channel_id = source_channel_id.get(),
                        "voice lobby routing still blocked by previously logged alias collision"
                    );
                }
                VoiceTurnTargetResolution::NeedsAgent
            }
        }
    }

    fn bind_routed_voice_context(
        &self,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
    ) {
        let Some(guild_id) = self.channels.guild_id(source_channel_id) else {
            return;
        };
        self.channels.register_context(target_channel_id, guild_id);
    }

    pub(super) fn voice_turn_guild_id(
        &self,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
    ) -> Option<GuildId> {
        self.channels
            .guild_id(source_channel_id)
            .or_else(|| self.channels.guild_id(target_channel_id))
    }

    pub(super) async fn ask_for_agent(&self, shared: &Arc<SharedData>, channel_id: ChannelId) {
        let Some(http) = shared.serenity_http_or_token_fallback() else {
            return;
        };
        super::rate_limit_wait(shared, channel_id).await;
        if let Err(error) =
            super::http::send_channel_message(&http, channel_id, "어느 에이전트?").await
        {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                "failed to send voice lobby routing prompt"
            );
        }
    }
}

pub(super) fn agent_voice_matches_channel(
    agent: &crate::config::AgentDef,
    channel_id: ChannelId,
) -> bool {
    agent
        .voice
        .channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
        == Some(channel_id.get())
}

fn agent_voice_channel(agent: &crate::config::AgentDef) -> Option<ChannelId> {
    agent
        .voice
        .channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
        .map(ChannelId::new)
}

pub(super) fn agent_voice_background_channel(agent: &crate::config::AgentDef) -> Option<ChannelId> {
    let preferred_provider = agent.provider.trim();
    if !preferred_provider.is_empty()
        && let Some((_, Some(channel))) = agent
            .channels
            .iter()
            .into_iter()
            .find(|(provider, channel)| *provider == preferred_provider && channel.is_some())
        && let Some(channel_id) = channel
            .channel_id()
            .and_then(|value| value.parse::<u64>().ok())
    {
        return Some(ChannelId::new(channel_id));
    }

    agent
        .channels
        .iter()
        .into_iter()
        .filter_map(|(_, channel)| channel)
        .find_map(|channel| {
            channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
                .map(ChannelId::new)
        })
}

pub(super) fn agent_voice_background_channel_for(
    agent: &crate::config::AgentDef,
    voice_channel_id: ChannelId,
) -> Option<ChannelId> {
    agent_voice_matches_channel(agent, voice_channel_id)
        .then(|| agent_voice_background_channel(agent))
        .flatten()
}

pub(super) fn agent_voice_channel_for_background(
    agent: &crate::config::AgentDef,
    background_channel_id: ChannelId,
) -> Option<ChannelId> {
    let voice_channel_id = agent_voice_channel(agent)?;
    (agent_voice_background_channel(agent) == Some(background_channel_id))
        .then_some(voice_channel_id)
}

pub(super) fn agent_voice_source_channel_for_background(
    config: &crate::config::Config,
    background_channel_id: ChannelId,
) -> Option<ChannelId> {
    let mut matches = config
        .agents
        .iter()
        .filter_map(|agent| agent_voice_channel_for_background(agent, background_channel_id));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

pub(super) fn effective_voice_source_channel(
    config: &crate::config::Config,
    channel_id: ChannelId,
) -> ChannelId {
    agent_voice_source_channel_for_background(config, channel_id).unwrap_or(channel_id)
}

fn agent_text_channel_matches(agent: &crate::config::AgentDef, channel_id: ChannelId) -> bool {
    let channel_id = channel_id.get().to_string();
    agent
        .channels
        .iter()
        // AgentChannels::iter returns a fixed array, so into_iter is required.
        .into_iter()
        .filter_map(|(_, channel)| channel)
        .any(|channel| channel.channel_id().as_deref() == Some(channel_id.as_str()))
}

fn normalized_foreground_max_chars(value: usize) -> usize {
    if value == 0 {
        crate::voice::config::DEFAULT_FOREGROUND_MAX_CHARS
    } else {
        value
    }
}

fn normalized_foreground_timeout_ms(value: u64) -> u64 {
    let value = if value == 0 {
        crate::voice::config::DEFAULT_FOREGROUND_TIMEOUT_MS
    } else {
        value
    };
    // #3914: clamp up tiny misconfigured values so a typo cannot make every
    // foreground call time out and degrade to Silence.
    value.max(crate::voice::config::MIN_FOREGROUND_TIMEOUT_MS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::config::{DEFAULT_FOREGROUND_TIMEOUT_MS, MIN_FOREGROUND_TIMEOUT_MS};

    #[test]
    fn foreground_timeout_zero_falls_back_to_default() {
        assert_eq!(
            normalized_foreground_timeout_ms(0),
            DEFAULT_FOREGROUND_TIMEOUT_MS
        );
    }

    #[test]
    fn foreground_timeout_below_minimum_is_clamped_up() {
        // #3914: a 50ms misconfiguration must not make every foreground call
        // time out (which would degrade each utterance to Silence).
        assert_eq!(
            normalized_foreground_timeout_ms(50),
            MIN_FOREGROUND_TIMEOUT_MS
        );
    }

    #[test]
    fn foreground_timeout_above_minimum_is_preserved() {
        assert_eq!(normalized_foreground_timeout_ms(10_000), 10_000);
    }
}
