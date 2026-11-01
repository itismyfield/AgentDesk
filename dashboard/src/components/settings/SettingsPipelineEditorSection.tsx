import { Suspense, lazy } from "react";
import { formatPipelineAgentLabel, type SettingsNotificationType } from "./SettingsModel";
import { SETTINGS_INPUT_STYLE, SettingsDisclosure, SettingsStatusMessage } from "./SettingsPanels";
import type { SettingsPanelContext } from "./SettingsPanelTypes";
import { sectionTitle } from "./SettingsRowSection";
import { useSettingsPipelineSelector } from "./useSettingsPipelineSelector";

const PipelineVisualEditor = lazy(() => import("../agent-manager/PipelineVisualEditor"));

interface Props {
  ctx: SettingsPanelContext;
  open: boolean;
  onToggle: () => void;
  notify: (ko: string, en: string, type?: SettingsNotificationType) => void;
}

/** The visual pipeline editor stays behind “고급 편집” and loads its data only once opened. */
export function SettingsPipelineEditorSection({ ctx, open, onToggle, notify }: Props) {
  const { tr, isKo } = ctx;
  const selector = useSettingsPipelineSelector({ enabled: open, notify, tr });
  const { title, description } = sectionTitle(ctx, "workflow.editor");

  return (
    <SettingsDisclosure
      title={title}
      description={description}
      openLabel={tr("고급 편집", "Advanced editing")}
      closeLabel={tr("편집기 닫기", "Close editor")}
      open={open}
      onToggle={onToggle}
      variant="plain"
      testId="settings-pipeline-editor-section"
    >
      <p className="mb-4 text-xs leading-5 break-keep text-th-text-muted">
        {tr(
          "편집기 안에서 바꾼 내용은 편집기의 저장 버튼으로 따로 저장돼요.",
          "Changes made inside the editor are saved with the editor's own Save button.",
        )}
      </p>
      {selector.pipelineSelectorLoading && selector.pipelineRepos.length === 0 ? (
        <SettingsStatusMessage>{tr("편집할 저장소를 불러오는 중…", "Loading repositories…")}</SettingsStatusMessage>
      ) : selector.pipelineSelectorError ? (
        <SettingsStatusMessage tone="error">{selector.pipelineSelectorError}</SettingsStatusMessage>
      ) : selector.pipelineRepos.length === 0 ? (
        <SettingsStatusMessage>{tr("편집할 수 있는 저장소가 없어요.", "No repositories can be edited.")}</SettingsStatusMessage>
      ) : (
        <div className="space-y-4">
          <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_minmax(0,16rem)]">
            <label className="min-w-0 text-sm font-medium text-th-text-primary">
              {tr("대상 저장소", "Repository")}
              <span className="mt-0.5 block text-xs font-normal leading-5 break-keep text-th-text-muted">
                {tr("이 저장소의 작업 흐름을 바꿔요.", "Edit the workflow for this repository.")}
              </span>
              <select
                value={selector.selectedPipelineRepo}
                onChange={(event) => selector.setSelectedPipelineRepo(event.target.value)}
                className="mt-2 min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm font-normal"
                style={SETTINGS_INPUT_STYLE}
              >
                {selector.pipelineRepos.map((repo) => (
                  <option key={repo.nameWithOwner} value={repo.nameWithOwner}>{repo.nameWithOwner}</option>
                ))}
              </select>
            </label>
            <label className="min-w-0 text-sm font-medium text-th-text-primary">
              {tr("에이전트별 흐름", "Per-agent workflow")}
              <span className="mt-0.5 block text-xs font-normal leading-5 break-keep text-th-text-muted">
                {tr("한 에이전트만 다르게 할 때 골라요.", "Pick one only if a single agent needs a different flow.")}
              </span>
              <select
                value={selector.selectedPipelineAgentId ?? ""}
                onChange={(event) => selector.setSelectedPipelineAgentId(event.target.value || null)}
                className="mt-2 min-h-11 w-full min-w-0 rounded-md px-3 py-2 text-sm font-normal"
                style={SETTINGS_INPUT_STYLE}
              >
                <option value="">{tr("모든 에이전트 같게", "Same for all agents")}</option>
                {selector.pipelineAgents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{formatPipelineAgentLabel(agent, isKo)}</option>
                ))}
              </select>
            </label>
          </div>
          {selector.selectedPipelineRepo ? (
            <div data-testid="settings-pipeline-editor">
              <Suspense fallback={<SettingsStatusMessage>{tr("편집기를 준비하는 중…", "Preparing the editor…")}</SettingsStatusMessage>}>
                <PipelineVisualEditor
                  tr={tr}
                  locale={isKo ? "ko" : "en"}
                  repo={selector.selectedPipelineRepo}
                  agents={selector.pipelineAgents}
                  selectedAgentId={selector.selectedPipelineAgentId}
                  defaultCollapsed={false}
                />
              </Suspense>
            </div>
          ) : (
            <SettingsStatusMessage>{tr("저장소를 고르면 편집기가 열려요.", "Pick a repository to open the editor.")}</SettingsStatusMessage>
          )}
        </div>
      )}
    </SettingsDisclosure>
  );
}
