import type { CSSProperties } from "react";

import type {
  VoiceAgentConfig,
  VoiceConfigResponse,
  VoiceGlobalConfig,
  VoiceSensitivityMode,
} from "../../types";
import {
  SurfaceCallout as SettingsCallout,
  SurfaceCard as SettingsCard,
  SurfaceEmptyState as SettingsEmptyState,
  SurfaceSubsection as SettingsSubsection,
} from "../common/SurfacePrimitives";
import { CompactFieldCard } from "./SettingsPanels";
import {
  VOICE_SENSITIVITY_OPTIONS,
  splitVoiceAliases,
  voiceAgentBuiltInAliases,
  voiceAgentKeys,
  type VoiceAliasConflict,
} from "./SettingsModel";
import type {
  RenderSettingGroupCard,
  SettingsActionStyles,
  SettingsTr,
} from "./SettingsPanelTypes";

interface SettingsVoicePanelProps extends Pick<
  SettingsActionStyles,
  "primaryActionClass" | "primaryActionStyle" | "secondaryActionClass" | "secondaryActionStyle"
> {
  inputStyle: CSSProperties;
  isKo: boolean;
  isRowVisible: (key: string) => boolean;
  loadVoiceConfig: () => Promise<VoiceConfigResponse | null>;
  onVoiceSave: () => Promise<void>;
  renderSettingGroupCard: RenderSettingGroupCard;
  tr: SettingsTr;
  updateVoiceAgent: (agentId: string, patch: Partial<VoiceAgentConfig>) => void;
  updateVoiceGlobal: <K extends keyof VoiceGlobalConfig>(key: K, value: VoiceGlobalConfig[K]) => void;
  voiceAliasConflict: VoiceAliasConflict | null;
  voiceDirty: boolean;
  voiceDraft: VoiceConfigResponse | null;
  voiceError: string | null;
  voiceLoaded: boolean;
  voiceSaving: boolean;
}

