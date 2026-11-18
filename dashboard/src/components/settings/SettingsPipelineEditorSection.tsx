import { Suspense, lazy } from "react";
import { formatPipelineAgentLabel, type SettingsNotificationType } from "./SettingsModel";
import { Select } from "../../kit/free/select-clean";
import { SettingsDisclosure, SettingsStatusMessage } from "./SettingsPrimitives";
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
          <div className="grid gap-4 @min-[37.5rem]:grid-cols-2">
            <Select
              label={tr("대상 저장소", "Repository")}
              hint={tr("이 저장소의 작업 흐름을 바꿔요.", "Edit the workflow for this repository.")}
              options={selector.pipelineRepos.map((repo) => ({ value: repo.nameWithOwner, label: repo.nameWithOwner }))}
              value={selector.selectedPipelineRepo}
              onChange={(value) => selector.setSelectedPipelineRepo(value)}
              className="min-w-0"
            />
            <Select
              label={tr("에이전트별 흐름", "Per-agent workflow")}
              hint={tr("한 에이전트만 다르게 할 때 골라요.", "Pick one only if a single agent needs a different flow.")}
              options={[
                { value: "", label: tr("모든 에이전트 같게", "Same for all agents") },
                ...selector.pipelineAgents.map((agent) => ({ value: agent.id, label: formatPipelineAgentLabel(agent, isKo) })),
              ]}
              value={selector.selectedPipelineAgentId ?? ""}
              onChange={(value) => selector.setSelectedPipelineAgentId(value || null)}
              className="min-w-0"
            />
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
