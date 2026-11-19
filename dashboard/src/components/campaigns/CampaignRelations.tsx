import { useId, useState } from "react";
import type { Campaign } from "../../api/campaigns";
import { Button } from "../../kit/free/button-clean";
import { Select } from "../common/Select";
import CampaignGroupOverview from "./CampaignGroupOverview";
import CampaignNeighborhood from "./CampaignNeighborhood";
import { campaignGroup } from "./campaignModel";
import type { Tr } from "./campaignPresentation";
import { keptDraft, type EditScope } from "./relationsEditorModel";
import { TaskGroupHeading } from "./TaskListParts";

/** 관계도: the group graph, or the open task's direct connections. A chosen group's task list stays while a task is
 * open, so closing the task returns focus to its row. `onEdit` (wide screens only) opens the editor on the open task's
 * neighbors, else the chosen group, else the group of the first task with a dependency. */
export default function CampaignRelations({ campaign, selectedId, onOpen, onEdit, tr }: {
  campaign: Campaign; selectedId: string | null; onOpen: (id: string | null) => void; onEdit?: (scope: EditScope) => void; tr: Tr;
}) {
  const [group, setGroup] = useState<string | null>(null);
  const listId = useId();
  const selected = campaign.nodes.find((node) => node.id === selectedId);
  const all = new Set(campaign.nodes.map((node) => node.id));
  const members = group === null ? [] : campaign.nodes.filter((node) => campaignGroup(node) === group);
  const kept = keptDraft(campaign.id) !== null;
  const editScope = (): EditScope => selected ? { kind: "task", id: selected.id, depth: 1 }
    : { kind: "group", key: group ?? campaignGroup(campaign.nodes.find((node) => node.dependencies.length) ?? campaign.nodes[0]) };
  return <div className="campaign-relations grid min-w-0 gap-3">
    <div className="flex min-w-0 flex-wrap items-end gap-3">
      <label className="campaign-node-picker min-w-0 flex-1">{tr("연결을 볼 작업", "Task to inspect")}<Select value={selectedId ?? ""} onChange={(event) => onOpen(event.target.value || null)}>
        <option value="" disabled>{tr("작업을 선택하세요", "Select a task")}</option>
        {campaign.nodes.map((node) => <option key={node.id} value={node.id}>{node.title}</option>)}
      </Select></label>
      {onEdit && <div className="flex items-center gap-2">
        {kept && <span className="text-xs text-[var(--warn)]">{tr("저장하지 않은 편집이 있어요", "Unsaved edits are waiting")}</span>}
        <Button variant="secondary" onClick={() => onEdit(editScope())}>{kept ? tr("이어서 편집", "Continue editing") : tr("편집", "Edit")}</Button>
      </div>}
    </div>
    {selected ? <>
      <div><Button variant="secondary" onClick={() => onOpen(null)}>{tr("전체 그룹 관계", "All group connections")}</Button></div>
      <CampaignNeighborhood nodes={campaign.nodes} selectedId={selected.id} visibleIds={all} onSelect={onOpen} tr={tr} />
    </> : <CampaignGroupOverview campaignId={campaign.id} revision={campaign.revision} nodes={campaign.nodes} tr={tr} onSelectGroup={setGroup} />}
    {group !== null && <div className="grid min-w-0 gap-0.5">
      <TaskGroupHeading open controls={listId} label={group || tr("미분류", "Ungrouped")} count={members.length} onToggle={() => setGroup(null)} />
      <div id={listId} className="grid min-w-0 gap-0.5">{members.map((node) => <button key={node.id} type="button" data-node-id={node.id} data-task-id={node.id} aria-current={node.id === selectedId || undefined} onClick={() => onOpen(node.id)}
        className="campaign-task-row !m-0 w-full min-w-0 truncate !rounded-[var(--r-sm)] !border-0 !bg-transparent !px-3 !py-2 text-left !text-[13px] hover:!bg-[color-mix(in_oklch,var(--fg)_5%,transparent)]">{node.title}</button>)}</div>
    </div>}
  </div>;
}
