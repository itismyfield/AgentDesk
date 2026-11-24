import { useId, useMemo, useRef, type KeyboardEvent } from "react";
import { Pencil, Plus, Search, UsersRound, X } from "lucide-react";
import { formatElapsedCompact } from "../../agent-insights";
import type { Agent, Department, DispatchedSession } from "../../types";
import { localeName } from "../../i18n";
import { isTestAgent } from "../../lib/agentHelpers";
import { Button } from "../../kit/free/button-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Select } from "../../kit/free/select-clean";
import { TextField } from "../../kit/free/text-field-clean";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { ActionMenu } from "../common/ActionMenu";
import { Cap, KitCard, SectionCard, StatusDot, type DotTone } from "../common/KitPrimitives";
import { AGENT_STATES, agentLine, agentStateLabel, monitorView, type HomeAgent, type HomeAgentState } from "../home/homeModel";
import { useAgentTurns } from "../home/useHomeData";
import { UnlinkedSessions } from "../session-panel/SessionPanel";
import AgentInfoCard from "./AgentInfoCard";
import { AgentInfoCardContent } from "./AgentInfoCardContent";
import AgentRow from "./AgentRow";
import { AgentsAttentionCard, useAgentsAttention } from "./AgentsAttentionCard";
import { AgentsSummaryPane } from "./AgentsSummaryPane";
import { NO_DEPARTMENT, filterAgents, hasDepartment, isListedAgent, isStandIn, type AgentStatusFilter } from "./agent-list-controls";
import type { Translator } from "./types";
import type { AgentSortMode } from "./useAgentManagerController";
import { useAgentDetailShared } from "./useAgentDetailData";
import { AgentManagerStyles } from "./AgentManagerStyles";

interface AgentsTabProps {
  tr: Translator;
  locale: string;
  isKo: boolean;
  /** Every agent with its home state, plus home's department-only session stand-ins (counted, never listed). */
  entries: HomeAgent[];
  /** The entries the filters keep, in list order. */
  sortedEntries: HomeAgent[];
  departments: Department[];
  deptTab: string;
  setDeptTab: (deptId: string) => void;
  search: string;
  setSearch: (next: string) => void;
  statusFilter: AgentStatusFilter;
  setStatusFilter: (next: AgentStatusFilter) => void;
  sortMode: AgentSortMode;
  setSortMode: (next: AgentSortMode) => void;
  includeTests: boolean;
  setIncludeTests: (next: boolean) => void;
  selectedAgentId: string | null;
  onSelectAgent: (agentId: string | null) => void;
  /** Live sessions no agent owns: a summary line on desktop, a card under the list on phones. */
  unlinkedSessions?: DispatchedSession[];
  onCreateAgent: () => void;
  onEditAgent: (agent: Agent) => void;
  onDuplicateAgent: (agent: Agent) => void;
  onEditDepartment: (department: Department) => void;
  onDeleteAgent: (agentId: string) => void;
  onAgentUpdated: () => void;
  saving: boolean;
}

const LEGEND_TONE: Record<HomeAgentState, DotTone> = { working: "ok", background: "warn", resting: "neutral", noSession: "neutral", unknown: "neutral" };

/** The agents tab: what needs a look, then every agent with home's states, and on desktop the detail (or a summary
 * while none is picked) beside the list. Phones open the detail full screen. */
