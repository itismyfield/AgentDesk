import { ArrowLeft, ArrowRight, TriangleAlert } from "lucide-react";
import { useId, useMemo, useRef, useState, type ReactNode } from "react";
import type { Campaign } from "../../api/campaigns";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { Select } from "../../kit/free/select-clean";
import { Cap, KitCard, MeterBar, Num, RowButton, Well } from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import CampaignGroupOverview, { buildCampaignGroupOverview, groupCycleText } from "./CampaignGroupOverview";
import CampaignGroupTasks from "./CampaignGroupTasks";
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

/** Phones: each group as a row with its task count, completion and blocked count, under any loop among the groups. */
function GroupList({ campaign, onOpen, tr, children }: { campaign: Campaign; onOpen: (group: string) => void; tr: Tr; children: ReactNode }) {
  const overview = useMemo(() => buildCampaignGroupOverview(campaign.nodes), [campaign.nodes]);
  return <div className="grid min-w-0 gap-3 p-3">
    {overview.cycles.map((path) => <Well key={path.join(">")} className="grid gap-0.5 bg-[var(--err-soft)] px-3 py-2.5">
      <strong className="flex items-center gap-1.5 text-sm text-[var(--err)]"><TriangleAlert aria-hidden className="size-4 shrink-0" /><span className="min-w-0 [overflow-wrap:anywhere]">{groupCycleText(overview, path, tr)}</span></strong>
      <Cap className="text-[var(--fg-dim)]">{tr("그룹끼리 서로 선행으로 이어져 있어요.", "These groups depend on each other in a loop.")}</Cap>
    </Well>)}
    <ul aria-label={tr("그룹 목록", "Groups")} className="m-0 grid list-none gap-0.5 p-0">{overview.groups.map((group) => {
      const { counts, percent, total } = group.progress;
      return <li key={group.id}><RowButton data-group={group.key} onClick={() => onOpen(group.key)} className="gap-3 rounded-[var(--r-sm)] px-3 py-2 sm:px-3">
        <span className="grid min-w-0 flex-1 gap-1.5">
          <span className="flex min-w-0 items-baseline gap-2"><strong className="min-w-0 truncate text-sm font-medium text-[var(--fg)]">{group.key || tr("미분류", "Ungrouped")}</strong><Cap className="shrink-0"><Num>{tr(`작업 ${total}개`, `${total} tasks`)}</Num></Cap></span>
          <MeterBar ratio={percent / 100} tone="ok" />
        </span>
        <span className="grid min-w-10 shrink-0 justify-items-end gap-0.5 text-xs"><Num className="text-[var(--fg)]">{percent}%</Num>{counts.blocked > 0 && <span className="text-[var(--err)]">{tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)}</span>}</span>
      </RowButton></li>;
    })}</ul>
    {children}
  </div>;
}

/** 관계도: the task picker and 편집 over the group graph, an opened group's task graph (lists, on phones) or a task's
 * connections. A task opened from a group keeps the group on screen, so closing it returns focus to its card or row. */
