import { useEffect, useMemo, useState, type CSSProperties } from "react";

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
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const visibleAgents = useMemo(
    () =>
      voiceDraft?.agents.filter((agent) =>
        voiceAgentKeys(agent.id).some((key) => isRowVisible(key)),
      ) ?? [],
    [isRowVisible, voiceDraft],
  );

  useEffect(() => {
    if (visibleAgents.length === 0) {
      if (selectedAgentId !== null) {
        setSelectedAgentId(null);
      }
      return;
    }
    if (!selectedAgentId || !visibleAgents.some((agent) => agent.id === selectedAgentId)) {
      setSelectedAgentId(visibleAgents[0].id);
    }
  }, [selectedAgentId, visibleAgents]);

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
        label={tr("음성 채널 ID", "Voice channel ID")}
        description={tr(
          "음성 입력을 받을 Discord 채널입니다.",
          "Discord channel that receives voice input.",
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
        label={tr("대화 유지 시간", "Conversation handoff time")}
        description={tr(
          "이름을 다시 부르지 않아도 같은 에이전트에게 이어 말할 수 있는 시간입니다.",
          "How long follow-up speech keeps talking to the same agent without repeating a name.",
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
          "말이 겹칠 때 새 발화로 얼마나 쉽게 인식할지 정합니다.",
          "Controls how readily overlapping speech is treated as a new utterance.",
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
  ].filter(Boolean);

  const selectedAgent =
    (selectedAgentId ? visibleAgents.find((agent) => agent.id === selectedAgentId) : null)
    ?? visibleAgents[0]
    ?? null;
  const enabledAgentCount = voiceDraft.agents.filter((agent) => agent.voice_enabled).length;
  const selectedAgentCard = selectedAgent
    ? (() => {
      const agent = selectedAgent;
      const displayName = isKo && agent.name_ko ? agent.name_ko : agent.name;
      const secondaryName = isKo && agent.name !== displayName ? agent.name : agent.name_ko;
      const conflictInAgent =
        voiceAliasConflict &&
        (voiceAliasConflict.firstAgent.id === agent.id || voiceAliasConflict.secondAgent.id === agent.id);
      return (
        <SettingsCard
          key={agent.id}
          data-testid="voice-agent-card"
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
              {secondaryName ? (
                <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  {secondaryName}
                </div>
              ) : null}
            </div>
            <button
              type="button"
              role="switch"
              aria-label={tr(`${displayName} 음성 사용`, `${displayName} voice enabled`)}
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
                label={tr("호출어", "Wake word")}
                description={tr("비워 두면 호출 이름만으로 연결합니다.", "When empty, spoken names are enough.")}
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
                description={tr("이 에이전트만 다르게 인식하도록 조정합니다.", "Tune recognition for this agent only.")}
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
                label={tr("추가 호출 이름", "Additional spoken names")}
                description={tr("쉼표 또는 줄바꿈으로 여러 호출명을 입력합니다.", "Enter multiple spoken aliases separated by commas or new lines.")}
                footer={tr(
                  "에이전트 이름은 기본 호출명으로 자동 포함됩니다.",
                  "The agent's name is always included automatically.",
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
    })()
    : null;

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
            "음성 채널, 대화 유지 시간, 에이전트별 호출 이름을 조정합니다. 저장하면 다음 음성 입력부터 반영됩니다.",
            "Adjust the voice channel, conversation handoff time, and each agent's spoken names. Saved changes apply to the next voice input.",
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
              `호출 이름이 겹칩니다: ${voiceAliasConflict.firstAgent.name} "${voiceAliasConflict.firstAlias}" ↔ ${voiceAliasConflict.secondAgent.name} "${voiceAliasConflict.secondAlias}"`,
              `Spoken names overlap: ${voiceAliasConflict.firstAgent.name} "${voiceAliasConflict.firstAlias}" ↔ ${voiceAliasConflict.secondAgent.name} "${voiceAliasConflict.secondAlias}"`,
            )}
          </p>
        </SettingsCallout>
      ) : null}

      {renderSettingGroupCard({
        titleKo: "전체 음성 설정",
        titleEn: "Global voice settings",
        descriptionKo: "모든 에이전트에 공통으로 적용되는 기본값입니다.",
        descriptionEn: "Defaults shared by every agent.",
        totalCount: 3,
        rows: visibleGlobalCards,
      })}

      <SettingsSubsection
        title={tr("에이전트별 설정", "Per-agent settings")}
        description={tr(
          "필요한 에이전트를 선택해 음성 사용 여부와 호출 이름을 조정합니다.",
          "Choose one agent at a time to adjust voice enablement and spoken names.",
        )}
      >
        <div className="grid gap-3 lg:grid-cols-[minmax(220px,0.8fr)_minmax(0,1.6fr)]">
          <SettingsCard
            className="rounded-2xl p-4"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
              background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
            }}
          >
            <label
              className="block text-xs font-semibold"
              style={{ color: "var(--th-text)" }}
              htmlFor="voice-agent-selector"
            >
              {tr("설정할 에이전트", "Agent to configure")}
            </label>
            <select
              id="voice-agent-selector"
              data-testid="voice-agent-selector"
              value={selectedAgent?.id ?? ""}
              onChange={(event) => setSelectedAgentId(event.target.value || null)}
              className="mt-3 w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
              disabled={visibleAgents.length === 0}
            >
              {visibleAgents.map((agent) => {
                const displayName = isKo && agent.name_ko ? agent.name_ko : agent.name;
                return (
                  <option key={agent.id} value={agent.id}>
                    {displayName}
                  </option>
                );
              })}
            </select>
            <p className="mt-3 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                `${enabledAgentCount}명 활성 / 전체 ${voiceDraft.agents.length}명`,
                `${enabledAgentCount} enabled / ${voiceDraft.agents.length} total`,
              )}
            </p>
          </SettingsCard>
          {selectedAgentCard ? (
            selectedAgentCard
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
