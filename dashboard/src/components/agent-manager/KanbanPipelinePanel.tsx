import type { Agent, UiLanguage } from "../../types";
import AutoQueuePanel from "./AutoQueuePanel";
import PipelineConfigView from "./PipelineConfigView";
import PipelineOverrideEditor from "./PipelineOverrideEditor";
import PipelineEditor from "./PipelineEditor";

interface KanbanPipelinePanelProps {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  agents: Agent[];
  selectedRepo: string;
  selectedAgentId: string | null;
}

export default function KanbanPipelinePanel({
  tr,
  locale,
  agents,
  selectedRepo,
  selectedAgentId,
}: KanbanPipelinePanelProps) {
  return (
    <>
      <AutoQueuePanel
        tr={tr}
        locale={locale}
        agents={agents}
        selectedRepo={selectedRepo}
        selectedAgentId={selectedAgentId}
      />
      <PipelineConfigView
        tr={tr}
        locale={locale}
        repo={selectedRepo}
        agents={agents}
        selectedAgentId={selectedAgentId}
      />
      <PipelineOverrideEditor
        tr={tr}
        locale={locale}
        repo={selectedRepo}
        agents={agents}
        selectedAgentId={selectedAgentId}
      />
      <PipelineEditor
        tr={tr}
        locale={locale}
        repo={selectedRepo}
        agents={agents}
        selectedAgentId={selectedAgentId}
      />
    </>
  );
}