export default function CampaignRelations({ campaign, selectedId, onOpen, onEdit, tr }: {
  campaign: Campaign; selectedId: string | null; onOpen: (id: string | null) => void; onEdit?: (scope: EditScope) => void; tr: Tr;
}) {
  const [chosen, setChosen] = useState<string | null>(null);
  const [group, setGroup] = useState<string | null>(null);
  const [task, setTask] = useState<string | null>(null);
  const listId = useId();
  const root = useRef<HTMLDivElement>(null);
  const selected = campaign.nodes.find((node) => node.id === selectedId);
  const byId = new Map(campaign.nodes.map((node) => [node.id, node]));
  const all = new Set(campaign.nodes.map((node) => node.id));
  const members = group === null ? [] : campaign.nodes.filter((node) => campaignGroup(node) === group);
  const groups = new Set(campaign.nodes.map(campaignGroup)).size;
  const kept = keptDraft(campaign.id) !== null;
  const phone = useIsMobile();
  const inGroup = !phone && group !== null && (!selected || campaignGroup(selected) === group);
  const editScope = (): EditScope => inGroup ? { kind: "group", key: group } : selected ? { kind: "task", id: selected.id, depth: 1 }
    : { kind: "group", key: chosen ?? group ?? campaignGroup(campaign.nodes.find((node) => node.dependencies.length) ?? campaign.nodes[0]) };
  /** Puts the focus on `selector` inside the view once the screen it belongs to is drawn. */
  const focus = (selector: string) => requestAnimationFrame(() => requestAnimationFrame(() => root.current?.querySelector<HTMLElement>(selector)?.focus()));
  /** Opening a group draws its tasks in the canvas's place (its list, on phones) and moves the focus there. */
  const openGroup = (key: string, id: string | null = null) => {
    setGroup(key); setChosen(key); setTask(id);
    if (!id) focus(phone ? "[data-relations-list] [aria-expanded]" : "[data-relations-back]");
  };
  const goto = (id: string) => { const node = byId.get(id); if (node) openGroup(campaignGroup(node), id); };
  const back = () => { const left = group; setGroup(null); setTask(null); if (left !== null) focus(`.react-flow__node[data-id="campaign-group:${CSS.escape(encodeURIComponent(left))}"]`); };
  const pick = (value: string) => { const node = byId.get(value); if (inGroup && node && campaignGroup(node) === group) setTask(value); onOpen(value || null); };
  const name = (key: string) => key || tr("미분류", "Ungrouped");
  return <div ref={root} className="campaign-relations grid min-w-0 gap-3">
    <div className="min-w-0 overflow-hidden rounded-[var(--r-md)] border border-[var(--line-soft)] bg-[var(--bg-1)]">
      <header className="flex min-h-14 min-w-0 flex-wrap items-center gap-x-4 gap-y-2 border-b border-[var(--line-soft)] px-4 py-2">
        {inGroup ? <div className="flex min-w-0 items-center gap-1">
          <IconButton data-relations-back icon={<ArrowLeft aria-hidden />} label={tr("전체 그룹으로 돌아가기", "Back to all groups")} onClick={back} />
          <nav aria-label={tr("이동 줄", "Breadcrumb")} className="min-w-0"><ol className="m-0 flex min-w-0 list-none items-center gap-1 p-0 text-[15px]">
            <li><Button variant="ghost" onClick={back}>{tr("전체 그룹", "All groups")}</Button></li>
            <li aria-hidden className="text-[var(--fg-muted)]">›</li>
            <li aria-current="page" className="min-w-0 truncate font-semibold text-[var(--fg)]">{name(group)}</li>
          </ol></nav>
          <Cap className="shrink-0 pl-2"><Num>{tr(`작업 ${members.length}개`, `${members.length} tasks`)}</Num></Cap>
        </div> : selected ? <Button variant="ghost" icon={<ArrowLeft aria-hidden className="size-4" />} onClick={() => onOpen(null)}>{!phone && group !== null ? tr("그룹 보기로", "Back to the group") : tr("전체 그룹 관계", "All group connections")}</Button>
          : <div className="grid min-w-0 gap-0.5"><strong className="truncate text-[15px] text-[var(--fg)]">{tr("그룹 관계", "Group connections")}</strong>
            <Cap><Num>{tr(`그룹 ${groups}개 · 작업 ${campaign.nodes.length}개`, `${groups} groups · ${campaign.nodes.length} tasks`)}</Num></Cap></div>}
        <div className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-2 max-sm:w-full">
          {onEdit && kept && <KitBadge size="md" tone="warning">{tr("저장하지 않은 편집이 있어요", "Unsaved edits are waiting")}</KitBadge>}
          <div className="campaign-node-picker min-w-0 flex-1 sm:w-72 sm:flex-none">
            <Select aria-label={tr("연결을 볼 작업", "Task to inspect")} size="lg" placeholder={tr("작업을 선택하세요", "Select a task")} value={selectedId}
              options={campaign.nodes.map((node) => ({ value: node.id, label: node.title }))} onChange={pick} />
          </div>
          {onEdit && <Button variant="secondary" size="lg" onClick={() => onEdit(editScope())}>{kept ? tr("이어서 편집", "Continue editing") : tr("편집", "Edit")}</Button>}
        </div>
      </header>
      {selected && !inGroup ? <div className="p-4"><CampaignNeighborhood nodes={campaign.nodes} selectedId={selected.id} visibleIds={all} onSelect={onOpen} tr={tr} /></div>
        : phone ? <GroupList campaign={campaign} onOpen={openGroup} tr={tr}><TieLines campaign={campaign} tr={tr} /></GroupList>
        : inGroup ? <CampaignGroupTasks key={group} campaign={campaign} group={group} chosen={task} onChoose={setTask} onGoto={goto} onGotoGroup={(key) => openGroup(key)}
          onOpenTask={(id) => { setTask(id); onOpen(id); }} tr={tr} />
        : <CampaignGroupOverview campaignId={campaign.id} nodes={campaign.nodes} chosen={chosen} onChoose={setChosen} onOpenGroup={openGroup} onOpenTask={goto} tr={tr} />}
    </div>
    {phone && group !== null && <div data-relations-list><KitCard pad="none">
      <div className="grid min-w-0 gap-0.5 p-1.5">
        <TaskGroupHeading open controls={listId} label={name(group)} count={members.length} onToggle={() => setGroup(null)} />
        <div id={listId} className="grid min-w-0 gap-0.5">{members.map((node) => {
          const after = campaign.nodes.filter((other) => other.dependencies.includes(node.id)).length, before = node.dependencies.filter((id) => byId.has(id)).length;
          return <RowButton key={node.id} data-node-id={node.id} data-task-id={node.id} selected={node.id === selectedId} onClick={() => onOpen(node.id)}
            className="campaign-task-row grid gap-0.5 rounded-[var(--r-sm)] py-2 text-[13px]"><span className="min-w-0 truncate">{node.title}</span>
            <Cap><Num>{tr(`선행 ${before} · 후행 ${after}`, `${before} before · ${after} after`)}</Num></Cap></RowButton>;
        })}</div>
      </div>
    </KitCard></div>}
  </div>;
}