export function SettingsVoicePanel({
  inputStyle,
  isKo,
  isRowVisible,
  loadVoiceConfig,
  onVoiceSave,
  primaryActionClass,
  primaryActionStyle,
  renderSettingGroupCard,
  secondaryActionClass,
  secondaryActionStyle,
  tr,
  updateVoiceAgent,
  updateVoiceGlobal,
  voiceAliasConflict,
  voiceDirty,
  voiceDraft,
  voiceError,
  voiceLoaded,
  voiceSaving,
}: SettingsVoicePanelProps) {
  if (!voiceLoaded) {
    return (
      <SettingsEmptyState className="text-sm">
        {tr("음성 설정을 불러오는 중...", "Loading voice config...")}
      </SettingsEmptyState>
    );
  }
  if (!voiceDraft) {
    return (
      <div className="space-y-4">
        <SettingsEmptyState className="text-sm">
          {voiceError ?? tr("음성 설정을 불러오지 못했습니다.", "Failed to load voice settings.")}
        </SettingsEmptyState>
        <button
          type="button"
          onClick={() => void loadVoiceConfig()}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          {tr("다시 불러오기", "Retry")}
        </button>
      </div>
    );
  }

  const visibleGlobalCards = [
    isRowVisible("voice.global.lobby_channel_id") ? (
      <CompactFieldCard
        key="lobby"
        label={tr("Lobby 채널 ID", "Lobby channel ID")}
        description={tr(
          "단일 voice-lobby로 들어오는 음성을 agent alias 라우팅에 사용합니다.",
          "Single voice-lobby channel used for agent alias routing.",
        )}
      >
        <input
          value={voiceDraft.global.lobby_channel_id ?? ""}
          onChange={(event) => updateVoiceGlobal("lobby_channel_id", event.target.value)}
          className="w-full rounded-2xl px-3 py-2.5 text-sm"
          style={inputStyle}
          placeholder={tr("예: 1503294653313712169", "e.g. 1503294653313712169")}
        />
      </CompactFieldCard>
    ) : null,
    isRowVisible("voice.global.active_agent_ttl_seconds") ? (
      <CompactFieldCard
        key="ttl"
        label={tr("Active agent TTL", "Active agent TTL")}
        description={tr(
          "alias 없이 이어 말할 수 있는 최근 agent 유지 시간입니다.",
          "How long follow-up speech can continue without repeating an alias.",
        )}
      >
        <div className="flex items-center gap-3">
          <input
            type="range"
            min={30}
            max={1800}
            step={30}
            value={voiceDraft.global.active_agent_ttl_seconds}
            onChange={(event) =>
              updateVoiceGlobal("active_agent_ttl_seconds", Number(event.target.value))
            }
            className="h-1.5 flex-1 cursor-pointer appearance-none rounded-full"
            style={{ accentColor: "var(--th-accent-primary)" }}
          />
          <input
            type="number"
            min={1}
            step={30}
            value={voiceDraft.global.active_agent_ttl_seconds}
            onChange={(event) =>
              updateVoiceGlobal("active_agent_ttl_seconds", Number(event.target.value) || 180)
            }
            className="w-24 rounded-xl px-2 py-1.5 text-right text-xs"
            style={{
              ...inputStyle,
              fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
            }}
          />
        </div>
      </CompactFieldCard>
    ) : null,
    isRowVisible("voice.global.default_sensitivity_mode") ? (
      <CompactFieldCard
        key="sensitivity"
        label={tr("기본 민감도", "Default sensitivity")}
        description={tr(
          "agent별 override가 없을 때 적용할 barge-in 민감도입니다.",
          "Barge-in sensitivity used when an agent has no override.",
        )}
      >
        <select
          value={voiceDraft.global.default_sensitivity_mode}
          onChange={(event) =>
            updateVoiceGlobal("default_sensitivity_mode", event.target.value as VoiceSensitivityMode)
          }
          className="w-full rounded-2xl px-3 py-2.5 text-sm"
          style={inputStyle}
        >
          {VOICE_SENSITIVITY_OPTIONS.map((option) => (
            <option key={option.value} value={option.value}>
              {tr(option.labelKo, option.labelEn)}
            </option>
          ))}
        </select>
      </CompactFieldCard>
    ) : null,
    isRowVisible("voice.global.version") ? (
      <CompactFieldCard
        key="version"
        label={tr("설정 버전", "Config version")}
        description={tr(
          "저장 시 optimistic locking에 사용하는 버전 해시입니다.",
          "Version hash used for optimistic locking on save.",
        )}
      >
        <code
          className="block truncate rounded-xl px-3 py-2 text-xs"
          style={{
            background: "color-mix(in srgb, var(--th-overlay-medium) 80%, transparent)",
            color: "var(--th-text-muted)",
            fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
          }}
        >
          {voiceDraft.version}
        </code>
      </CompactFieldCard>
    ) : null,
  ].filter(Boolean);

  const agentCards = voiceDraft.agents
    .filter((agent) =>
      voiceAgentKeys(agent.id).some((key) => isRowVisible(key)),
    )
    .map((agent) => {
      const displayName = isKo && agent.name_ko ? agent.name_ko : agent.name;
      const conflictInAgent =
        voiceAliasConflict &&
        (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id);
      return (
        <SettingsCard
          key={agent.id}
          className="rounded-2xl p-4"
          style={{
            borderColor: conflictInAgent
              ? "rgba(248,113,113,0.45)"
              : "color-mix(in srgb, var(--th-border) 72%, transparent)",
            background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
          }}
        >
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                {displayName}
              </div>
              <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                {agent.id} · {agent.name}
              </div>
            </div>
            <button
              type="button"
              role="switch"
              aria-checked={agent.voice_enabled}
              onClick={() => updateVoiceAgent(agent.id, { voice_enabled: !agent.voice_enabled })}
              className="relative inline-flex h-7 w-12 items-center rounded-full transition-colors"
              style={{
                background: agent.voice_enabled
                  ? "var(--th-accent-primary)"
                  : "color-mix(in srgb, var(--th-border) 80%, transparent)",
              }}
            >
              <span
                className="inline-block h-6 w-6 rounded-full bg-white shadow transition-transform"
                style={{ transform: agent.voice_enabled ? "translateX(1.45rem)" : "translateX(0.13rem)" }}
              />
            </button>
          </div>

          <div className="mt-4 grid gap-3 md:grid-cols-2">
            {isRowVisible(`voice.agent.${agent.id}.wake_word`) ? (
              <CompactFieldCard
                label={tr("Wake word", "Wake word")}
                description={tr("비어 있으면 agent alias만으로 라우팅합니다.", "When empty, the agent routes by alias only.")}
              >
                <input
                  value={agent.wake_word}
                  onChange={(event) => updateVoiceAgent(agent.id, { wake_word: event.target.value })}
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                  placeholder={tr("예: 에이전트", "e.g. agent")}
                />
              </CompactFieldCard>
            ) : null}
            {isRowVisible(`voice.agent.${agent.id}.sensitivity`) ? (
              <CompactFieldCard
                label={tr("민감도", "Sensitivity")}
                description={tr("agent별 barge-in 감지 민감도입니다.", "Per-agent barge-in detection sensitivity.")}
              >
                <select
                  value={agent.sensitivity_mode}
                  onChange={(event) =>
                    updateVoiceAgent(agent.id, { sensitivity_mode: event.target.value as VoiceSensitivityMode })
                  }
                  className="w-full rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                >
                  {VOICE_SENSITIVITY_OPTIONS.map((option) => (
                    <option key={option.value} value={option.value}>
                      {tr(option.labelKo, option.labelEn)}
                    </option>
                  ))}
                </select>
              </CompactFieldCard>
            ) : null}
            {isRowVisible(`voice.agent.${agent.id}.aliases`) ? (
              <CompactFieldCard
                label={tr("Aliases", "Aliases")}
                description={tr("쉼표 또는 줄바꿈으로 여러 호출명을 입력합니다.", "Enter multiple spoken aliases separated by commas or new lines.")}
                footer={tr(
                  `기본 alias: ${voiceAgentBuiltInAliases(agent).join(", ")}`,
                  `Built-in aliases: ${voiceAgentBuiltInAliases(agent).join(", ")}`,
                )}
              >
                <textarea
                  value={agent.aliases.join("\n")}
                  onChange={(event) => updateVoiceAgent(agent.id, { aliases: splitVoiceAliases(event.target.value) })}
                  className="min-h-[92px] w-full resize-y rounded-2xl px-3 py-2.5 text-sm"
                  style={inputStyle}
                />
              </CompactFieldCard>
            ) : null}
          </div>
        </SettingsCard>
      );
    });

  return (
    <div className="space-y-5">
      <SettingsCallout
        action={(
          <button
            type="button"
            onClick={() => void onVoiceSave()}
            disabled={voiceSaving || !voiceDirty || Boolean(voiceAliasConflict)}
            className={primaryActionClass}
            style={primaryActionStyle}
          >
            {voiceSaving ? tr("저장 중...", "Saving...") : tr("음성 설정 저장", "Save voice")}
          </button>
        )}
      >
        <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "음성 설정은 agentdesk.yaml에 저장되며 runtime voice routing이 다음 발화부터 다시 읽습니다. alias는 NFC/lowercase/공백·특수문자 제거 기준으로 충돌을 막습니다.",
            "Voice settings are stored in agentdesk.yaml and runtime voice routing reloads them on the next utterance. Aliases reject collisions after NFC/lowercase and removing spaces/special characters.",
          )}
        </p>
      </SettingsCallout>

      {voiceError ? (
        <SettingsEmptyState className="text-sm">{voiceError}</SettingsEmptyState>
      ) : null}

      {voiceAliasConflict ? (
        <SettingsCallout>
          <p className="text-sm leading-6" style={{ color: "rgba(252,165,165,0.95)" }}>
            {tr(
              `alias 충돌: ${voiceAliasConflict.firstAgent.name} "${voiceAliasConflict.firstAlias}" ↔ ${voiceAliasConflict.secondAgent.name} "${voiceAliasConflict.secondAlias}" (${voiceAliasConflict.normalized})`,
              `Alias collision: ${voiceAliasConflict.firstAgent.name} "${voiceAliasConflict.firstAlias}" ↔ ${voiceAliasConflict.secondAgent.name} "${voiceAliasConflict.secondAlias}" (${voiceAliasConflict.normalized})`,
            )}
          </p>
        </SettingsCallout>
      ) : null}

      {renderSettingGroupCard({
        titleKo: "Voice lobby",
        titleEn: "Voice lobby",
        descriptionKo: "lobby 채널, active-agent TTL, 기본 민감도와 버전입니다.",
        descriptionEn: "Lobby channel, active-agent TTL, default sensitivity, and version.",
        totalCount: 4,
        rows: visibleGlobalCards,
      })}

      <SettingsSubsection
        title={tr("에이전트 음성 라우팅", "Agent voice routing")}
        description={tr(
          "각 agent의 음성 활성화, wake word, 호출 alias, 민감도 override를 편집합니다.",
          "Edit each agent's voice enablement, wake word, spoken aliases, and sensitivity override.",
        )}
      >
        <div className="grid gap-3">
          {agentCards.length > 0 ? (
            agentCards
          ) : (
            <SettingsEmptyState className="text-sm">
              {tr("검색 결과가 없습니다.", "No matching agents.")}
            </SettingsEmptyState>
          )}
        </div>
      </SettingsSubsection>
    </div>
  );
}
