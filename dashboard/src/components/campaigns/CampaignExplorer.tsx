import { useMemo, useRef, useState, type KeyboardEvent } from "react";
import type { Campaign, CampaignNodeStatus } from "../../api/campaigns";
import { WidgetState } from "../common/WidgetState";
import CampaignNodeDetails, { type CampaignDraft } from "./CampaignNodeDetails";
import CampaignNeighborhood from "./CampaignNeighborhood";
import CampaignGroupOverview from "./CampaignGroupOverview";
import CampaignGlance from "./CampaignGlance";
import { EMPTY_FILTERS, NODE_STATUSES, campaignIssueLabel, filterCampaignNodes, groupCampaignNodes, type CampaignFilters } from "./campaignModel";
import { Badge, LABELS, type Tr } from "./campaignPresentation";

export default function CampaignExplorer({ campaign, tr, onSaved, drafts, onDraftChange }: {
  campaign: Campaign; tr: Tr; onSaved: (campaign: Campaign) => void;
  drafts: Record<string, CampaignDraft>; onDraftChange: (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => void;
}) {
  const [filters, setFilters] = useState<CampaignFilters>(EMPTY_FILTERS);
  const [collapsed, setCollapsed] = useState<Set<string>>(() => {
    const initialGroups = groupCampaignNodes(campaign.nodes);
    return new Set(campaign.nodes.length > 40 && initialGroups.length > 1 ? initialGroups.map((group) => group.key) : []);
  });
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [view, setView] = useState<"list" | "graph">("list");
  const [filtersOpen, setFiltersOpen] = useState(false);
  const rowRefs = useRef(new Map<string, HTMLButtonElement>());
  const inspectorRef = useRef<HTMLElement>(null);
  const groups = useMemo(() => groupCampaignNodes(campaign.nodes), [campaign.nodes]);
  const filtered = useMemo(() => filterCampaignNodes(campaign.nodes, filters), [campaign.nodes, filters]);
  const visibleIds = useMemo(() => new Set(filtered.map((node) => node.id)), [filtered]);
  const visibleGroups = groups.map((group) => ({ ...group, visible: group.nodes.filter((node) => visibleIds.has(node.id)) })).filter((group) => group.visible.length > 0);
  const visibleRows = visibleGroups.flatMap((group) => collapsed.has(group.key) ? [] : group.visible);
  const selected = campaign.nodes.find((node) => node.id === selectedId);
  const patchFilters = (patch: Partial<CampaignFilters>) => {
    const next = { ...filters, ...patch };
    setFilters(next);
    const matches = groupCampaignNodes(filterCampaignNodes(campaign.nodes, next));
    setCollapsed((current) => { const expanded = new Set(current); matches.forEach((group) => expanded.delete(group.key)); return expanded; });
  };
  const selectNode = (id: string, reveal = false) => {
    setSelectedId(id);
    if (reveal || window.matchMedia?.("(max-width: 900px)").matches) window.requestAnimationFrame(() => inspectorRef.current?.scrollIntoView({ behavior: "smooth", block: "start" }));
  };
  const toggleGroup = (key: string) => setCollapsed((current) => { const next = new Set(current); if (next.has(key)) next.delete(key); else next.add(key); return next; });
  const navigateRows = (event: KeyboardEvent, id: string) => {
    const index = visibleRows.findIndex((node) => node.id === id);
    const target = event.key === "ArrowDown" ? visibleRows[index + 1] : event.key === "ArrowUp" ? visibleRows[index - 1] : event.key === "Home" ? visibleRows[0] : event.key === "End" ? visibleRows.at(-1) : undefined;
    if (target) { event.preventDefault(); rowRefs.current.get(target.id)?.focus(); }
  };
  const hiddenDependencies = selected?.dependencies.filter((id) => !visibleIds.has(id)) ?? [];
  return <div className="campaign-explorer">
    <CampaignGlance nodes={campaign.nodes} tr={tr} onOpen={(id) => selectNode(id, true)} />
    <div className={`campaign-toolbar ${filtersOpen ? "filters-open" : ""}`}>
      <label className="campaign-search"><span>{tr("작업 검색", "Search tasks")}</span><input type="search" aria-label={tr("작업 검색", "Search tasks")} placeholder={tr("제목, 번호, 담당, 세션…", "Title, ID, assignee, session…")} value={filters.query} onChange={(event) => patchFilters({ query: event.target.value })} /></label>
      <button className="campaign-filter-toggle" aria-expanded={filtersOpen} onClick={() => setFiltersOpen((current) => !current)}>{tr("필터", "Filters")}{(filters.status !== "all" || filters.group !== null || filters.hideCompleted) ? " •" : ""}</button>
      <label><span>{tr("상태 필터", "Status filter")}</span><select value={filters.status} onChange={(event) => patchFilters({ status: event.target.value as CampaignNodeStatus | "all" })}><option value="all">{tr("모든 상태", "All statuses")}</option>{NODE_STATUSES.map((status) => <option key={status} value={status}>{tr(...LABELS[status])}</option>)}</select></label>
      <label><span>{tr("그룹 필터", "Group filter")}</span><select value={JSON.stringify(filters.group)} onChange={(event) => patchFilters({ group: JSON.parse(event.target.value) as string | null })}><option value="null">{tr("모든 그룹", "All groups")}</option>{groups.map((group) => <option key={group.key} value={JSON.stringify(group.key)}>{group.key || tr("미분류", "Ungrouped")}</option>)}</select></label>
      <label className="campaign-check"><input type="checkbox" checked={filters.hideCompleted} onChange={(event) => patchFilters({ hideCompleted: event.target.checked })} />{tr("완료 숨김", "Hide completed")}</label>
    </div>
    <div className="campaign-row campaign-explorer-meta"><span role="status">{tr(`작업 ${filtered.length}/${campaign.nodes.length}개 · 그룹 ${visibleGroups.length}개`, `${filtered.length}/${campaign.nodes.length} tasks · ${visibleGroups.length} groups`)}</span><div className="campaign-view-toggle" aria-label={tr("보기 방식", "View mode")}><button aria-pressed={view === "list"} onClick={() => setView("list")}>{tr("그룹 목록", "Grouped list")}</button><button aria-pressed={view === "graph"} onClick={() => setView("graph")}>{tr("연결 보기", "Connections")}</button></div></div>
    <div className={`campaign-workbench ${selected ? "has-inspector" : ""}`}>
      <div className="campaign-browser">
        {view === "list" ? <>
          <div className="campaign-list-tools"><button onClick={() => setCollapsed(new Set())}>{tr("모두 펼치기", "Expand all")}</button><button onClick={() => setCollapsed(new Set(groups.map((group) => group.key)))}>{tr("모두 접기", "Collapse all")}</button></div>
          {filtered.length === 0 && <WidgetState kind="empty" title={tr("조건에 맞는 작업이 없습니다.", "No matching tasks.")} action={<button onClick={() => setFilters(EMPTY_FILTERS)}>{tr("필터 초기화", "Reset filters")}</button>} />}
          <div className="campaign-group-list" aria-label={tr("그룹별 작업", "Tasks by group")}>
            {visibleGroups.map((group, groupIndex) => <section className="campaign-task-group" key={group.key}>
              <button className="campaign-group-heading" aria-expanded={!collapsed.has(group.key)} aria-controls={`campaign-group-${groupIndex}`} onClick={() => toggleGroup(group.key)}><span aria-hidden>{collapsed.has(group.key) ? "▸" : "▾"}</span><strong>{group.key || tr("미분류", "Ungrouped")}</strong><span>{group.visible.length === group.nodes.length ? group.nodes.length : `${group.visible.length}/${group.nodes.length}`}</span><span className="campaign-group-progress">{group.progress.counts.completed}/{group.nodes.length} {tr("완료", "done")}</span><span className="campaign-mini-progress" aria-label={`${group.progress.percent}%`}><i style={{ width: `${group.progress.percent}%` }} /></span><span className="campaign-group-active">{tr(`진행 ${group.progress.counts.running}`, `${group.progress.counts.running} running`)}</span>{group.progress.counts.blocked > 0 && <span className="campaign-group-blocked">{tr(`막힘 ${group.progress.counts.blocked}`, `${group.progress.counts.blocked} blocked`)}</span>}</button>
              {!collapsed.has(group.key) && <div id={`campaign-group-${groupIndex}`}>
                <div className="campaign-task-columns" aria-hidden><span>{tr("작업", "Task")}</span><span>{tr("상태", "Status")}</span><span>{tr("단계", "Stage")}</span><span>{tr("회차", "Round")}</span><span>{tr("담당", "Assignee")}</span></div>
                {group.visible.map((node) => { const hidden = node.dependencies.filter((id) => !visibleIds.has(id)).length; return <button key={node.id} ref={(element) => { if (element) rowRefs.current.set(node.id, element); else rowRefs.current.delete(node.id); }} className="campaign-task-row" data-node-id={node.id} aria-pressed={node.id === selectedId} onClick={() => selectNode(node.id)} onKeyDown={(event) => navigateRows(event, node.id)}><span className="campaign-task-title" title={`${node.id} · ${node.title}`}><span className="campaign-task-id">{campaignIssueLabel(node)}</span><strong>{node.title}</strong>{drafts[`${campaign.id}:${node.id}`] && <small title={tr("수정 중인 초안", "Unsaved draft")}>*</small>}{hidden > 0 && <small title={tr("필터 밖 선행 작업", "Dependencies outside filters")}>↳ {hidden}</small>}</span><Badge status={node.status} tr={tr} /><span className="campaign-task-stage" title={node.stage}>{node.stage}</span><span className="campaign-task-round">R{node.round}</span><span className="campaign-task-owner" title={node.assignee || ""}>{node.assignee || "—"}</span></button>; })}
              </div>}
            </section>)}
          </div>
        </> : <>
          <label className="campaign-node-picker">{tr("연결을 볼 작업", "Task to inspect")}<select value={selectedId ?? ""} onChange={(event) => selectNode(event.target.value)}><option value="" disabled>{tr("작업을 선택하세요", "Select a task")}</option>{filtered.map((node) => <option key={node.id} value={node.id}>{node.title}</option>)}{selected && !visibleIds.has(selected.id) && <option value={selected.id}>{selected.title} · {tr("필터 밖", "outside filters")}</option>}</select></label>
          {selected ? <><button onClick={() => setSelectedId(null)}>{tr("전체 그룹 관계", "All group connections")}</button><CampaignNeighborhood nodes={campaign.nodes} selectedId={selected.id} visibleIds={visibleIds} onSelect={selectNode} tr={tr} /></> : <CampaignGroupOverview nodes={campaign.nodes} tr={tr} onSelectGroup={(group) => { setFilters({ ...EMPTY_FILTERS, group }); setCollapsed((current) => { const next = new Set(current); next.delete(group); return next; }); setView("list"); }} />}
        </>}
      </div>
      {selected ? <aside ref={inspectorRef} className="campaign-inspector" aria-label={tr("선택 작업", "Selected task")}>
        <div className="campaign-row campaign-inspector-header"><span>{selected.id}</span><button onClick={() => setSelectedId(null)} aria-label={tr("상세 닫기", "Close details")}>×</button></div>
        {!visibleIds.has(selected.id) && <p className="campaign-filter-note">{tr("선택 작업은 현재 필터 밖에 있습니다.", "The selected task is outside the current filters.")}</p>}
        {hiddenDependencies.length > 0 && <p className="campaign-filter-note">{tr(`필터 밖 선행 작업 ${hiddenDependencies.length}개. 연결 보기에서 확인할 수 있습니다.`, `${hiddenDependencies.length} dependencies are outside the filters. Open Connections to inspect them.`)}</p>}
        <CampaignNodeDetails key={selected.id} campaign={campaign} node={selected} tr={tr} onSaved={onSaved} editing={drafts[`${campaign.id}:${selected.id}`] ?? null} onDraftChange={(draft, expected) => onDraftChange(`${campaign.id}:${selected.id}`, draft, expected)} />
      </aside> : <aside className="campaign-inspector-placeholder"><span>↗</span><p>{tr("작업을 선택해 담당 세션과 다음 행동을 확인하세요.", "Select a task to inspect its session and next action.")}</p></aside>}
    </div>
  </div>;
}
