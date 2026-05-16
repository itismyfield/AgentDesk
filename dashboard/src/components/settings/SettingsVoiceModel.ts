import type {
  VoiceAgentConfig,
  VoiceConfigPutBody,
  VoiceConfigResponse,
  VoiceSensitivityMode,
} from "../../types";

export const VOICE_SENSITIVITY_OPTIONS: Array<{
  value: VoiceSensitivityMode;
  labelKo: string;
  labelEn: string;
}> = [
  { value: "normal", labelKo: "보통", labelEn: "Normal" },
  { value: "conservative", labelKo: "보수적", labelEn: "Conservative" },
];

export interface VoiceAliasConflict {
  normalized: string;
  firstAgent: VoiceAgentConfig;
  firstAlias: string;
  secondAgent: VoiceAgentConfig;
  secondAlias: string;
}

export function cloneVoiceConfig(config: VoiceConfigResponse): VoiceConfigResponse {
  return {
    ...config,
    global: { ...config.global },
    agents: config.agents.map((agent) => ({
      ...agent,
      aliases: [...agent.aliases],
    })),
  };
}

export function normalizeVoiceAliasKey(value: string): string {
  return Array.from(value.normalize("NFC").toLocaleLowerCase())
    .filter((ch) => /[\p{Letter}\p{Number}]/u.test(ch))
    .join("")
    .normalize("NFC");
}

export function splitVoiceAliases(value: string): string[] {
  return value
    .split(/[,\n]/)
    .map((alias) => alias.trim())
    .filter((alias, index, aliases) => alias.length > 0 && aliases.indexOf(alias) === index);
}

export function voiceAgentBuiltInAliases(agent: VoiceAgentConfig): string[] {
  return [agent.id, agent.name, agent.name_ko ?? ""].filter((value) => value.trim().length > 0);
}

export function findVoiceAliasConflict(config: VoiceConfigResponse | null): VoiceAliasConflict | null {
  if (!config) return null;
  const seen = new Map<string, { agent: VoiceAgentConfig; alias: string }>();
  for (const agent of config.agents) {
    for (const alias of [...voiceAgentBuiltInAliases(agent), ...agent.aliases]) {
      const normalized = normalizeVoiceAliasKey(alias);
      if (!normalized) continue;
      const existing = seen.get(normalized);
      if (existing && existing.agent.id !== agent.id) {
        return {
          normalized,
          firstAgent: existing.agent,
          firstAlias: existing.alias,
          secondAgent: agent,
          secondAlias: alias,
        };
      }
      if (!existing) {
        seen.set(normalized, { agent, alias });
      }
    }
  }
  return null;
}

export function voiceAgentKeys(agentId: string): string[] {
  return [
    `voice.agent.${agentId}.enabled`,
    `voice.agent.${agentId}.wake_word`,
    `voice.agent.${agentId}.aliases`,
    `voice.agent.${agentId}.sensitivity`,
  ];
}

export function voiceConfigComparable(config: VoiceConfigResponse | null): unknown {
  if (!config) return null;
  return {
    global: config.global,
    agents: config.agents.map((agent) => ({
      id: agent.id,
      voice_enabled: agent.voice_enabled,
      wake_word: agent.wake_word,
      aliases: agent.aliases,
      sensitivity_mode: agent.sensitivity_mode,
    })),
  };
}

export function voiceSaveBody(config: VoiceConfigResponse): VoiceConfigPutBody {
  return {
    version: config.version,
    actor: "dashboard",
    global: {
      lobby_channel_id: config.global.lobby_channel_id?.trim() || null,
      active_agent_ttl_seconds: Math.max(1, Math.round(config.global.active_agent_ttl_seconds || 180)),
      default_sensitivity_mode: config.global.default_sensitivity_mode,
    },
    agents: config.agents.map((agent) => ({
      ...agent,
      wake_word: agent.wake_word.trim(),
      aliases: splitVoiceAliases(agent.aliases.join("\n")),
    })),
  };
}