export default function AgentsTab({
  tr,
  locale,
  isKo,
  entries,
  sortedEntries,
  departments,
  deptTab,
  setDeptTab,
  search,
  setSearch,
  statusFilter,
  setStatusFilter,
  sortMode,
  setSortMode,
  includeTests,
  setIncludeTests,
  selectedAgentId,
  onSelectAgent,
  unlinkedSessions = [],
  onCreateAgent,
  onEditAgent,
  onDuplicateAgent,
  onEditDepartment,
  onDeleteAgent,
  onAgentUpdated,
  saving,
}: AgentsTabProps) {
  const isMobile = useIsMobile();
  const panelId = useId();
  const listRef = useRef<HTMLUListElement>(null);
  // Home's counted set; the rows and everything about real agents leave the stand-ins out.
  const counted = useMemo(() => entries.filter((entry) => isListedAgent(entry.agent, includeTests)), [entries, includeTests]);
  const listed = useMemo(() => counted.filter((entry) => !isStandIn(entry.agent)), [counted]);
  const standIns = counted.length - listed.length;
  const listedAgents = useMemo(() => listed.map((entry) => entry.agent), [listed]);
  const testAgentCount = entries.filter((entry) => isTestAgent(entry.agent)).length;
  const attention = useAgentsAttention(listedAgents, tr, isKo);
  // Routes, accounts and skill calls are read once here and handed to every detail. A new, removed or re-routed agent
  // reads the routes again, so a fresh agent never shows "no route".
  const routesKey = useMemo(() => entries.filter((entry) => !isStandIn(entry.agent)).map(({ agent }) =>
    [agent.id, agent.discord_channel_id, agent.discord_channel_alt, agent.discord_channel_cc, agent.discord_channel_cdx].join(":")).sort().join("|"), [entries]);
  const shared = useAgentDetailShared(true, selectedAgentId !== null, routesKey);
  const bound = useMemo(() => (shared.bindings ? new Set(shared.bindings.map((binding) => binding.agentId)) : null), [shared.bindings]);
  // Phones show no tool line, so they read no turns either (as home).
  const turns = useAgentTurns(isMobile ? [] : listed.filter((entry) => entry.state === "working").map((entry) => entry.agent.id));
  const departmentById = useMemo(() => new Map(departments.map((department) => [department.id, department])), [departments]);
  const known = useMemo(() => new Set(departmentById.keys()), [departmentById]);

  // Tab counts follow the other filters, so each number is what that tab would show.
  const statusCounts = useMemo(() => {
    const base = filterAgents(entries, { deptTab, statusFilter: "all", search, includeTests, departments: known });
    const counts: Record<string, number> = { all: base.length };
    for (const { state } of base) counts[state] = (counts[state] ?? 0) + 1;
    return counts;
  }, [entries, deptTab, includeTests, search, known]);
  const legend = AGENT_STATES.filter((state) => state !== "unknown" || statusCounts.unknown);
  // A picked tab stays while its count drops to zero, so the filter never hides behind a vanished tab.
  const statusKeys: AgentStatusFilter[] = ["all", ...AGENT_STATES.filter((state) => legend.includes(state) || state === statusFilter)];

  const selectedEntry = entries.find((entry) => entry.agent.id === selectedAgentId);
  const selected = selectedEntry?.agent ?? null;
  const reasons = attention.entries.find((entry) => entry.agent.id === selectedAgentId)?.reasons ?? [];
  const selectedDept = deptTab !== "all" ? departmentById.get(deptTab) : undefined;
  const filtersActive = search.trim() !== "" || statusFilter !== "all" || deptTab !== "all";
  const unassigned = listed.filter((entry) => !hasDepartment(entry.agent, known)).length;

  const lineOf = (entry: HomeAgent) => {
    const view = monitorView(turns.get(entry.agent.id), entry, tr);
    if (!view) return agentLine(entry, tr, isKo);
    // The clock goes first, so a long tool line truncates instead of it.
    const elapsed = view.started === null ? "" : ` ${formatElapsedCompact(Date.now() - view.started, isKo)}`;
    return `${agentStateLabel(entry.state, tr)}${elapsed} · ${view.label}`;
  };
  const needsCheck = (agent: Agent) => !hasDepartment(agent, known) || (bound !== null && !bound.has(agent.id));
  const anyCheck = sortedEntries.some((entry) => needsCheck(entry.agent));

  const onRowKey = (event: KeyboardEvent<HTMLButtonElement>) => {
    const step = { ArrowDown: 1, ArrowUp: -1, Home: -Infinity, End: Infinity }[event.key];
    if (step === undefined || event.altKey || event.metaKey || event.ctrlKey) return;
    event.preventDefault();
    const rows = Array.from(listRef.current?.querySelectorAll<HTMLButtonElement>("[data-agent-item]") ?? []);
    const index = Math.max(0, Math.min(rows.length - 1, rows.indexOf(event.currentTarget) + step));
    rows[index]?.focus();
    rows[index]?.scrollIntoView?.({ block: "nearest" });
  };

  const detailActions = (agent: Agent, closable: boolean) => {
    const name = localeName(locale, agent);
    return (
      <>
        <Button variant="secondary" size="sm" icon={<Pencil size={14} aria-hidden />} onClick={() => onEditAgent(agent)}>
          {tr("편집", "Edit")}
        </Button>
        <ActionMenu
          label={tr(`${name} 메뉴`, `${name} menu`)}
          items={[
            { key: "duplicate", label: tr("복제", "Duplicate"), onSelect: () => onDuplicateAgent(agent) },
            {
              key: "delete",
              label: tr("삭제", "Delete"),
              danger: true,
              disabled: saving || agent.status === "working",
              onSelect: () => onDeleteAgent(agent.id),
              confirm: {
                title: tr(`${name}을(를) 삭제할까요?`, `Delete ${name}?`),
                description: tr("에이전트 설정이 지워지고 되돌릴 수 없어요.", "The agent's settings are removed for good."),
                confirmLabel: tr("삭제", "Delete"),
                cancelLabel: tr("취소", "Cancel"),
              },
            },
          ]}
        />
        {closable ? (
          <IconButton icon={<X size={15} />} label={tr("상세 닫기", "Close details")} onClick={() => onSelectAgent(null)} />
        ) : null}
      </>
    );
  };

  const list =
    listed.length === 0 ? (
      <EmptyState
        icon={<UsersRound size={18} />}
        title={tr("아직 에이전트가 없어요", "No agents yet")}
        text={tr("에이전트를 추가하면 여기에서 상태와 설정을 볼 수 있어요.", "Add an agent to see its status and settings here.")}
        action={
          <Button variant="secondary" size="sm" icon={<Plus size={14} aria-hidden />} onClick={onCreateAgent}>
            {tr("에이전트 추가", "Add agent")}
          </Button>
        }
      />
    ) : sortedEntries.length === 0 ? (
      <EmptyState
        icon={<Search size={18} />}
        title={tr("검색 결과 없음", "No agents found")}
        text={tr("검색어나 상태·부서 조건을 바꿔 보세요.", "Try another search, status or department.")}
        action={
          filtersActive ? (
            <Button
              variant="secondary"
              size="sm"
              onClick={() => {
                setSearch("");
                setStatusFilter("all");
                setDeptTab("all");
              }}
            >
              {tr("조건 지우기", "Clear filters")}
            </Button>
          ) : undefined
        }
      />
    ) : (
      <ul ref={listRef} data-testid="agents-list" className="@container m-0 min-w-0 list-none p-0">
        {sortedEntries.map((entry) => (
          <li key={entry.agent.id} className="border-t border-(--line-soft) first:border-t-0">
            <AgentRow
              entry={entry}
              line={lineOf(entry)}
              department={entry.agent.department_id ? departmentById.get(entry.agent.department_id) : undefined}
              warn={needsCheck(entry.agent)}
              locale={locale}
              tr={tr}
              selected={entry.agent.id === selectedAgentId}
              onSelect={() => onSelectAgent(entry.agent.id)}
              onKeyDown={onRowKey}
            />
          </li>
        ))}
      </ul>
    );

  return (
    <div data-testid="agents-tab" className="agent-kit-scope min-w-0">
      <AgentManagerStyles />
      <div className="grid min-w-0 items-start gap-4 sm:grid-cols-[minmax(0,1fr)_minmax(18rem,38%)] xl:gap-5">
        <div className="flex min-w-0 flex-col gap-3 sm:gap-4">
          <AgentsAttentionCard attention={attention} locale={locale} isMobile={isMobile} tr={tr} onOpenAgent={onSelectAgent} />
          <SectionCard
            title={tr("모든 에이전트", "All agents")}
            count={sortedEntries.length}
            aside={isMobile ? null : (
              <Cap className="flex flex-wrap items-center gap-x-3.5 gap-y-1">
                {legend.map((state) => (
                  <span key={state} className="flex items-center gap-1.5 font-display tabular-nums">
                    <StatusDot tone={LEGEND_TONE[state]} />
                    {agentStateLabel(state, tr)} {statusCounts[state] ?? 0}
                  </span>
                ))}
              </Cap>
            )}
            testId="agents-roster"
          >
            <div className="@container flex min-w-0 flex-col gap-2 border-t border-(--line-soft) px-3 py-3 sm:px-4">
              {standIns > 0 ? (
                <Cap data-testid="agents-stand-ins">
                  {tr(`수에는 에이전트 없이 부서에만 붙은 세션 ${standIns}개도 들어 있어요(홈과 같은 기준).`, `Counts include ${standIns} sessions attached to a department only, as on home.`)}
                </Cap>
              ) : null}
              {attention.status === "empty" ? (
                <Cap data-testid="agents-attention-empty">{tr("막히거나 실패한 에이전트가 없어요.", "No agent is stuck or failing.")}</Cap>
              ) : null}
              <div className="grid min-w-0 grid-cols-1 items-center gap-2 @xl:grid-cols-[minmax(0,1fr)_10rem_8rem_auto]">
                <TextField
                  type="search"
                  aria-label={tr("에이전트 검색", "Search agents")}
                  placeholder={tr("이름·역할 검색", "Search name or role")}
                  prefix={<Search size={14} aria-hidden />}
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                  clearable
                  onClear={() => setSearch("")}
                  className="min-w-0"
                />
                <Select
                  aria-label={tr("소속 부서", "Department")}
                  value={deptTab}
                  onChange={setDeptTab}
                  className="order-last min-w-0 @xl:order-none"
                  options={[
                    { value: "all", label: tr(`전체 부서 · ${listed.length}명`, `All departments · ${listed.length}`) },
                    ...departments.map((department) => ({
                      value: department.id,
                      label: `${localeName(locale, department)} · ${listed.filter((entry) => entry.agent.department_id === department.id).length}`,
                    })),
                    ...(unassigned ? [{ value: NO_DEPARTMENT, label: tr(`미배정 · ${unassigned}`, `Unassigned · ${unassigned}`) }] : []),
                  ]}
                />
                {isMobile ? null : (
                  <Select
                    aria-label={tr("에이전트 정렬", "Sort agents")}
                    value={sortMode}
                    onChange={(value) => setSortMode(value as AgentSortMode)}
                    className="min-w-0"
                    options={[
                      { value: "status", label: tr("상태순", "By status") },
                      { value: "department", label: tr("부서순", "By department") },
                      { value: "name", label: tr("이름순", "By name") },
                      { value: "activity", label: tr("활동량순", "By activity") },
                      { value: "created", label: tr("생성일순", "By created date") },
                    ]}
                  />
                )}
                {testAgentCount > 0 ? (
                  <Checkbox
                    data-testid="agents-include-tests"
                    checked={includeTests}
                    onChange={(event) => setIncludeTests(event.target.checked)}
                    label={tr(`테스트 포함 (${testAgentCount})`, `Tests (${testAgentCount})`)}
                    className="order-last"
                  />
                ) : null}
                {/* The kit control is an inline-size container, so its grid slot carries the placement. */}
                <div className="min-w-0 @xl:order-last @xl:col-span-4">
                  <SegmentedControl
                    kind="tabs"
                    variant="tray"
                    aria-label={tr("에이전트 상태", "Agent status")}
                    value={statusFilter}
                    onChange={(value) => setStatusFilter(value as AgentStatusFilter)}
                    options={statusKeys.map((key) => ({
                      value: key,
                      label: key === "all" ? tr("전체", "All") : agentStateLabel(key, tr),
                      count: statusCounts[key] ?? 0,
                      controls: panelId,
                    }))}
                  />
                </div>
              </div>
              {selectedDept ? (
                <Button variant="ghost" size="sm" className="self-start" onClick={() => onEditDepartment(selectedDept)}>
                  {tr("부서 편집", "Edit department")}
                </Button>
              ) : null}
            </div>
            <div id={panelId} role="tabpanel" aria-label={statusFilter === "all" ? tr("전체", "All") : agentStateLabel(statusFilter, tr)} className="min-w-0 border-t border-(--line-soft)">
              {list}
            </div>
            {anyCheck && !isMobile ? (
              <Cap className="block border-t border-(--line-soft) px-4 py-2.5 text-right">
                {tr("주황 점은 설정 확인이 필요하다는 뜻이에요(부서나 Discord 경로 없음).", "An orange dot marks settings to check (no department or Discord route).")}
              </Cap>
            ) : null}
          </SectionCard>
          {isMobile && unlinkedSessions.length > 0 ? (
            <KitCard pad="sm">
              <UnlinkedSessions sessions={unlinkedSessions} departments={departments} locale={locale} tr={tr} isKo={isKo} />
            </KitCard>
          ) : null}
        </div>

        {!isMobile ? (
          <section
            data-testid="agents-detail-pane"
            aria-label={
              selected
                ? `${localeName(locale, selected)} — ${tr("에이전트 상세", "Agent details")}`
                : tr("에이전트 요약", "Agents summary")
            }
            className="sticky top-0 max-h-[calc(100dvh-9.5rem)] min-w-0 overflow-y-auto overscroll-contain rounded-(--r-xl) bg-(--bg-1) p-4 shadow-[inset_0_0_0_1px_var(--line-soft)] xl:p-5"
          >
            {selected ? (
              <AgentInfoCardContent
                key={selected.id}
                agent={selected}
                state={selectedEntry?.state}
                isKo={isKo}
                locale={locale}
                tr={tr}
                departments={departments}
                onAgentUpdated={onAgentUpdated}
                actions={detailActions(selected, true)}
                shared={shared}
                turns={turns}
                reasons={reasons}
              />
            ) : (
              <AgentsSummaryPane
                counted={counted}
                entries={listed}
                departments={departments}
                locale={locale}
                tr={tr}
                isKo={isKo}
                deptTab={deptTab}
                onSelectDept={setDeptTab}
                unlinkedSessions={unlinkedSessions}
              />
            )}
          </section>
        ) : null}
      </div>

      {isMobile && selected ? (
        <AgentInfoCard
          agent={selected}
          state={selectedEntry?.state}
          isKo={isKo}
          locale={locale}
          tr={tr}
          departments={departments}
          onClose={() => onSelectAgent(null)}
          onAgentUpdated={onAgentUpdated}
          backLabel={tr("목록으로", "Back to list")}
          actions={detailActions(selected, false)}
          shared={shared}
          reasons={reasons}
        />
      ) : null}
    </div>
  );
}
