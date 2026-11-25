import { ChevronDown, ChevronRight } from "lucide-react";
import { useCallback, useId, useMemo, useState, type CSSProperties } from "react";
import { useNavigate } from "react-router";
import { Button } from "../../kit/free/button-clean";
import { AgentMonitor, type AgentMonitorLabels } from "../../kit/pro/agent-monitor";
import { useNow } from "../../lib/useNow";
import type { Agent } from "../../types";
import AgentPersona, { type PersonaState } from "../AgentPersona";
import {
  Cap, CardNote, CardSkeleton, Chip, HeaderLink, InspSection, KeyValue, KitCard, RowButton, RowLink, SectionCard, StatusDot, cn,
} from "../common/KitPrimitives";
import { formatClock } from "../dashboard/routineModel";
import {
  AGENT_STATES, QUEUE_PATH, agentLine, agentName, agentStateLabel, buildSystemRows, describeHealthReasons, monitorView,
  systemNeedsAttention, todoHeaderLink, todoStatus, unlinkedMonitorView,
  type HomeAgent, type HomeAgentState, type HomeHealth, type HomeMementoStatus, type HomeTone, type MonitorView,
  type SourceState, type TodoItem, type Tr, type TriageAlert,
} from "./homeModel";
import { loadHealthDetailOnDemand, loadHomeHealth, loadHomeMemento, useAgentTurns, usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

const TODO_ROWS = 5;
const FULL_TOUCH_BUTTON_STYLE = {
  "--aui-button-clean-h": "44px",
  "--aui-button-clean-muted": "var(--fg-dim)",
  width: "100%",
} as CSSProperties;

const BUTTON_STYLE = {
  "--aui-button-clean-muted": "var(--fg-dim)",
  "--aui-button-clean-surface": "var(--bg-3)",
  "--aui-button-clean-shadow": "inset 0 0 0 1px var(--line)",
  "--aui-button-clean-shadow-hover": "inset 0 0 0 1px var(--fg-muted)",
} as CSSProperties;

function TodoAction({ item, tr, onOpen }: { item: TodoItem; tr: Tr; onOpen: () => void }) {
  const reasonId = useId();
  if (item.kind === "other") return null;
  if (item.kind === "approve") {
    return (
      <span className="flex shrink-0 flex-col items-end gap-1 py-2 pr-4">
        <Button size="lg" disabled aria-describedby={reasonId}>{tr("승인", "Approve")}</Button>
        <Cap id={reasonId}>{approvalNote(tr)}</Cap>
      </span>
    );
  }
  return (
    <span className="shrink-0 pr-4">
      <Button size="lg" variant="secondary" onClick={onOpen} style={BUTTON_STYLE}>
        {item.kind === "decide" ? tr("판단하기", "Decide") : tr("배포 보기", "See deploy")}
      </Button>
    </span>
  );
}

const approvalNote = (tr: Tr) => tr("결정 포럼에서 승인해요", "Approve in the Decision forum");

function HealthReasonList({ reasons, tr, loading }: { reasons: string[]; tr: Tr; loading: boolean }) {
  if (loading) {
    return <Cap className="block px-3 pb-3 sm:px-4">{tr("상세 원인을 확인하고 있어요…", "Loading detailed reasons…")}</Cap>;
  }
  if (reasons.length === 0) {
    return (
      <Cap className="block px-3 pb-3 sm:px-4">
        {tr("상세 원인 코드가 비어 있어요. 설정 > 시스템에서 연결 상태를 확인해 주세요.", "No specific reason code was returned; check Settings > System.")}
      </Cap>
    );
  }
  return (
    <ul data-testid="home-health-reasons" className="m-0 flex max-h-52 list-none flex-col gap-1 overflow-y-auto border-t border-[var(--line-soft)] bg-[var(--bg-1)] px-3 py-2.5 text-xs leading-4 text-[var(--fg-dim)] sm:px-4">
      {reasons.map((reason) => (
        <li key={reason} className="flex items-start gap-2">
          <StatusDot tone="warn" className="mt-1" />
          <span>{reason}</span>
        </li>
      ))}
    </ul>
  );
}

/** Top 3-second triage + human to-do card. Collapses to a 1-line status banner when both alerts and to-dos are zero. */
export function HomeTodoCard({
  items,
  alerts = [],
  agents,
  cards,
  campaigns,
  details,
  isMobile,
  tr,
  onReconcileSession,
  busySessionKeys,
  className,
}: {
  items: TodoItem[];
  alerts?: TriageAlert[];
  agents: Agent[];
  cards: SourceState;
  campaigns: SourceState;
  details?: { failed: number; cut: number };
  isMobile: boolean;
  tr: Tr;
  onReconcileSession?: (sessionKey: string) => Promise<void> | void;
  busySessionKeys?: ReadonlySet<string>;
  className?: string;
}) {
  const navigate = useNavigate();
  const agentById = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);
  const [healthOpen, setHealthOpen] = useState(false);
  const [detailReasons, setDetailReasons] = useState<string[] | null>(null);
  const [detailLoading, setDetailLoading] = useState(false);

  const total = alerts.length + items.length;
  const hidden = Math.max(0, items.length - TODO_ROWS);
  const status = todoStatus(total, [cards, campaigns]);
  const headerLink = todoHeaderLink(items, alerts, tr);

  const toggleHealthReasons = useCallback(async (fallbackReasons: string[]) => {
    setHealthOpen((prev) => {
      const next = !prev;
      if (next) {
        setDetailLoading(true);
        void loadHealthDetailOnDemand()
          .then((detail) => {
            const parsed = describeHealthReasons(detail, tr);
            setDetailReasons(parsed.length > 0 ? parsed : fallbackReasons);
          })
          .catch(() => setDetailReasons(fallbackReasons))
          .finally(() => setDetailLoading(false));
      }
      return next;
    });
  }, [tr]);

  if (status === "empty") {
    return (
      <KitCard
        as="section"
        aria-label={tr("할 일", "To do")}
        data-testid="home-todo"
        data-collapsed="true"
        className={cn("w-full", className)}
      >
        <div className="flex min-h-11 flex-wrap items-center justify-between gap-x-3 gap-y-1 px-3 py-2 sm:px-4">
          <span className="flex min-w-0 items-center gap-2 text-sm leading-5 text-[var(--fg-dim)]">
            <StatusDot tone="ok" />
            <strong className="font-semibold text-[var(--fg)]">{tr("할 일 0", "To do 0")}</strong>
            <span className="truncate text-[var(--fg-muted)]">{tr("지금 할 일이나 긴급 경보가 없어요.", "Nothing needs you right now.")}</span>
          </span>
          <HeaderLink to={QUEUE_PATH}>{tr("대기열에서 보기", "Open queue")}</HeaderLink>
        </div>
      </KitCard>
    );
  }

  return (
    <SectionCard
      title={tr("할 일", "To do")}
      count={status === "loading" || (status === "partial" && total === 0) ? null : total}
      countTone={alerts.some((a) => a.tone === "err") ? "err" : total > 0 ? "warn" : undefined}
      aside={<HeaderLink to={headerLink.to}>{headerLink.label}</HeaderLink>}
      testId="home-todo"
      className={className}
    >
      {status === "loading" ? (
        <CardSkeleton label={tr("할 일 불러오는 중", "Loading to-dos")} />
      ) : (
        <>
          {(alerts.length > 0 || items.length > 0) ? (
            <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
              {alerts.map((alert) => {
                const reconciling = Boolean(alert.sessionKey && busySessionKeys?.has(alert.sessionKey));
                return (
                  <li key={alert.key} data-testid={`home-triage-${alert.kind}`} className="border-t border-[var(--line-soft)] first:border-t-0">
                    <div className="flex min-w-0 items-center">
                      <RowLink to={alert.to} className="min-h-14 flex-1">
                        <Chip tone={alert.tone} className="justify-center sm:min-w-[4.5rem]">{alert.chip}</Chip>
                        <span className="min-w-0 flex-1">
                          <span className="block truncate text-sm leading-5 font-medium" title={alert.title}>{alert.title}</span>
                          <Cap className="mt-0.5 block truncate" title={alert.detail}>{alert.detail}</Cap>
                        </span>
                      </RowLink>
                      {alert.kind === "stale-session" && alert.sessionKey && onReconcileSession ? (
                        <span className="shrink-0 pr-3 sm:pr-4">
                          <Button
                            size="lg"
                            variant="secondary"
                            loading={reconciling}
                            onClick={() => void onReconcileSession(alert.sessionKey!)}
                            style={BUTTON_STYLE}
                          >
                            {tr("정리", "Clean up")}
                          </Button>
                        </span>
                      ) : alert.kind === "health" ? (
                        <span className="shrink-0 pr-3 sm:pr-4">
                          <Button
                            size="lg"
                            variant="secondary"
                            aria-expanded={healthOpen}
                            iconRight={<ChevronDown size={14} className={cn("transition-transform", healthOpen && "rotate-180")} />}
                            onClick={() => void toggleHealthReasons(alert.reasons ?? [])}
                            style={BUTTON_STYLE}
                          >
                            {healthOpen ? tr("접기", "Hide") : tr("원인 보기", "Why")}
                          </Button>
                        </span>
                      ) : (
                        <span className="hidden shrink-0 pr-4 sm:inline-flex">
                          <Button size="lg" variant="secondary" onClick={() => navigate(alert.to)} style={BUTTON_STYLE}>
                            {alert.kind === "quota" ? tr("한도 보기", "View limits") : tr("에이전트 보기", "View agent")}
                          </Button>
                        </span>
                      )}
                    </div>
                    {alert.kind === "health" && healthOpen ? (
                      <HealthReasonList reasons={detailReasons ?? alert.reasons ?? []} tr={tr} loading={detailLoading} />
                    ) : null}
                  </li>
                );
              })}
              {items.slice(0, TODO_ROWS).map((item) => {
                const assignee = item.agentId ? agentById.get(item.agentId) : undefined;
                const action = !isMobile && item.kind !== "other";
                return (
                  <li key={item.key} className="flex min-w-0 items-center border-t border-[var(--line-soft)] first:border-t-0">
                    <RowLink to={item.to} className="min-h-16 flex-1">
                      <Chip tone={item.tone} className="justify-center sm:min-w-[4.5rem]">{item.chip}</Chip>
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-sm leading-5 font-medium" title={item.title}>{item.title}</span>
                        <Cap className="mt-0.5 block truncate">{item.detail}</Cap>
                        {isMobile && item.kind === "approve" ? <Cap className="mt-0.5 block truncate">{approvalNote(tr)}</Cap> : null}
                      </span>
                      {assignee ? (
                        <span aria-hidden className="hidden sm:inline-flex">
                          <AgentPersona agent={assignee} size={24} />
                        </span>
                      ) : null}
                      {action ? null : <ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" />}
                    </RowLink>
                    {action ? <TodoAction item={item} tr={tr} onOpen={() => navigate(item.to)} /> : null}
                  </li>
                );
              })}
            </ul>
          ) : null}
          {hidden > 0 ? (
            <div className="border-t border-[var(--line-soft)] px-3 py-3 sm:px-4">
              <HeaderLink to={QUEUE_PATH}>{tr(`${hidden}건 더 보기`, `${hidden} more`)}</HeaderLink>
            </div>
          ) : null}
          {cards === "failed" ? <CardNote>{tr("작업 카드를 읽지 못해 할 일 일부는 확인 불가예요.", "Task cards did not load, so some to-dos are unknown.")}</CardNote> : null}
          {campaigns === "failed" ? <CardNote>{tr("캠페인 요약을 읽지 못해 할 일 일부는 확인 불가예요.", "Campaign summaries did not load, so some to-dos are unknown.")}</CardNote> : null}
          {details?.failed ? <CardNote>{tr("캠페인 상세 일부를 읽지 못해 할 일 일부는 확인 불가예요.", "Some campaign details did not load, so some to-dos are unknown.")}</CardNote> : null}
          {details?.cut ? <CardNote>{tr(`나머지 캠페인 ${details.cut}개의 할 일은 캠페인 화면에서 확인해요.`, `To-dos of ${details.cut} more campaigns are on the Campaigns screen.`)}</CardNote> : null}
        </>
      )}
    </SectionCard>
  );
}

/** Server, Discord bots, Memento, campaign runs and the queue, with expandable `/api/health/detail` cause inspection. */
export function HomeSystemCard({ campaigns, healthState, mementoState, agents, onlyWhenOff, tr, className }: {
  campaigns: Loaded<HomeCampaigns>;
  healthState?: Loaded<HomeHealth>;
  mementoState?: Loaded<HomeMementoStatus | null>;
  agents?: Agent[];
  onlyWhenOff?: boolean;
  tr: Tr;
  className?: string;
}) {
  const internalHealth = usePolled(loadHomeHealth, 30_000, healthState === undefined);
  const loadMemento = useCallback(() => loadHomeMemento(agents), [agents]);
  const internalMemento = usePolled(loadMemento, 60_000, mementoState === undefined);
  const health = healthState ?? internalHealth;
  const memento = mementoState ?? internalMemento;
  const [reasonsOpen, setReasonsOpen] = useState(false);
  const [freshDetail, setFreshDetail] = useState<HomeHealth | null>(null);
  const [loadingDetail, setLoadingDetail] = useState(false);

  const current = health.failed ? null : health.data;
  const rows = buildSystemRows({
    health: current,
    healthFailed: health.failed,
    legacy: campaigns.failed ? null : campaigns.data?.legacy ?? null,
    documents: campaigns.failed ? [] : campaigns.data?.documents ?? [],
    memento: memento.failed ? null : memento.data,
    tr,
  });
  const settled = (health.data !== null || health.failed) && (campaigns.data !== null || campaigns.failed);
  if (onlyWhenOff && !(settled && systemNeedsAttention(rows, tr))) return null;

  const publicOnly = current?.publicOnly;
  const clock = health.at === null ? null : formatClock(health.at);
  const serverRow = rows.find((row) => row.key === "server");
  const serverDegraded = serverRow?.tone === "warn" || serverRow?.tone === "err";
  const reasons = describeHealthReasons(freshDetail ?? current, tr);

  const handleToggleReasons = () => {
    setReasonsOpen((prev) => {
      const next = !prev;
      if (next) {
        setLoadingDetail(true);
        void loadHealthDetailOnDemand()
          .then((detail) => setFreshDetail(detail))
          .catch(() => {})
          .finally(() => setLoadingDetail(false));
      }
      return next;
    });
  };

  return (
    <SectionCard
      title={tr("시스템", "System")}
      aside={(
        <span className="flex items-center gap-2">
          {serverDegraded ? (
            <Button
              size="sm"
              variant="ghost"
              aria-expanded={reasonsOpen}
              data-testid="home-system-reasons-toggle"
              onClick={handleToggleReasons}
            >
              {reasonsOpen ? tr("원인 접기", "Hide why") : tr("원인 보기", "Why")}
            </Button>
          ) : null}
          {clock ? (
            <Cap className="font-display tabular-nums">
              {health.failed ? tr(`마지막 확인 ${clock}`, `Last checked ${clock}`) : tr(`${clock} 확인`, `Checked ${clock}`)}
            </Cap>
          ) : null}
        </span>
      )}
      testId="home-system"
      className={className}
    >
      <InspSection as="dl" className="gap-2.5">
        {rows.map((row) => (
          <KeyValue
            key={row.key}
            dl
            data-testid={`home-system-${row.key}`}
            leading={<StatusDot tone={row.tone} />}
            label={row.label}
            tone={row.tone}
            num={row.key === "queue"}
            valueTitle={row.value}
          >
            {row.value}
          </KeyValue>
        ))}
      </InspSection>
      {serverDegraded && reasonsOpen ? (
        <HealthReasonList reasons={reasons} tr={tr} loading={loadingDetail} />
      ) : null}
      {publicOnly ? (
        <CardNote>
          {publicOnly === "forbidden"
            ? tr("자세한 값은 서버가 도는 맥에서 열 때만 보여요.", "Details show only on the Mac running the server.")
            : tr("자세한 상태를 읽지 못해 요약만 보여요.", "Detailed status did not load; showing the summary.")}
        </CardNote>
      ) : null}
    </SectionCard>
  );
}

const PERSONA: Record<HomeAgentState, PersonaState> = { working: "working", background: "waiting", resting: "idle", noSession: "idle", unknown: "offline" };
const LEGEND_TONE: Record<HomeAgentState, HomeTone> = { working: "ok", background: "warn", resting: "neutral", noSession: "neutral", unknown: "neutral" };
const DESKTOP_SHOWN = 7;

function AgentCell({ entry, tr, isKo, onOpen }: { entry: HomeAgent; tr: Tr; isKo: boolean; onOpen: (entry: HomeAgent) => void }) {
  const line = agentLine(entry, tr, isKo);
  return (
    <RowButton className="min-h-14 gap-2.5 py-2.5 sm:py-3" onClick={() => onOpen(entry)}>
      <span aria-hidden className="flex"><AgentPersona agent={entry.agent} state={PERSONA[entry.state]} size={32} /></span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm leading-5 font-medium">{agentName(entry.agent, isKo)}</span>
        <Cap className="mt-0.5 block truncate" title={line}>{line}</Cap>
      </span>
    </RowButton>
  );
}

export const monitorLabels = (tr: Tr): Partial<AgentMonitorLabels> => ({
  agent: tr("에이전트", "Agent"),
  statusRunning: tr("실행 중", "Running"),
  cancel: tr("중단", "Stop"),
  log: tr("로그", "Log"),
  elapsed: tr("경과 시간", "Elapsed"),
  expand: (label) => tr(`${label}, 펼치기`, `${label}, expand`),
  fold: (name) => tr(`${name} 모니터 접기`, `Collapse ${name} monitor`),
  timeline: (steps) => tr(`실행한 단계: ${steps}`, `Steps run: ${steps}`),
  recent: tr("완료한 단계", "Finished steps"),
});

function AgentMonitors({
  items,
  isMobile,
  tr,
  isKo,
  onOpen,
  onStopTurn,
}: {
  items: Array<{ entry: HomeAgent; view: MonitorView }>;
  isMobile: boolean;
  tr: Tr;
  isKo: boolean;
  onOpen: (entry: HomeAgent) => void;
  onStopTurn?: (agent: Agent) => void;
}) {
  const now = useNow(1_000);
  const labels = useMemo(() => monitorLabels(tr), [tr]);
  return (
    <ul className={cn("m-0 grid list-none gap-2 border-t border-[var(--line-soft)] p-3 sm:gap-3 sm:px-4", items.length === 1 ? "grid-cols-1" : "sm:grid-cols-2")}>
      {items.map(({ entry, view }) => {
        const canCancel = Boolean(onStopTurn && !entry.agent.id.startsWith("dispatched:"));
        return (
          <li key={entry.agent.id} className="min-w-0" data-testid={`home-agent-monitor-${entry.agent.id}`}>
            <AgentMonitor
              status="running"
              defaultFolded={isMobile}
              agent={{ name: agentName(entry.agent, isKo), role: view.role ?? undefined, mark: <AgentPersona agent={entry.agent} state="working" size={28} /> }}
              current={{ label: view.label, kind: view.kind }}
              steps={view.steps}
              started={view.started ?? undefined}
              now={now}
              onCancel={canCancel ? () => onStopTurn!(entry.agent) : undefined}
              onOpenLog={() => onOpen(entry)}
              labels={labels}
              style={{ width: "100%" }}
            />
          </li>
        );
      })}
    </ul>
  );
}

/** Agents + unlinked lanes, ordered working -> background/stalled -> recent activity, with mobile folded monitors and turn stop. */
export function HomeAgentsCard({
  entries: everyone,
  isMobile,
  tr,
  isKo,
  onOpenAgent,
  onStopAgentTurn,
}: {
  entries: HomeAgent[];
  isMobile: boolean;
  tr: Tr;
  isKo: boolean;
  onOpenAgent: (agent: Agent) => void;
  onStopAgentTurn?: (agent: Agent) => void;
}) {
  const navigate = useNavigate();
  const [expanded, setExpanded] = useState(false);
  const workingAgentIds = useMemo(
    () => everyone.filter((entry) => entry.state === "working" && !entry.agent.id.startsWith("dispatched:")).map((entry) => entry.agent.id),
    [everyone],
  );
  const turns = useAgentTurns(workingAgentIds);
  const monitors = useMemo(
    () => everyone.flatMap((entry) => {
      const view = entry.unlinkedSession
        ? unlinkedMonitorView(entry, tr)
        : monitorView(turns.get(entry.agent.id), entry, tr);
      return view ? [{ entry, view }] : [];
    }),
    [everyone, tr, turns],
  );
  const monitored = new Set(monitors.map(({ entry }) => entry.agent.id));
  const entries = everyone.filter((entry) => !monitored.has(entry.agent.id));
  const counts = Object.fromEntries(AGENT_STATES.map((state) => [state, everyone.filter((entry) => entry.state === state).length])) as Record<HomeAgentState, number>;
  const busy = entries.filter((entry) => entry.state === "working" || entry.state === "background" || entry.stalled).length;
  const legendStates = AGENT_STATES.filter((state) => counts[state] > 0 || (!isMobile && state !== "unknown"));
  const legend = isMobile ? (
    <Cap className="font-display tabular-nums">
      {legendStates.map((state) => `${agentStateLabel(state, tr)} ${counts[state]}`).join(" · ")}
    </Cap>
  ) : (
    <Cap className="flex flex-wrap items-center gap-x-3.5 gap-y-1">
      {legendStates.map((state) => (
        <span key={state} className="flex items-center gap-1.5 font-display tabular-nums">
          <StatusDot tone={LEGEND_TONE[state]} />
          {agentStateLabel(state, tr)} {counts[state]}
        </span>
      ))}
    </Cap>
  );

  const handleOpenEntry = useCallback((entry: HomeAgent) => {
    if (entry.unlinkedSession || entry.agent.id.startsWith("dispatched:")) {
      navigate("/agents");
      return;
    }
    onOpenAgent(entry.agent);
  }, [navigate, onOpenAgent]);

  const preferred = entries.slice(0, isMobile ? busy : Math.max(DESKTOP_SHOWN, busy));
  const folds = isMobile ? preferred.length < entries.length : entries.length > DESKTOP_SHOWN + 1 && preferred.length < entries.length;
  const shown = expanded || !folds ? entries : preferred;
  const rest = entries.length - preferred.length;
  const toggleLabel = expanded
    ? tr("접기", "Show less")
    : isMobile ? tr(`${rest}명 더 보기`, `Show ${rest} more`) : tr(`나머지 ${rest}명`, `${rest} more`);

  return (
    <SectionCard title={tr("에이전트", "Agents")} count={isMobile ? null : everyone.length} aside={legend} testId="home-agents">
      {monitors.length > 0 ? (
        <AgentMonitors
          items={monitors}
          isMobile={isMobile}
          tr={tr}
          isKo={isKo}
          onOpen={handleOpenEntry}
          onStopTurn={onStopAgentTurn}
        />
      ) : null}
      {everyone.length === 0 ? (
        <CardNote>{tr("등록된 에이전트가 없어요.", "No agents yet.")}</CardNote>
      ) : entries.length === 0 ? null : isMobile ? (
        <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
          {shown.length === 0 && monitors.length === 0 ? (
            <li className="px-3 py-4 text-sm text-[var(--fg-muted)]">
              {counts.unknown > 0 ? tr("세션을 읽지 못해 상태는 확인 불가예요.", "Sessions did not load, so states are unknown.") : tr("모두 쉬는 중이에요.", "Everyone is resting.")}
            </li>
          ) : null}
          {shown.map((entry) => (
            <li key={entry.agent.id} className="border-b border-[var(--line-soft)] last:border-b-0">
              <AgentCell entry={entry} tr={tr} isKo={isKo} onOpen={handleOpenEntry} />
            </li>
          ))}
          {folds ? (
            <li className="flex justify-center border-t border-[var(--line-soft)] px-3 py-1.5">
              <Button
                variant="ghost"
                aria-expanded={expanded}
                onClick={() => setExpanded((value) => !value)}
                style={FULL_TOUCH_BUTTON_STYLE}
              >
                {toggleLabel}
              </Button>
            </li>
          ) : null}
        </ul>
      ) : (
        <ul className="m-0 -mr-px -mb-px grid list-none grid-cols-4 border-t border-[var(--line-soft)] p-0">
          {shown.map((entry) => (
            <li key={entry.agent.id} className="min-w-0 border-r border-b border-[var(--line-soft)]">
              <AgentCell entry={entry} tr={tr} isKo={isKo} onOpen={handleOpenEntry} />
            </li>
          ))}
          {folds ? (
            <li className="min-w-0 border-r border-b border-[var(--line-soft)]">
              <RowButton
                aria-expanded={expanded}
                onClick={() => setExpanded((value) => !value)}
                className="min-h-14 gap-2.5 text-sm text-[var(--fg-muted)]"
              >
                {expanded ? null : (
                  <span aria-hidden className="flex">
                    {entries.slice(preferred.length, preferred.length + 3).map((entry, index) => (
                      <span key={entry.agent.id} className={index ? "-ml-1.5" : undefined}>
                        <AgentPersona agent={entry.agent} state={PERSONA[entry.state]} size={24} />
                      </span>
                    ))}
                  </span>
                )}
                <span className="break-keep">{toggleLabel}</span>
              </RowButton>
            </li>
          ) : null}
        </ul>
      )}
    </SectionCard>
  );
}
