import { useId, useMemo, useState } from "react";
import { ChevronDown, RotateCcw, X } from "lucide-react";
import { getProviderMeta } from "../../app/providerTheme";
import { formatRelativeTime } from "../../app/shellFormatting";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { CompactBreakdown } from "../../kit/pro/compact-breakdown";
import type { Department, DispatchedSession } from "../../types";
import AgentPersona from "../AgentPersona";
import { Cap, Chip, RowButton, StatusDot, cn, type DotTone } from "../common/KitPrimitives";
import { AGENT_STATES, agentStateLabel, type HomeAgent, type HomeAgentState } from "../home/homeModel";
import { SessionInfoCard } from "../session-panel/SessionInfoCard";
import {
  canCleanupStaleIdleSession,
  canReconcileStaleSession,
  cleanupStaleIdleSession,
  compareSessions,
  isStaleIdleSession,
  reconcileStaleSessionTurn,
  sessionLastActivityTs,
  sessionPersona,
  sessionState,
  sessionStateText,
  sessionSummaryCounts,
  sessionTitle,
} from "../session-panel/SessionPanelModel";
import { AgentOverlay } from "./AgentOverlay";
import { NO_DEPARTMENT, departmentLabels, hasDepartment } from "./agent-list-controls";
import type { Translator } from "./types";

/** Collapses idle or closed subagent tool sessions (`toolu_*`) while keeping active turns visible. */
export function isIdleSubagentToolSession(session: DispatchedSession): boolean {
  const { title } = sessionTitle(session);
  const isToolSession =
    /toolu_/i.test(session.session_key) ||
    /toolu_/i.test(session.name ?? "") ||
    /toolu_/i.test(title);
  if (!isToolSession) return false;
  const state = sessionState(session);
  return state === "resting" || state === "closed";
}

