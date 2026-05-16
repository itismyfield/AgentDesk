import { Suspense, lazy } from "react";
import type { CSSProperties } from "react";

import type { GitHubRepoOption } from "../../api";
import type { Agent } from "../../types";
import {
  SurfaceCallout as SettingsCallout,
  SurfaceEmptyState as SettingsEmptyState,
  SurfaceSubsection as SettingsSubsection,
} from "../common/SurfacePrimitives";
import {
  CompactFieldCard,
  GroupLabel,
} from "./SettingsPanels";
import { SettingsAuditNotes } from "./SettingsKnowledge";
import {
  ADVANCED_PIPELINE_CATEGORIES,
  PRIMARY_PIPELINE_CATEGORIES,
  SYSTEM_CATEGORY_META,
  formatPipelineAgentLabel,
  type ConfigEntry,
  type SettingRowMeta,
} from "./SettingsModel";
import type {
  RenderSettingGroupCard,
  RenderSettingRow,
  SettingsTr,
} from "./SettingsPanelTypes";

const FsmEditor = lazy(() => import("../agent-manager/FsmEditor"));
const PipelineVisualEditor = lazy(() => import("../agent-manager/PipelineVisualEditor"));

interface SettingsPipelinePanelProps {
  configDirty: boolean;
  configEntries: ConfigEntry[];
  configSaving: boolean;
  groupedConfigEntries: Record<string, ConfigEntry[]>;
  inputStyle: CSSProperties;
  isKo: boolean;
  onConfigSave: () => Promise<void>;
  pipelineAgents: Agent[];
  pipelineMetas: SettingRowMeta[];
  pipelineRepos: GitHubRepoOption[];
  pipelineSelectorError: string | null;
  pipelineSelectorLoading: boolean;
  primaryActionClass: string;
  primaryActionStyle: CSSProperties;
  renderSettingGroupCard: RenderSettingGroupCard;
  renderSettingRow: RenderSettingRow;
  selectedPipelineAgentId: string | null;
  selectedPipelineRepo: string;
  setSelectedPipelineAgentId: (agentId: string | null) => void;
  setSelectedPipelineRepo: (repo: string) => void;
  tr: SettingsTr;
}

