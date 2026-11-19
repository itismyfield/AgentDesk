import { ChevronRight } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { getHealthDetail } from "../../api/settingsRuntime";
import type { Agent } from "../../types";
import AgentPersona, { type PersonaState } from "../AgentPersona";
import { formatClock } from "../dashboard/routineModel";
import { CardNote, CardSkeleton, Chip, Dot, HeaderLink, HomeCard, Muted, ROW_HOVER, toneText } from "./HomeCard";
import {
  AGENT_STATES, QUEUE_PATH, agentLine, agentName, agentStateLabel, buildSystemRows,
  type HomeAgent, type HomeAgentState, type HomeTone, type TodoItem, type Tr,
} from "./homeModel";
import { usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

// Home's first three sections: what needs a person, how the system is, and who is working.

const TODO_ROWS = 5;

/** What only a person can move, at the top of home. Each row opens the screen where it is handled. */
export function HomeTodoCard({ items, loading, cardsFailed, tr, className }: {
  items: TodoItem[];
  loading: boolean;
  cardsFailed: boolean;
  tr: Tr;
  className?: string;
}) {
  const total = items.reduce((sum, item) => sum + item.count, 0);
  const hidden = items.length - TODO_ROWS;
  return (
    <HomeCard
      title={tr("할 일", "To do")}
      count={loading ? null : total}
      countTone={total > 0 ? "warn" : undefined}
      aside={<HeaderLink to={QUEUE_PATH}>{tr("대기열에서 보기", "Open queue")}</HeaderLink>}
      testId="home-todo"
      className={className}
    >
      {loading ? (
        <CardSkeleton label={tr("할 일 불러오는 중", "Loading to-dos")} />
      ) : (
        <>
          {items.length === 0 ? <CardNote>{tr("지금 할 일이 없어요.", "Nothing needs you right now.")}</CardNote> : null}
          <ul className="m-0 list-none p-0">
            {items.slice(0, TODO_ROWS).map((item) => (
              <li key={item.key} className="border-t border-[var(--line-soft)]">
                <Link
                  to={item.to}
                  className={`flex min-h-16 items-center gap-3 px-4 py-3 ${ROW_HOVER} focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]`}
                >
                  <span className="flex w-[4.75rem] shrink-0 justify-center"><Chip tone={item.tone}>{item.chip}</Chip></span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-sm font-medium" title={item.title}>{item.title}</span>
                    <Muted className="block truncate">{item.detail}</Muted>
                  </span>
                  <ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" />
                </Link>
              </li>
            ))}
          </ul>
          {hidden > 0 ? (
            <div className="border-t border-[var(--line-soft)] px-4 py-1">
              <HeaderLink to={QUEUE_PATH}>{tr(`${hidden}건 더 보기`, `${hidden} more`)}</HeaderLink>
            </div>
          ) : null}
          {cardsFailed ? <CardNote>{tr("작업 카드를 읽지 못했어요. 잠시 뒤 다시 읽어요.", "Could not read task cards. Retrying shortly.")}</CardNote> : null}
        </>
      )}
    </HomeCard>
  );
}

/** Server, Discord bots, Memento, campaign runs and the queue, one dot and value per line. */
export function HomeSystemCard({ campaigns, tr, className }: { campaigns: Loaded<HomeCampaigns>; tr: Tr; className?: string }) {
  const health = usePolled(getHealthDetail, 30_000);
  const rows = buildSystemRows({
    health: health.data,
    healthFailed: health.failed,
    legacy: campaigns.data?.legacy ?? null,
    documents: campaigns.data?.documents ?? [],
    tr,
  });
  const publicOnly = health.data?.publicOnly;
  return (
    <HomeCard
      title={tr("시스템", "System")}
      aside={health.at ? <Muted className="tabular-nums">{tr(`${formatClock(health.at)} 확인`, `Checked ${formatClock(health.at)}`)}</Muted> : null}
      testId="home-system"
      className={className}
    >
      <dl className="m-0 flex flex-col gap-2.5 border-t border-[var(--line-soft)] px-4 py-3.5">
        {rows.map((row) => (
          <div key={row.key} data-testid={`home-system-${row.key}`} className="flex min-w-0 items-center gap-3 text-[13px] leading-5">
            <dt className="flex shrink-0 items-center gap-2 text-[var(--fg-muted)]"><Dot tone={row.tone} />{row.label}</dt>
            <dd className="m-0 min-w-0 flex-1 truncate text-right" style={{ color: toneText(row.tone) }} title={row.value}>{row.value}</dd>
          </div>
        ))}
      </dl>
      {publicOnly ? (
        <CardNote>
          {publicOnly === "forbidden"
            ? tr("자세한 값은 서버가 도는 맥에서 열 때만 보여요.", "Details show only on the Mac running the server.")
            : tr("자세한 상태를 읽지 못해 요약만 보여요.", "Detailed status did not load; showing the summary.")}
        </CardNote>
      ) : null}
    </HomeCard>
  );
}

// The legend dots use the persona dot colors, so a count and its avatars read the same. A session opens when a
// Discord message arrives, so an agent without one is at rest: gray, never the error color.
const PERSONA: Record<HomeAgentState, PersonaState> = { working: "working", background: "waiting", resting: "idle", noSession: "idle" };
const LEGEND_TONE: Record<HomeAgentState, HomeTone> = { working: "ok", background: "warn", resting: "neutral", noSession: "neutral" };
/** Desktop shows two rows of four: up to seven agents and the fold, unless more are busy. */
const DESKTOP_SHOWN = 7;

const CELL = `flex min-h-14 w-full min-w-0 items-center gap-2.5 px-4 py-3 text-left ${ROW_HOVER} focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]`;

function AgentCell({ entry, tr, isKo, onOpen }: { entry: HomeAgent; tr: Tr; isKo: boolean; onOpen: (agent: Agent) => void }) {
  const line = agentLine(entry, tr, isKo);
  return (
    <button type="button" className={CELL} onClick={() => onOpen(entry.agent)}>
      <span aria-hidden className="flex"><AgentPersona agent={entry.agent} state={PERSONA[entry.state]} size={32} /></span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">{agentName(entry.agent, isKo)}</span>
        <span className="block truncate text-xs text-[var(--fg-muted)]" title={line}>{line}</span>
      </span>
    </button>
  );
}

/** Everyone with a state dot and one line of what they do. Busy agents lead; desktop folds the rest into one cell,
 * phones list only busy agents. */
export function HomeAgentsCard({ entries, isMobile, tr, isKo, onOpenAgent }: {
  entries: HomeAgent[];
  isMobile: boolean;
  tr: Tr;
  isKo: boolean;
  onOpenAgent: (agent: Agent) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const counts = Object.fromEntries(AGENT_STATES.map((state) => [state, entries.filter((entry) => entry.state === state).length])) as Record<HomeAgentState, number>;
  const busy = counts.working + counts.background;
  const legendStates = isMobile ? AGENT_STATES.filter((state) => counts[state] > 0) : AGENT_STATES;
  const legend = (
    <Muted className="flex min-h-11 flex-wrap items-center gap-x-3.5 gap-y-1">
      {legendStates.map((state) => (
        <span key={state} className="flex items-center gap-1.5 tabular-nums"><Dot tone={LEGEND_TONE[state]} />{agentStateLabel(state, tr)} {counts[state]}</span>
      ))}
    </Muted>
  );

  const preferred = entries.slice(0, isMobile ? busy : Math.max(DESKTOP_SHOWN, busy));
  const folds = isMobile ? preferred.length < entries.length : entries.length > DESKTOP_SHOWN + 1 && preferred.length < entries.length;
  const shown = expanded || !folds ? entries : preferred;
  const rest = entries.length - preferred.length;
  const toggleLabel = expanded
    ? tr("접기", "Show less")
    : isMobile ? tr(`${rest}명 더 보기`, `Show ${rest} more`) : tr(`나머지 ${rest}명`, `${rest} more`);

  return (
    <HomeCard title={tr("에이전트", "Agents")} count={entries.length} aside={legend} testId="home-agents">
      {entries.length === 0 ? (
        <p className="m-0 border-t border-[var(--line-soft)] px-4 py-5 text-sm text-[var(--fg-muted)]">{tr("등록된 에이전트가 없어요.", "No agents yet.")}</p>
      ) : isMobile ? (
        <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
          {shown.length === 0 ? <li className="px-4 py-4 text-sm text-[var(--fg-muted)]">{tr("모두 쉬는 중이에요.", "Everyone is resting.")}</li> : null}
          {shown.map((entry) => (
            <li key={entry.agent.id} className="border-b border-[var(--line-soft)] last:border-b-0">
              <AgentCell entry={entry} tr={tr} isKo={isKo} onOpen={onOpenAgent} />
            </li>
          ))}
          {folds ? (
            <li className="flex justify-center border-t border-[var(--line-soft)] p-1">
              <button type="button" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)} className={`min-h-11 rounded-[var(--r-md)] px-4 text-sm font-medium text-[var(--fg)] ${ROW_HOVER}`}>
                {toggleLabel}
              </button>
            </li>
          ) : null}
        </ul>
      ) : (
        // Negative margins tuck the last column's and row's borders under the card edge.
        <ul className="m-0 -mr-px -mb-px grid list-none grid-cols-2 border-t border-[var(--line-soft)] p-0 lg:grid-cols-4">
          {shown.map((entry) => (
            <li key={entry.agent.id} className="min-w-0 border-r border-b border-[var(--line-soft)]">
              <AgentCell entry={entry} tr={tr} isKo={isKo} onOpen={onOpenAgent} />
            </li>
          ))}
          {folds ? (
            <li className="min-w-0 border-r border-b border-[var(--line-soft)]">
              <button type="button" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)} className={`${CELL} text-sm text-[var(--fg-muted)]`}>
                {expanded ? null : (
                  <span aria-hidden className="flex">
                    {entries.slice(preferred.length, preferred.length + 3).map((entry, index) => (
                      <span key={entry.agent.id} className={index ? "-ml-1.5" : undefined}><AgentPersona agent={entry.agent} state={PERSONA[entry.state]} size={24} /></span>
                    ))}
                  </span>
                )}
                {toggleLabel}
              </button>
            </li>
          ) : null}
        </ul>
      )}
    </HomeCard>
  );
}
