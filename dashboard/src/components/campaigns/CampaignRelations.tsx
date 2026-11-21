import { ArrowRight } from "lucide-react";
import { useId, useState } from "react";
import type { Campaign } from "../../api/campaigns";
import { Button } from "../../kit/free/button-clean";
import { Select } from "../../kit/free/select-clean";
import { Cap, KitCard, RowButton, Well } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import CampaignGroupOverview from "./CampaignGroupOverview";
import { campaignRawEdges, rawEdgeId } from "./campaignGraphRouting";
import CampaignNeighborhood from "./CampaignNeighborhood";
import { campaignGroup, campaignTaskTitle } from "./campaignModel";
import type { Tr } from "./campaignPresentation";
import { loadCampaignView, storedEditorScopes } from "./campaignViewStorage";
import { keptDraft, type EditScope } from "./relationsEditorModel";
import { TaskGroupHeading } from "./TaskListParts";

/** The cable ties saved for this campaign's editor scopes, one line each: how many lines, from which tasks, and on the
 * second line the tasks they lead to. Members gone from the campaign are left out. */
function TieLines({ campaign, tr }: { campaign: Campaign; tr: Tr }) {
  const present = new Set(campaignRawEdges(campaign.nodes).map(rawEdgeId));
  const title = (id: string) => { const node = campaign.nodes.find((value) => value.id === id); return node ? campaignTaskTitle(node) : id; };
  const ties = storedEditorScopes(campaign.id).flatMap((scope) => loadCampaignView(scope).view.ties)
    .map((tie) => ({ id: tie.id, edges: tie.strandOrder.filter((edge) => present.has(rawEdgeId(edge))) })).filter((tie) => tie.edges.length);
  if (!ties.length) return null;
  return <ul aria-label={tr("케이블타이", "Cable ties")} className="m-0 grid list-none gap-1.5 p-0">{ties.map(({ id, edges }) => {
    const from = [...new Set(edges.map(([dep]) => dep))], to = [...new Set(edges.map(([, next]) => next))];
    const more = from.length - 1;
    return <li key={id}><Well className="flex items-center gap-3 px-3 py-2.5">
      <i aria-hidden className="size-4 shrink-0 rounded-full border-2 border-[var(--fg-muted)]" />
      <span className="grid min-w-0 flex-1 gap-0.5">
        <strong className="flex min-w-0 gap-1 text-sm font-medium text-[var(--fg)]"><span className="truncate">{tr(`묶음 ${edges.length} · ${title(from[0])}`, `Bundle of ${edges.length} · ${title(from[0])}`)}</span>
          {more > 0 && <>{" "}<span className="shrink-0">{tr(`외 ${more}`, `and ${more} more`)}</span></>}</strong>
        <Cap className="flex min-w-0 items-center gap-1"><ArrowRight aria-hidden className="size-3 shrink-0" /><span className="sr-only">{tr("이어지는 작업", "Leads to")}</span><span className="truncate">{to.map(title).join(" · ")}</span></Cap>
      </span>
      <Cap className="shrink-0">{tr("보기만", "View only")}</Cap>
    </Well></li>;
  })}</ul>;
}

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
  const phone = useIsMobile();
  const editScope = (): EditScope => selected ? { kind: "task", id: selected.id, depth: 1 }
    : { kind: "group", key: group ?? campaignGroup(campaign.nodes.find((node) => node.dependencies.length) ?? campaign.nodes[0]) };
  return <div className="campaign-relations grid min-w-0 gap-3">
    <div className="flex min-w-0 flex-wrap items-end gap-3">
      <div className="campaign-node-picker min-w-0 flex-1">
        <Select label={tr("연결을 볼 작업", "Task to inspect")} aria-label={tr("연결을 볼 작업", "Task to inspect")} size="lg"
          placeholder={tr("작업을 선택하세요", "Select a task")} value={selectedId}
          options={campaign.nodes.map((node) => ({ value: node.id, label: node.title }))}
          onChange={(value) => onOpen(value || null)} />
      </div>
      {onEdit && <div className="flex items-center gap-2">
        {kept && <span className="text-xs text-[var(--warn)]">{tr("저장하지 않은 편집이 있어요", "Unsaved edits are waiting")}</span>}
        <Button variant="secondary" onClick={() => onEdit(editScope())}>{kept ? tr("이어서 편집", "Continue editing") : tr("편집", "Edit")}</Button>
      </div>}
    </div>
    {selected ? <>
      <div><Button variant="secondary" size="sm" onClick={() => onOpen(null)}>{tr("전체 그룹 관계", "All group connections")}</Button></div>
      <CampaignNeighborhood nodes={campaign.nodes} selectedId={selected.id} visibleIds={all} onSelect={onOpen} tr={tr} />
    </> : <>{phone && <TieLines campaign={campaign} tr={tr} />}<CampaignGroupOverview campaignId={campaign.id} nodes={campaign.nodes} tr={tr} onSelectGroup={setGroup} /></>}
    {group !== null && <KitCard pad="none">
      <div className="grid min-w-0 gap-0.5 p-1.5">
        <TaskGroupHeading open controls={listId} label={group || tr("미분류", "Ungrouped")} count={members.length} onToggle={() => setGroup(null)} />
        <div id={listId} className="grid min-w-0 gap-0.5">{members.map((node) => <RowButton key={node.id} data-node-id={node.id} data-task-id={node.id} selected={node.id === selectedId} onClick={() => onOpen(node.id)}
          className="campaign-task-row rounded-[var(--r-sm)] py-2 text-[13px]"><span className="min-w-0 truncate">{node.title}</span></RowButton>)}</div>
      </div>
    </KitCard>}
  </div>;
}