export function SettingsPipelinePanel({
  configDirty,
  configEntries,
  configSaving,
  groupedConfigEntries,
  inputStyle,
  isKo,
  onConfigSave,
  pipelineAgents,
  pipelineMetas,
  pipelineRepos,
  pipelineSelectorError,
  pipelineSelectorLoading,
  primaryActionClass,
  primaryActionStyle,
  renderSettingGroupCard,
  renderSettingRow,
  selectedPipelineAgentId,
  selectedPipelineRepo,
  setSelectedPipelineAgentId,
  setSelectedPipelineRepo,
  tr,
}: SettingsPipelinePanelProps) {
  const renderPipelineCategory = (categoryKey: keyof typeof SYSTEM_CATEGORY_META) => {
    const entries = groupedConfigEntries[categoryKey] ?? [];
    if (entries.length === 0) return null;
    const meta = SYSTEM_CATEGORY_META[categoryKey];
    const metasInCategory = entries
      .map((entry) => pipelineMetas.find((m) => m.key === entry.key))
      .filter((m): m is SettingRowMeta => Boolean(m));
    return renderSettingGroupCard({
      titleKo: meta.titleKo,
      titleEn: meta.titleEn,
      descriptionKo: meta.descriptionKo,
      descriptionEn: meta.descriptionEn,
      totalCount: metasInCategory.length,
      rows: metasInCategory.map((m) => {
        const trailingMeta = m.key.endsWith("_channel_id") ? (
          <span style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "Discord channel ID는 정밀도 손실을 피하려고 문자열로 유지합니다.",
              "Discord channel IDs stay as strings to avoid precision loss.",
            )}
          </span>
        ) : null;
        return renderSettingRow(m, { trailingMeta });
      }),
    });
  };

  return (
    <div className="space-y-5">
      {configEntries.length === 0 ? (
        <SettingsEmptyState className="text-sm">
          {tr("파이프라인 설정을 불러오는 중...", "Loading pipeline config...")}
        </SettingsEmptyState>
      ) : (
        <div className="space-y-5">
          <SettingsCallout
            action={(
              <button
                onClick={onConfigSave}
                disabled={configSaving || !configDirty}
                className={primaryActionClass}
                style={primaryActionStyle}
              >
                {configSaving ? tr("저장 중...", "Saving...") : tr("파이프라인 저장", "Save pipeline")}
              </button>
            )}
          >
            <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "이 섹션은 whitelist된 개별 `kv_meta` 키만 편집합니다. read-only 항목도 숨기지 않고 현재 상태를 드러내며, `context_clear_*` 같은 API 바깥 항목은 아래 audit 노트에서 별도로 정리합니다.",
                "This section edits only whitelisted individual `kv_meta` keys. Read-only items remain visible as status, and API-outside items such as `context_clear_*` are tracked in the audit notes below.",
              )}
            </p>
          </SettingsCallout>

          <SettingsSubsection
            title={tr("FSM 비주얼 에디터", "FSM visual editor")}
            description={tr(
              "repo/agent 범위를 먼저 고른 뒤, 상태 전환 event·hook·policy를 전용 FSM 캔버스에서 조정합니다.",
              "Pick the repo or agent scope first, then tune transition events, hooks, and policies on the dedicated FSM canvas.",
            )}
          >
            {pipelineSelectorLoading && pipelineRepos.length === 0 ? (
              <SettingsEmptyState className="text-sm">
                {tr("파이프라인 에디터 대상을 불러오는 중...", "Loading pipeline editor targets...")}
              </SettingsEmptyState>
            ) : pipelineSelectorError ? (
              <SettingsEmptyState className="text-sm">
                {pipelineSelectorError}
              </SettingsEmptyState>
            ) : pipelineRepos.length === 0 ? (
              <SettingsEmptyState className="text-sm">
                {tr("편집 가능한 repo가 없습니다.", "No editable repositories are available.")}
              </SettingsEmptyState>
            ) : (
              <div className="space-y-4">
                <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_220px]">
                  <CompactFieldCard
                    label={tr("대상 repo", "Target repo")}
                    description={tr(
                      "기본 FSM은 repo 레벨에서 편집하고, 필요할 때만 agent override로 내려갑니다.",
                      "Start at the repo-level FSM and only drop to an agent override when needed.",
                    )}
                  >
                    <select
                      value={selectedPipelineRepo}
                      onChange={(event) => setSelectedPipelineRepo(event.target.value)}
                      className="w-full rounded-2xl px-3 py-2.5 text-sm"
                      style={inputStyle}
                    >
                      {pipelineRepos.map((repo) => (
                        <option key={repo.nameWithOwner} value={repo.nameWithOwner}>
                          {repo.nameWithOwner}
                        </option>
                      ))}
                    </select>
                  </CompactFieldCard>
                  <CompactFieldCard
                    label={tr("에이전트 override", "Agent override")}
                    description={tr(
                      "선택하면 editor 안에서 agent 레벨 전환을 활성화합니다.",
                      "Selecting an agent enables the agent-level path inside the editor.",
                    )}
                  >
                    <select
                      value={selectedPipelineAgentId ?? ""}
                      onChange={(event) => setSelectedPipelineAgentId(event.target.value || null)}
                      className="w-full rounded-2xl px-3 py-2.5 text-sm"
                      style={inputStyle}
                    >
                      <option value="">{tr("없음", "None")}</option>
                      {pipelineAgents.map((agent) => (
                        <option key={agent.id} value={agent.id}>
                          {formatPipelineAgentLabel(agent, isKo)}
                        </option>
                      ))}
                    </select>
                  </CompactFieldCard>
                </div>

                {selectedPipelineRepo ? (
                  <div className="space-y-4">
                    <Suspense
                      fallback={(
                        <SettingsEmptyState className="text-sm">
                          {tr("FSM 에디터를 준비하는 중...", "Preparing FSM editor...")}
                        </SettingsEmptyState>
                      )}
                    >
                      <FsmEditor
                        tr={tr}
                        locale={isKo ? "ko" : "en"}
                        repo={selectedPipelineRepo}
                        agents={pipelineAgents}
                        selectedAgentId={selectedPipelineAgentId}
                      />
                    </Suspense>

                    <SettingsSubsection
                      title={tr("고급 / Agent별 파이프라인 편집기", "Advanced / agent-specific pipeline editor")}
                      description={tr(
                        "FSM 바깥의 state hook, timeout, phase gate, stage 실행 순서는 아래 고급 편집기에서 따로 다룹니다.",
                        "State hooks, timeouts, phase gates, and stage execution stay in the advanced editor below.",
                      )}
                    >
                      <Suspense
                        fallback={(
                          <SettingsEmptyState className="text-sm">
                            {tr("고급 파이프라인 편집기를 준비하는 중...", "Preparing advanced pipeline editor...")}
                          </SettingsEmptyState>
                        )}
                      >
                        <PipelineVisualEditor
                          tr={tr}
                          locale={isKo ? "ko" : "en"}
                          repo={selectedPipelineRepo}
                          agents={pipelineAgents}
                          selectedAgentId={selectedPipelineAgentId}
                          variant="advanced"
                        />
                      </Suspense>
                    </SettingsSubsection>
                  </div>
                ) : (
                  <SettingsEmptyState className="text-sm">
                    {tr("repo를 선택하면 FSM 에디터가 열립니다.", "Select a repo to open the FSM editor.")}
                  </SettingsEmptyState>
                )}
              </div>
            )}
          </SettingsSubsection>

          <div className="space-y-3">
            <GroupLabel title={tr("자주 쓰는 설정", "Frequent settings")} />
            {PRIMARY_PIPELINE_CATEGORIES.map(renderPipelineCategory)}
          </div>
          <div className="space-y-3">
            <GroupLabel title={tr("고급 설정", "Advanced settings")} />
            {ADVANCED_PIPELINE_CATEGORIES.map(renderPipelineCategory)}
          </div>

          <SettingsAuditNotes isKo={isKo} />
        </div>
      )}
    </div>
  );
}