function AgentsUnlinkedSessionList({
  sessions,
  locale,
  tr,
  isKo,
  onSessionsChange,
}: {
  sessions: DispatchedSession[];
  locale: string;
  tr: Translator;
  isKo: boolean;
  onSessionsChange?: () => void;
}) {
  const baseId = useId();
  const [open, setOpen] = useState<string | null>(null);
  const [subtoolsOpen, setSubtoolsOpen] = useState(false);
  const [reconcilingKey, setReconcilingKey] = useState<string | null>(null);
  const [reconciledKeys, setReconciledKeys] = useState<Record<string, string>>({});
  const effective = useMemo(
    () =>
      sessions
        .filter((s) => reconciledKeys[s.session_key] !== "disconnected")
        .map((s) => (reconciledKeys[s.session_key] === "ok" ? { ...s, status: "idle" as const, last_seen_at: Date.now() } : s)),
    [sessions, reconciledKeys],
  );
  const sorted = useMemo(() => [...effective].sort(compareSessions), [effective]);
  const primarySessions = useMemo(() => sorted.filter((s) => !isIdleSubagentToolSession(s)), [sorted]);
  const idleSubtoolSessions = useMemo(() => sorted.filter((s) => isIdleSubagentToolSession(s)), [sorted]);

  const handleReconcile = async (session: DispatchedSession) => {
    setReconcilingKey(session.session_key);
    try {
      const res = await reconcileStaleSessionTurn(session.session_key);
      const verdict = res.reconciled ? "ok" : (res.reason || tr("살아 있는 턴이라 그대로 두었어요.", "Turn is still live; kept as-is."));
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: verdict }));
      if (res.reconciled) onSessionsChange?.();
    } catch {
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: tr("정리 요청에 실패했어요. 다시 시도해 보세요.", "Reconcile failed. Try again.") }));
    } finally {
      setReconcilingKey(null);
    }
  };

  const handleCleanupIdle = async (session: DispatchedSession) => {
    setReconcilingKey(session.session_key);
    try {
      const res = await cleanupStaleIdleSession(session.session_key);
      const verdict = res.disconnected ? "disconnected" : (res.reason || tr("아직 활성 상태라 세션을 유지했어요.", "Session is still active; kept as-is."));
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: verdict }));
      if (res.disconnected) onSessionsChange?.();
    } catch {
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: tr("정리 요청에 실패했어요. 다시 시도해 보세요.", "Cleanup failed. Try again.") }));
    } finally {
      setReconcilingKey(null);
    }
  };

  const renderRows = (items: DispatchedSession[], testId?: string) => (
    <ul data-testid={testId} className="m-0 min-w-0 list-none overflow-hidden rounded-(--r-md) border border-(--line-soft) bg-(--bg-2) p-0">
      {items.map((session) => {
        const { title, child } = sessionTitle(session);
        const last = sessionLastActivityTs(session);
        const info = session.session_info?.split("\n")[0]?.trim();
        const line = [
          sessionStateText(session, tr),
          child ? tr("백그라운드 작업", "Background job") : null,
          info || (last === null ? null : tr(`마지막 신호 ${formatRelativeTime(last, isKo)}`, `Last signal ${formatRelativeTime(last, isKo)}`)),
        ].filter(Boolean).join(" · ");
        const expanded = open === session.id;
        const detailId = `${baseId}-${session.id}`;
        const stale = isStaleIdleSession(session);
        const note = reconciledKeys[session.session_key];
        const reconcilable = canReconcileStaleSession(session) && note !== "ok";
        const cleanableIdle = canCleanupStaleIdleSession(session) && note !== "disconnected";
        return (
          <li key={session.id} className="border-t border-(--line-soft) first:border-t-0">
            <div className="flex min-w-0 items-center gap-1.5 pr-2">
              <RowButton data-session-row={session.id} aria-expanded={expanded} aria-controls={expanded ? detailId : undefined} onClick={() => setOpen(expanded ? null : session.id)} className="min-h-14 flex-1 py-2.5 pr-2 text-[13px]">
                <span aria-hidden className="flex"><AgentPersona {...sessionPersona(session)} size={32} /></span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm leading-5 font-medium" title={session.session_key}>{title}</span>
                  <span className="block truncate text-xs leading-4 text-(--fg-muted)" title={line}>{line}</span>
                </span>
                {note === "ok" ? <Chip tone="ok">{tr("정리됨", "Reconciled")}</Chip> : stale ? <Chip tone="warn">{tr("7일 넘게 멈춤", "Idle 7+ days")}</Chip> : null}
                <span className="hidden w-20 shrink-0 truncate text-right text-xs text-(--fg-dim) sm:block">{getProviderMeta(session.provider).label}</span>
                <ChevronDown size={16} aria-hidden className={cn("shrink-0 text-(--fg-muted) transition-transform motion-reduce:transition-none", expanded && "rotate-180")} />
              </RowButton>
              {reconcilable ? (
                <Button variant="secondary" size="sm" icon={<RotateCcw size={13} aria-hidden />} loading={reconcilingKey === session.session_key} onClick={() => void handleReconcile(session)} data-testid={`reconcile-session-${session.id}`}>
                  {tr("멈춘 턴 정리", "Reconcile stale turn")}
                </Button>
              ) : cleanableIdle ? (
                <Button variant="secondary" size="sm" loading={reconcilingKey === session.session_key} onClick={() => void handleCleanupIdle(session)} data-testid={`cleanup-session-${session.id}`}>
                  {tr("정리", "Clean up")}
                </Button>
              ) : null}
            </div>
            {note && note !== "ok" ? <Cap className="block bg-(--bg-1) px-3 py-1.5 text-(--warn) sm:px-4" role="status">{note}</Cap> : null}
            {expanded ? <SessionInfoCard id={detailId} session={session} departments={[]} locale={locale} tr={tr} isKo={isKo} /> : null}
          </li>
        );
      })}
    </ul>
  );

  return (
    <div data-testid="agents-sessions" className="flex min-w-0 flex-col gap-3">
      <Cap className="block">
        {tr(
          "에이전트에 연결되지 않은 채 열려 있는 세션이에요. 세션을 에이전트에 붙이는 저장은 아직 지원하지 않으며, 행을 열면 실시간 출력을 확인하고 멈춘 턴을 바로 정리할 수 있어요.",
          "Open sessions no agent owns. Saving an agent assignment is not supported yet; expand a row to preview live output or reconcile stale turns.",
        )}
      </Cap>
      {primarySessions.length > 0 ? renderRows(primarySessions) : null}
      {idleSubtoolSessions.length > 0 ? (
        <div className="flex min-w-0 flex-col gap-2">
          <Button
            variant="ghost"
            size="sm"
            className="self-start"
            aria-expanded={subtoolsOpen}
            data-testid="toggle-subtool-sessions"
            icon={<ChevronDown size={14} aria-hidden className={cn("transition-transform motion-reduce:transition-none", subtoolsOpen && "rotate-180")} />}
            onClick={() => setSubtoolsOpen((prev) => !prev)}
          >
            {subtoolsOpen
              ? tr(`하위 도구 세션 ${idleSubtoolSessions.length}개 접기`, `Hide ${idleSubtoolSessions.length} sub-tool sessions`)
              : tr(`하위 도구 세션 ${idleSubtoolSessions.length}개 보기`, `Show ${idleSubtoolSessions.length} sub-tool sessions`)}
          </Button>
          {subtoolsOpen ? renderRows(idleSubtoolSessions, "agents-subtool-sessions") : null}
        </div>
      ) : null}
    </div>
  );
}

