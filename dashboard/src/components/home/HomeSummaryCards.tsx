import { ChevronRight } from "lucide-react";
import { useEffect, useId, useMemo, useState, type CSSProperties } from "react";
import { useNavigate } from "react-router";
import { Button } from "../../kit/free/button-clean";
import { AgentMonitor, type AgentMonitorLabels } from "../../kit/pro/agent-monitor";
import type { Agent } from "../../types";
import AgentPersona, { type PersonaState } from "../AgentPersona";
import {
  Cap, CardNote, CardSkeleton, Chip, HeaderLink, InspSection, KeyValue, RowButton, RowLink, SectionCard, StatusDot,
} from "../common/KitPrimitives";
import { formatClock } from "../dashboard/routineModel";
import {
  AGENT_STATES, QUEUE_PATH, agentLine, agentName, agentStateLabel, buildSystemRows, monitorView, systemNeedsAttention, todoStatus,
  type HomeAgent, type HomeAgentState, type HomeTone, type MonitorView, type SourceState, type TodoItem, type Tr,
} from "./homeModel";
import { loadHomeHealth, useAgentTurns, usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

// Home's first three sections: what needs a person, how the system is, and who is working.

const TODO_ROWS = 5;
const FULL_TOUCH_BUTTON_STYLE = {
  "--aui-button-clean-h": "44px",
  "--aui-button-clean-muted": "var(--fg-dim)",
  width: "100%",
} as CSSProperties;

const BUTTON_STYLE = { "--aui-button-clean-muted": "var(--fg-dim)" } as CSSProperties;

/** Desktop row button: approval stays disabled with its reason until the server takes dashboard approvals;
 * decisions and deploys open the screen where they are handled. */
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

// Dashboard approval needs the coordinator's forum evidence (docs/dashboard-campaign-decisions-api.md).
const approvalNote = (tr: Tr) => tr("결정 포럼에서 승인해요", "Approve in the Decision forum");

/** What only a person can move, at the top of home. Each row opens the screen where it is handled; desktop rows
 * add the approval, decision or deploy button. `details` counts the server campaigns whose to-dos went unread. */
export function HomeTodoCard({ items, agents, cards, campaigns, details, isMobile, tr, className }: {
  items: TodoItem[];
  agents: Agent[];
  cards: SourceState;
  campaigns: SourceState;
  details?: { failed: number; cut: number };
  isMobile: boolean;
  tr: Tr;
  className?: string;
}) {
  const navigate = useNavigate();
  const agentById = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);
  const total = items.length;
  const hidden = total - TODO_ROWS;
  const status = todoStatus(total, [cards, campaigns]);
  return (
    <SectionCard
      title={tr("할 일", "To do")}
      count={status === "loading" || (status === "partial" && total === 0) ? null : total}
      countTone={total > 0 ? "warn" : undefined}
      aside={<HeaderLink to={QUEUE_PATH}>{tr("대기열에서 보기", "Open queue")}</HeaderLink>}
      testId="home-todo"
      className={className}
    >
      {status === "loading" ? (
        <CardSkeleton label={tr("할 일 불러오는 중", "Loading to-dos")} />
      ) : (
        <>
          {status === "empty" ? <CardNote>{tr("지금 할 일이 없어요.", "Nothing needs you right now.")}</CardNote> : null}
          {items.length > 0 ? (
            <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
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

/** Server, Discord bots, Memento, campaign runs and the queue, one dot and value per line. A failed read shows
 * unknown values and when the last answer came, never the last answer itself. */
export function HomeSystemCard({ campaigns, onlyWhenOff, tr, className }: {
  campaigns: Loaded<HomeCampaigns>;
  onlyWhenOff?: boolean;
  tr: Tr;
  className?: string;
}) {
  const health = usePolled(loadHomeHealth, 30_000);
  const current = health.failed ? null : health.data;
  const rows = buildSystemRows({
    health: current,
    healthFailed: health.failed,
    legacy: campaigns.failed ? null : campaigns.data?.legacy ?? null,
    documents: campaigns.failed ? [] : campaigns.data?.documents ?? [],
    tr,
  });
  // Phones keep this card for trouble only, judged once both reads have answered.
  const settled = (health.data !== null || health.failed) && (campaigns.data !== null || campaigns.failed);
  if (onlyWhenOff && !(settled && systemNeedsAttention(rows, tr))) return null;
  const publicOnly = current?.publicOnly;
  const clock = health.at === null ? null : formatClock(health.at);
  return (
    <SectionCard
      title={tr("시스템", "System")}
      aside={clock ? (
        <Cap className="font-display tabular-nums">
          {health.failed ? tr(`마지막 확인 ${clock}`, `Last checked ${clock}`) : tr(`${clock} 확인`, `Checked ${clock}`)}
        </Cap>
      ) : null}
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

// The legend dots use the persona dot colors, so a count and its avatars read the same. A session opens when a
// Discord message arrives, so an agent without one is at rest: gray, never the error color.
const PERSONA: Record<HomeAgentState, PersonaState> = { working: "working", background: "waiting", resting: "idle", noSession: "idle", unknown: "offline" };
const LEGEND_TONE: Record<HomeAgentState, HomeTone> = { working: "ok", background: "warn", resting: "neutral", noSession: "neutral", unknown: "neutral" };
/** Desktop shows two rows of four: up to seven agents and the fold, unless more are busy. */
const DESKTOP_SHOWN = 7;

function AgentCell({ entry, tr, isKo, onOpen }: { entry: HomeAgent; tr: Tr; isKo: boolean; onOpen: (agent: Agent) => void }) {
  const line = agentLine(entry, tr, isKo);
  return (
    <RowButton className="min-h-14 gap-2.5 py-2.5 sm:py-3" onClick={() => onOpen(entry.agent)}>
      <span aria-hidden className="flex"><AgentPersona agent={entry.agent} state={PERSONA[entry.state]} size={32} /></span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm leading-5 font-medium">{agentName(entry.agent, isKo)}</span>
        <Cap className="mt-0.5 block truncate" title={line}>{line}</Cap>
      </span>
    </RowButton>
  );
}

/** Monitor texts home can show or speak: a running monitor without steps, with fold and log buttons. */
const monitorLabels = (tr: Tr): Partial<AgentMonitorLabels> => ({
  agent: tr("에이전트", "Agent"),
  statusRunning: tr("실행 중", "Running"),
  log: tr("로그", "Log"),
  elapsed: tr("경과 시간", "Elapsed"),
  expand: (label) => tr(`${label}, 펼치기`, `${label}, expand`),
  fold: (name) => tr(`${name} 모니터 접기`, `Collapse ${name} monitor`),
  timeline: (steps) => tr(`실행한 단계: ${steps}`, `Steps run: ${steps}`),
  recent: tr("완료한 단계", "Finished steps"),
});

/** Working agents with a live turn as kit monitors: what runs now and for how long. The host clock ticks once a
 * second instead of the monitor's own 100ms clock; the log button opens the agent detail with its activity log. */
function AgentMonitors({ items, tr, isKo, onOpen }: { items: Array<{ entry: HomeAgent; view: MonitorView }>; tr: Tr; isKo: boolean; onOpen: (agent: Agent) => void }) {
  const [now, setNow] = useState(() => Date.now());
  const labels = useMemo(() => monitorLabels(tr), [tr]);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, []);
  return (
    <ul className="m-0 grid list-none gap-2 border-t border-[var(--line-soft)] p-3 sm:grid-cols-2 sm:gap-3 sm:px-4">
      {items.map(({ entry, view }) => (
        <li key={entry.agent.id} className="min-w-0" data-testid={`home-agent-monitor-${entry.agent.id}`}>
          <AgentMonitor
            status="running"
            agent={{ name: agentName(entry.agent, isKo), role: view.role ?? undefined, mark: <AgentPersona agent={entry.agent} state="working" size={28} /> }}
            current={{ label: view.label, kind: view.kind }}
            started={view.started ?? undefined}
            now={now}
            onOpenLog={() => onOpen(entry.agent)}
            labels={labels}
            style={{ width: "100%" }}
          />
        </li>
      ))}
    </ul>
  );
}

/** Everyone with a state dot and one line of what they do. On desktop working agents with a live turn get a kit
 * monitor and the rest follow, busy first, folded into one cell; phones list only busy agents as one-line rows, since
 * the monitor wraps to about five rows' height there. */
export function HomeAgentsCard({ entries: everyone, isMobile, tr, isKo, onOpenAgent }: {
  entries: HomeAgent[];
  isMobile: boolean;
  tr: Tr;
  isKo: boolean;
  onOpenAgent: (agent: Agent) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  // Phones show no monitors, so they read no turns either.
  const turns = useAgentTurns(isMobile ? [] : everyone.filter((entry) => entry.state === "working").map((entry) => entry.agent.id));
  const monitors = isMobile ? [] : everyone.flatMap((entry) => {
    const view = monitorView(turns.get(entry.agent.id), entry, tr);
    return view ? [{ entry, view }] : [];
  });
  const monitored = new Set(monitors.map(({ entry }) => entry.agent.id));
  const entries = everyone.filter((entry) => !monitored.has(entry.agent.id));
  const counts = Object.fromEntries(AGENT_STATES.map((state) => [state, everyone.filter((entry) => entry.state === state).length])) as Record<HomeAgentState, number>;
  const busy = entries.filter((entry) => entry.state === "working" || entry.state === "background").length;
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

  const preferred = entries.slice(0, isMobile ? busy : Math.max(DESKTOP_SHOWN, busy));
  const folds = isMobile ? preferred.length < entries.length : entries.length > DESKTOP_SHOWN + 1 && preferred.length < entries.length;
  const shown = expanded || !folds ? entries : preferred;
  const rest = entries.length - preferred.length;
  const toggleLabel = expanded
    ? tr("접기", "Show less")
    : isMobile ? tr(`${rest}명 더 보기`, `Show ${rest} more`) : tr(`나머지 ${rest}명`, `${rest} more`);

  return (
    <SectionCard title={tr("에이전트", "Agents")} count={isMobile ? null : everyone.length} aside={legend} testId="home-agents">
      {monitors.length > 0 ? <AgentMonitors items={monitors} tr={tr} isKo={isKo} onOpen={onOpenAgent} /> : null}
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
              <AgentCell entry={entry} tr={tr} isKo={isKo} onOpen={onOpenAgent} />
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
        // Desktop starts at 900px, so four columns hold from there. Negative margins tuck the last borders under the card edge.
        <ul className="m-0 -mr-px -mb-px grid list-none grid-cols-4 border-t border-[var(--line-soft)] p-0">
          {shown.map((entry) => (
            <li key={entry.agent.id} className="min-w-0 border-r border-b border-[var(--line-soft)]">
              <AgentCell entry={entry} tr={tr} isKo={isKo} onOpen={onOpenAgent} />
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
