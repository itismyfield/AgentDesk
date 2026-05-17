import { Fragment, Suspense, lazy } from "react";
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
                "작업이 어떤 단계로 이동하는지 정합니다. 변경 후 저장하면 이후 작업 흐름에 반영됩니다.",
                "Set how work moves between stages. Saved changes apply to upcoming workflow changes.",
              )}
            </p>
          </SettingsCallout>

          <SettingsSubsection
            title={tr("세부 흐름 편집기", "Detailed workflow editor")}
            description={tr(
              "대상을 고른 뒤 단계, 전환, 실행 순서를 한 화면에서 조정합니다.",
              "Pick a target, then adjust stages, transitions, and execution order in one editor.",
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
                    label={tr("대상 저장소", "Target repository")}
                    description={tr(
                      "기본 작업 흐름을 적용할 저장소입니다.",
                      "The repository this default workflow applies to.",
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
                    label={tr("에이전트별 조정", "Agent-specific adjustments")}
                    description={tr(
                      "특정 에이전트에만 다른 흐름을 적용할 때 선택합니다.",
                      "Choose this only when one agent needs a different workflow.",
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
                          {tr("세부 흐름 편집기를 준비하는 중...", "Preparing workflow editor...")}
                        </SettingsEmptyState>
                      )}
                    >
                      <PipelineVisualEditor
                        tr={tr}
                        locale={isKo ? "ko" : "en"}
                        repo={selectedPipelineRepo}
                        agents={pipelineAgents}
                        selectedAgentId={selectedPipelineAgentId}
                        defaultCollapsed={false}
                      />
                    </Suspense>
                  </div>
                ) : (
                  <SettingsEmptyState className="text-sm">
                    {tr("저장소를 선택하면 편집기가 열립니다.", "Select a repository to open the editor.")}
                  </SettingsEmptyState>
                )}
              </div>
            )}
          </SettingsSubsection>

          <div className="space-y-3">
            <GroupLabel title={tr("자주 쓰는 설정", "Frequent settings")} />
            {PRIMARY_PIPELINE_CATEGORIES.map((category) => (
              <Fragment key={category}>{renderPipelineCategory(category)}</Fragment>
            ))}
          </div>
          <div className="space-y-3">
            <GroupLabel title={tr("고급 설정", "Advanced settings")} />
            {ADVANCED_PIPELINE_CATEGORIES.map((category) => (
              <Fragment key={category}>{renderPipelineCategory(category)}</Fragment>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