/** Unlinked sessions summary row + modal sheet for Agents view. */
export function AgentsUnlinkedSessions({
  sessions,
  locale,
  tr,
  isKo,
  onSessionsChange,
  className,
}: {
  sessions: DispatchedSession[];
  locale: string;
  tr: Translator;
  isKo: boolean;
  onSessionsChange?: () => void;
  className?: string;
}) {
  const [sheetOpen, setSheetOpen] = useState(false);
  if (sessions.length === 0) return null;
  const label = tr("연결되지 않은 세션", "Unlinked sessions");
  return (
    <section aria-label={label} className={cn("flex min-w-0 items-center gap-3", className)}>
      <span className="min-w-0 flex-1">
        <span className="block text-sm leading-5 font-medium text-(--fg)">{tr(`에이전트에 연결되지 않은 세션 ${sessions.length}`, `Sessions without an agent: ${sessions.length}`)}</span>
        <Cap className="mt-0.5 block" data-testid="unlinked-sessions-summary">{sessionSummaryCounts(sessions, tr)}</Cap>
      </span>
      <Button variant="secondary" size="lg" onClick={() => setSheetOpen(true)}>{tr("보기", "View")}</Button>
      {sheetOpen ? (
        <AgentOverlay
          variant="sheet"
          ariaLabel={label}
          widthClassName="sm:max-w-2xl"
          onClose={() => setSheetOpen(false)}
          header={(
            <div className="flex items-center justify-between gap-3">
              <span className="flex items-baseline gap-1.5 text-sm font-semibold">
                {label}
                <span className="font-display text-xs font-normal text-(--fg-muted) tabular-nums">{sessions.length}</span>
              </span>
              <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={() => setSheetOpen(false)} />
            </div>
          )}
        >
          <AgentsUnlinkedSessionList sessions={sessions} locale={locale} tr={tr} isKo={isKo} onSessionsChange={onSessionsChange} />
        </AgentOverlay>
      ) : null}
    </section>
  );
}

// Home's legend tones: a session opens when a message arrives, so agents without one are gray, not an error.
const LEGEND_TONE: Record<HomeAgentState, DotTone> = { working: "ok", background: "warn", resting: "neutral", noSession: "neutral", unknown: "neutral" };

