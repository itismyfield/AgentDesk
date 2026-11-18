import { ListTree, Search, SlidersHorizontal, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { Campaign, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { Button } from "../../kit/free/button-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select as KitSelect } from "../../kit/free/select-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { Drawer } from "../common/overlay/Drawer";
import { DrawerDockContext } from "../common/overlay/DrawerDock";
import { Select } from "../common/Select";
import CampaignNodeDetails, { type CampaignDraft } from "./CampaignNodeDetails";
import CampaignNeighborhood from "./CampaignNeighborhood";
import CampaignGroupOverview from "./CampaignGroupOverview";
import CampaignGlance from "./CampaignGlance";
import { EMPTY_FILTERS, NODE_STATUSES, campaignIssueLabel, filterCampaignNodes, groupCampaignNodes, type CampaignFilters } from "./campaignModel";
import { CAMPAIGN_SCOPE, LABELS, type Tr } from "./campaignPresentation";
import CampaignTaskRow from "./CampaignTaskRow";
import { DetailDockPane, TaskGroupHeading, moveListFocus, useDetailDock } from "./TaskListParts";

type View = "list" | "graph";

export default function CampaignExplorer({ campaign, live, tr, onSaved, drafts, onDraftChange, initialSelectedId = null, onSelectedChange }: {
  campaign: Campaign; live: Record<string, CampaignNodeLive>; tr: Tr; onSaved: (campaign: Campaign) => void;
  drafts: Record<string, CampaignDraft>; onDraftChange: (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => void;
  /** A task named by a deep link, and where to report the selected task for the URL. */
  initialSelectedId?: string | null; onSelectedChange?: (id: string | null) => void;
}) {
  const [filters, setFilters] = useState<CampaignFilters>(EMPTY_FILTERS);
  const [collapsed, setCollapsed] = useState<Set<string>>(() => {
    const initialGroups = groupCampaignNodes(campaign.nodes);
    return new Set(campaign.nodes.length > 40 && initialGroups.length > 1 ? initialGroups.map((group) => group.key) : []);
  });
  const [selectedId, setSelectedId] = useState<string | null>(initialSelectedId);
  useEffect(() => onSelectedChange?.(selectedId), [selectedId, onSelectedChange]);
  const [view, setView] = useState<View>("list");
  const [filtersOpen, setFiltersOpen] = useState(false);
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());
  // Wide viewports keep the task docked beside the list; phones and tablets open it as a bottom sheet.
  const detailDock = useDetailDock();
  const controls = detailDock.dock ? detailDock.id : undefined;
  const groups = useMemo(() => groupCampaignNodes(campaign.nodes), [campaign.nodes]);
  const filtered = useMemo(() => filterCampaignNodes(campaign.nodes, filters), [campaign.nodes, filters]);
  const visibleIds = useMemo(() => new Set(filtered.map((node) => node.id)), [filtered]);
  const visibleGroups = groups.map((group) => ({ ...group, visible: group.nodes.filter((node) => visibleIds.has(node.id)) })).filter((group) => group.visible.length > 0);
  const selected = campaign.nodes.find((node) => node.id === selectedId);
  const filtering = filters.status !== "all" || filters.group !== null || filters.hideCompleted;
  const patchFilters = (patch: Partial<CampaignFilters>) => {
    const next = { ...filters, ...patch };
    setFilters(next);
    const matches = groupCampaignNodes(filterCampaignNodes(campaign.nodes, next));
    setCollapsed((current) => { const expanded = new Set(current); matches.forEach((group) => expanded.delete(group.key)); return expanded; });
  };
  const toggleGroup = (key: string) => setCollapsed((current) => { const next = new Set(current); if (next.has(key)) next.delete(key); else next.add(key); return next; });
  const groupName = (key: string) => key || tr("미분류", "Ungrouped");
  const hiddenDependencies = selected?.dependencies.filter((id) => !visibleIds.has(id)) ?? [];
  // Closing returns focus to the row that opened the task, when it is still on screen.
  const close = () => { const row = selectedId ? rowRefs.current.get(selectedId) : undefined; setSelectedId(null); row?.focus({ preventScroll: true }); };
  return <div ref={detailDock.root} className="campaign-explorer @container/tasks grid min-w-0 gap-4" style={detailDock.rootStyle}>
    {/* Search and the filter switch share the first row, the filters the second; a wide list puts all on one row.
        A narrow list keeps the filters behind the switch. */}
    <div className="campaign-toolbar grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-center gap-2 @3xl/tasks:grid-cols-[minmax(0,1fr)_repeat(2,10rem)_auto]">
      <TextField type="search" value={filters.query} onChange={(event) => patchFilters({ query: event.target.value })}
        aria-label={tr("작업 검색", "Search tasks")} placeholder={tr("제목, 번호, 담당, 세션…", "Title, ID, assignee, session…")} prefix={<Search aria-hidden />} />
      <div className="@min-[35rem]/tasks:hidden">
        <Button variant="secondary" icon={<SlidersHorizontal aria-hidden />} aria-expanded={filtersOpen} onClick={() => setFiltersOpen((current) => !current)}>
          {tr("필터", "Filters")}{filtering ? " •" : ""}
        </Button>
      </div>
      <div className={`col-span-2 grid min-w-0 grid-cols-2 items-center gap-2 @min-[35rem]/tasks:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] @3xl/tasks:contents ${filtersOpen ? "" : "@max-[35rem]/tasks:hidden"}`}>
        <KitSelect aria-label={tr("상태 필터", "Status filter")} value={filters.status} onChange={(value) => patchFilters({ status: value as CampaignNodeStatus | "all" })}
          options={[{ value: "all", label: tr("모든 상태", "All statuses") }, ...NODE_STATUSES.map((status) => ({ value: status, label: tr(...LABELS[status]) }))]} />
        <KitSelect aria-label={tr("그룹 필터", "Group filter")} value={JSON.stringify(filters.group)} onChange={(value) => patchFilters({ group: JSON.parse(value) as string | null })}
          options={[{ value: "null", label: tr("모든 그룹", "All groups") }, ...groups.map((group) => ({ value: JSON.stringify(group.key), label: groupName(group.key) }))]} />
        <div className="col-span-2 @min-[35rem]/tasks:col-span-1">
          <Checkbox className="campaign-hide-completed" label={tr("완료 숨김", "Hide completed")} checked={filters.hideCompleted} onChange={(event) => patchFilters({ hideCompleted: event.target.checked })} />
        </div>
      </div>
    </div>
    <CampaignGlance nodes={campaign.nodes} live={live} tr={tr} onOpen={setSelectedId} selectedId={selectedId} controls={controls} />
    <div className="campaign-explorer-meta flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2">
      <p role="status" className="mr-auto text-xs text-[var(--fg-muted)]">{tr(`작업 ${filtered.length}/${campaign.nodes.length}개 · 그룹 ${visibleGroups.length}개`, `${filtered.length}/${campaign.nodes.length} tasks · ${visibleGroups.length} groups`)}</p>
      {view === "list" && <div className="campaign-list-tools flex gap-1">
        <Button variant="ghost" size="sm" onClick={() => setCollapsed(new Set())}>{tr("모두 펼치기", "Expand all")}</Button>
        <Button variant="ghost" size="sm" onClick={() => setCollapsed(new Set(groups.map((group) => group.key)))}>{tr("모두 접기", "Collapse all")}</Button>
      </div>}
      {/* The kit control fills its container, so the container gets the width of two short labels. */}
      <div className="w-[12rem] max-w-full">
        <SegmentedControl<View> size="sm" aria-label={tr("보기 방식", "View mode")} value={view} onChange={setView}
          options={[{ value: "list", label: tr("그룹 목록", "Grouped list") }, { value: "graph", label: tr("연결 보기", "Connections") }]} />
      </div>
    </div>
    {view === "list" ? <>
      {filtered.length === 0 && <EmptyState icon={<ListTree aria-hidden />} title={tr("조건에 맞는 작업이 없습니다.", "No matching tasks.")} text={tr("검색어나 필터를 바꿔 보세요.", "Try another search or filter.")}
        action={<Button variant="secondary" onClick={() => setFilters(EMPTY_FILTERS)}>{tr("필터 초기화", "Reset filters")}</Button>} />}
      <ul className="campaign-group-list grid min-w-0 gap-3" aria-label={tr("그룹별 작업", "Tasks by group")} onKeyDown={moveListFocus}>
        {visibleGroups.map((group, groupIndex) => {
          const open = !collapsed.has(group.key);
          const listId = `${detailDock.id}-group-${groupIndex}`;
          const { counts, percent } = group.progress;
          return <li className="campaign-task-group grid min-w-0 gap-0.5" key={group.key}>
            <TaskGroupHeading className="campaign-group-heading" open={open} controls={listId} label={groupName(group.key)} title={group.key || undefined}
              count={group.visible.length < group.nodes.length ? `${group.visible.length}/${group.nodes.length}` : group.nodes.length} onToggle={() => toggleGroup(group.key)}>
              <span className="ml-auto flex shrink-0 items-center gap-2 pl-2 text-xs font-normal tabular-nums text-[var(--fg-muted)]">
                {counts.running > 0 && <span className="text-[var(--info)]">{tr(`진행 ${counts.running}`, `${counts.running} running`)}</span>}
                {counts.blocked > 0 && <span className="text-[var(--warn)]">{tr(`막힘 ${counts.blocked}`, `${counts.blocked} blocked`)}</span>}
                <span>{counts.completed}/{group.nodes.length} {tr("완료", "done")}</span>
                <span className="h-1 w-10 overflow-hidden rounded-full bg-[var(--line)] @max-[35rem]/tasks:hidden" aria-hidden><span className="block h-full bg-[var(--ok)]" style={{ width: `${percent}%` }} /></span>
              </span>
            </TaskGroupHeading>
            {open && <div id={listId} className="grid min-w-0 gap-0.5">
              {group.visible.map((node) => <CampaignTaskRow key={node.id} node={node} live={live[node.id]} selected={node.id === selectedId} draft={!!drafts[`${campaign.id}:${node.id}`]}
                hiddenDependencies={node.dependencies.filter((id) => !visibleIds.has(id)).length} controls={controls} tr={tr}
                rowRef={(element) => { if (element) rowRefs.current.set(node.id, element); else rowRefs.current.delete(node.id); }}
                onSelect={() => setSelectedId(node.id)} />)}
            </div>}
          </li>;
        })}
      </ul>
    </> : <div className="grid min-w-0 gap-3">
      <label className="campaign-node-picker">{tr("연결을 볼 작업", "Task to inspect")}<Select value={selectedId ?? ""} onChange={(event) => setSelectedId(event.target.value)}><option value="" disabled>{tr("작업을 선택하세요", "Select a task")}</option>{filtered.map((node) => <option key={node.id} value={node.id}>{node.title}</option>)}{selected && !visibleIds.has(selected.id) && <option value={selected.id}>{selected.title} · {tr("필터 밖", "outside filters")}</option>}</Select></label>
      {selected ? <><div><Button variant="secondary" onClick={() => setSelectedId(null)}>{tr("전체 그룹 관계", "All group connections")}</Button></div><CampaignNeighborhood nodes={campaign.nodes} selectedId={selected.id} visibleIds={visibleIds} onSelect={setSelectedId} tr={tr} /></> : <CampaignGroupOverview campaignId={campaign.id} revision={campaign.revision} nodes={campaign.nodes} tr={tr} onSelectGroup={(group) => { setFilters({ ...EMPTY_FILTERS, group }); setCollapsed((current) => { const next = new Set(current); next.delete(group); return next; }); setView("list"); }} />}
    </div>}
    <DetailDockPane dock={detailDock} label={tr("선택 작업", "Selected task")} empty={selected ? null : {
      title: tr("고른 작업이 없습니다", "No task selected"),
      text: tr("작업을 선택해 담당 세션과 다음 행동을 확인하세요.", "Select a task to inspect its session and next action."),
    }} />
    <DrawerDockContext.Provider value={detailDock.dock}>
      {selected && <Drawer open onClose={close} ariaLabel={tr("선택 작업", "Selected task")} width="min(560px, 100vw)">
        <div className={`${CAMPAIGN_SCOPE} campaign-inspector min-w-0 gap-3 text-sm [overflow-wrap:anywhere]`}>
          <div className="campaign-inspector-header flex items-center justify-between gap-3 border-b border-[var(--line)] pb-2 text-xs text-[var(--fg-muted)]">
            <span className="tabular-nums">{campaignIssueLabel(selected)}</span>
            <IconButton icon={<X aria-hidden />} label={tr("상세 닫기", "Close details")} onClick={close} />
          </div>
          {!visibleIds.has(selected.id) && <p className="campaign-filter-note">{tr("선택 작업은 현재 필터 밖에 있습니다.", "The selected task is outside the current filters.")}</p>}
          {hiddenDependencies.length > 0 && <p className="campaign-filter-note">{tr(`필터 밖 선행 작업 ${hiddenDependencies.length}개. 연결 보기에서 확인할 수 있습니다.`, `${hiddenDependencies.length} dependencies are outside the filters. Open Connections to inspect them.`)}</p>}
          <CampaignNodeDetails key={selected.id} campaign={campaign} node={selected} live={live[selected.id]} tr={tr} onSaved={onSaved} editing={drafts[`${campaign.id}:${selected.id}`] ?? null} onDraftChange={(draft, expected) => onDraftChange(`${campaign.id}:${selected.id}`, draft, expected)} />
        </div>
      </Drawer>}
    </DrawerDockContext.Provider>
  </div>;
}
