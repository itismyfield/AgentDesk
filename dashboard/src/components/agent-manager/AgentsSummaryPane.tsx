import { X } from "lucide-react";
import { useMemo, useState } from "react";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { CompactBreakdown } from "../../kit/pro/compact-breakdown";
import type { Agent, Department, DispatchedSession } from "../../types";
import { Cap, StatusDot, type DotTone } from "../common/KitPrimitives";
import { AGENT_STATES, agentStateLabel, type HomeAgent, type HomeAgentState } from "../home/homeModel";
import { SessionPanel } from "../session-panel/SessionPanel";
import { AgentOverlay } from "./AgentOverlay";
import { NO_DEPARTMENT, hasDepartment } from "./agent-list-controls";
import type { Translator } from "./types";

// Home's legend tones: a session opens when a message arrives, so agents without one are gray, not an error.
const LEGEND_TONE: Record<HomeAgentState, DotTone> = { working: "ok", background: "warn", resting: "neutral", noSession: "neutral", unknown: "neutral" };

/** Right pane while no agent is picked: counts per state, agents per department (a row filters the list), and the
 * sessions not linked to an agent, which open in a sheet to attach. */
export function AgentsSummaryPane({ counted, entries, agents, departments, locale, tr, deptTab, onSelectDept, unlinkedSessions, onAssign }: {
  /** Home's counted set for the state counts; `entries` are the real agents. */
  counted: HomeAgent[];
  entries: HomeAgent[];
  agents: Agent[];
  departments: Department[];
  locale: string;
  tr: Translator;
  deptTab: string;
  onSelectDept: (deptId: string) => void;
  unlinkedSessions: DispatchedSession[];
  onAssign?: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
}) {
  const [sessionsOpen, setSessionsOpen] = useState(false);
  const counts = AGENT_STATES.map((state) => [state, counted.filter((entry) => entry.state === state).length] as const)
    .filter(([state, count]) => count > 0 || state !== "unknown");
  const byDept = useMemo(() => {
    const rows = departments.map((department) => ({
      id: department.id,
      label: localeName(locale, department),
      value: entries.filter((entry) => entry.agent.department_id === department.id).length,
    }));
    const known = new Set(departments.map((department) => department.id));
    rows.push({ id: NO_DEPARTMENT, label: tr("미배정", "Unassigned"), value: entries.filter((entry) => !hasDepartment(entry.agent, known)).length });
    // One measure, so one series color, clear of the state colors.
    return rows.filter((row) => row.value > 0).map((row) => ({ ...row, color: "var(--accent)" }));
  }, [departments, entries, locale, tr]);

  return (
    <div data-testid="agents-summary" className="flex min-w-0 flex-col gap-5">
      <h2 className="m-0 text-sm leading-5 font-semibold text-(--fg)">{tr("요약", "Summary")}</h2>
      <section aria-label={tr("상태", "States")} className="flex flex-col gap-2">
        <Cap>{tr("상태", "States")}</Cap>
        <p className="m-0 flex flex-wrap gap-x-3.5 gap-y-1 text-xs text-(--fg-dim)">
          {counts.map(([state, count]) => (
            <span key={state} className="flex items-center gap-1.5 font-display tabular-nums">
              <StatusDot tone={LEGEND_TONE[state]} />
              {agentStateLabel(state, tr)} {count}
            </span>
          ))}
        </p>
      </section>
      {byDept.length > 0 ? (
        <CompactBreakdown
          items={byDept}
          title={tr("부서별 인원", "Agents by department")}
          totalLabel={tr("전체 에이전트", "All agents")}
          format={(value) => value.toLocaleString()}
          onSelect={onSelectDept}
          selectedId={deptTab === "all" ? null : deptTab}
          hideFooter
          labels={{
            list: tr("부서별 인원과 비중", "Agents and share by department"),
            row: (label, value, share) => tr(`${label}, ${value}명, 전체의 ${share}`, `${label}, ${value}, ${share} of all`),
            summary: (total, amount) => `${total}: ${amount}`,
            empty: tr("에이전트가 없어요.", "No agents yet."),
            selectHint: tr("누르면 이 부서만 목록에 보여요.", "Shows only this department in the list."),
          }}
        />
      ) : null}
      {unlinkedSessions.length > 0 && onAssign ? (
        <section aria-label={tr("연결되지 않은 세션", "Unlinked sessions")} className="flex min-w-0 items-center gap-3 border-t border-(--line-soft) pt-4">
          <span className="min-w-0 flex-1">
            <span className="block text-sm leading-5 font-medium text-(--fg)">
              {tr(`에이전트에 연결되지 않은 세션 ${unlinkedSessions.length}`, `Sessions without an agent: ${unlinkedSessions.length}`)}
            </span>
            <Cap className="mt-0.5 block">{tr("부서나 에이전트에 붙일 수 있어요", "Attach them to a department or agent")}</Cap>
          </span>
          <Button variant="secondary" size="lg" onClick={() => setSessionsOpen(true)}>{tr("붙이기", "Attach")}</Button>
        </section>
      ) : null}
      {sessionsOpen && onAssign ? (
        <AgentOverlay
          variant="sheet"
          ariaLabel={tr("연결되지 않은 세션", "Unlinked sessions")}
          onClose={() => setSessionsOpen(false)}
          header={(
            <div className="flex items-center justify-between gap-3">
              <span className="text-sm font-semibold">{tr("연결되지 않은 세션", "Unlinked sessions")}</span>
              <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={() => setSessionsOpen(false)} />
            </div>
          )}
        >
          <SessionPanel sessions={unlinkedSessions} departments={departments} agents={agents} onAssign={onAssign} />
        </AgentOverlay>
      ) : null}
    </div>
  );
}