/** Right pane while no agent is picked: state counts, unlinked sessions up top, and agent distribution by Provider. */
export function AgentsSummaryPane({ counted, entries, departments, locale, tr, isKo, deptTab, onSelectDept, unlinkedSessions, onSessionsChange }: {
  /** Home's counted set for the state counts; `entries` are the real agents. */
  counted: HomeAgent[];
  entries: HomeAgent[];
  departments: Department[];
  locale: string;
  tr: Translator;
  isKo: boolean;
  deptTab?: string;
  onSelectDept?: (deptId: string) => void;
  unlinkedSessions: DispatchedSession[];
  onSessionsChange?: () => void;
}) {
  const counts = AGENT_STATES.map((state) => [state, counted.filter((entry) => entry.state === state).length] as const)
    .filter(([state, count]) => count > 0 || state !== "unknown");
  const byProvider = useMemo(() => {
    const totals = new Map<string, number>();
    for (const entry of entries) {
      const id = entry.agent.cli_provider?.trim().toLowerCase() || "claude";
      totals.set(id, (totals.get(id) ?? 0) + 1);
    }
    return Array.from(totals.entries())
      .sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]))
      .map(([id, value]) => {
        const meta = getProviderMeta(id);
        return { id, label: meta.label, value, color: meta.color };
      });
  }, [entries]);
  const byDept = useMemo(() => {
    if (!onSelectDept) return [];
    const labels = departmentLabels(departments, locale);
    const rows = departments.map((department) => ({
      id: department.id,
      label: labels.get(department.id) ?? department.name,
      value: entries.filter((entry) => entry.agent.department_id === department.id).length,
    }));
    const known = new Set(departments.map((department) => department.id));
    rows.push({ id: NO_DEPARTMENT, label: tr("미배정", "Unassigned"), value: entries.filter((entry) => !hasDepartment(entry.agent, known)).length });
    return rows.filter((row) => row.value > 0).map((row) => ({ ...row, color: "var(--accent)" }));
  }, [departments, entries, locale, onSelectDept, tr]);

  return (
    <div data-testid="agents-summary" className="flex min-w-0 flex-col gap-4">
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
      <AgentsUnlinkedSessions sessions={unlinkedSessions} locale={locale} tr={tr} isKo={isKo} onSessionsChange={onSessionsChange} className="border-t border-(--line-soft) pt-3.5" />
      {onSelectDept ? (
        byDept.length ? (
          <CompactBreakdown
            items={byDept}
            title={tr("부서별 인원", "Agents by department")}
            totalLabel={tr("전체 에이전트", "All agents")}
            format={(value) => Math.round(value).toLocaleString()}
            onSelect={onSelectDept}
            selectedId={!deptTab || deptTab === "all" ? null : deptTab}
            hideFooter
            labels={{
              list: tr("부서별 인원과 비중", "Agents and share by department"),
              row: (label, value, share) => tr(`${label}, ${value}명, 전체의 ${share}`, `${label}, ${value}, ${share} of all`),
              summary: (total, amount) => `${total}: ${amount}`,
              empty: tr("에이전트가 없어요.", "No agents yet."),
              selectHint: tr("누르면 이 부서만 목록에 보여요.", "Shows only this department in the list."),
            }}
          />
        ) : null
      ) : byProvider.length ? (
        <CompactBreakdown
          items={byProvider}
          title={tr("Provider별 에이전트 분포", "Agents by provider")}
          totalLabel={tr("전체 에이전트", "All agents")}
          format={(value) => Math.round(value).toLocaleString()}
          hideFooter
          className="max-w-none!"
          labels={{
            list: tr("Provider별 에이전트 수와 비중", "Agents and share by provider"),
            row: (label, value, share) => tr(`${label}, ${value}명, 전체의 ${share}`, `${label}, ${value}, ${share} of all`),
            summary: (total, amount) => `${total}: ${amount}`,
            empty: tr("에이전트가 없어요.", "No agents yet."),
          }}
        />
      ) : null}
    </div>
  );
}
