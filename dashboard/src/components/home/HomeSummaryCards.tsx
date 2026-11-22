import { ChevronRight } from "lucide-react";
import { useMemo, useState, type CSSProperties } from "react";
import { Button } from "../../kit/free/button-clean";
import type { Agent } from "../../types";
import AgentPersona, { type PersonaState } from "../AgentPersona";
import {
  Cap, CardNote, CardSkeleton, Chip, HeaderLink, InspSection, KeyValue, RowButton, RowLink, SectionCard, StatusDot,
} from "../common/KitPrimitives";
import { formatClock } from "../dashboard/routineModel";
import {
  AGENT_STATES, QUEUE_PATH, agentLine, agentName, agentStateLabel, buildSystemRows, systemNeedsAttention, todoStatus,
  type HomeAgent, type HomeAgentState, type HomeTone, type SourceState, type TodoItem, type Tr,
} from "./homeModel";
import { loadHomeHealth, usePolled, type HomeCampaigns, type Loaded } from "./useHomeData";

// Home's first three sections: what needs a person, how the system is, and who is working.

const TODO_ROWS = 5;
const FULL_TOUCH_BUTTON_STYLE = {
  "--aui-button-clean-h": "44px",
  "--aui-button-clean-muted": "var(--fg-dim)",
  width: "100%",
} as CSSProperties;

/** What only a person can move, at the top of home. Each row opens the screen where it is handled. */
export function HomeTodoCard({ items, agents, cards, campaigns, tr, className }: {
  items: TodoItem[];
  agents: Agent[];
  cards: SourceState;
  campaigns: SourceState;
  tr: Tr;
  className?: string;
}) {
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
                return (
                  <li key={item.key} className="border-t border-[var(--line-soft)] first:border-t-0">
                    <RowLink to={item.to} className="min-h-16">
                      <Chip tone={item.tone} className="justify-center sm:min-w-[4.5rem]">{item.chip}</Chip>
                      <span className="min-w-0 flex-1">
                        <span className="block truncate text-sm leading-5 font-medium" title={item.title}>{item.title}</span>
                        <Cap className="mt-0.5 block truncate">{item.detail}</Cap>
                      </span>
                      {assignee ? (
                        <span aria-hidden className="hidden sm:inline-flex">
                          <AgentPersona agent={assignee} size={24} />
                        </span>
                      ) : null}
                      <ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" />
                    </RowLink>
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
    <SectionCard title={tr("에이전트", "Agents")} count={isMobile ? null : entries.length} aside={legend} testId="home-agents">
      {entries.length === 0 ? (
        <CardNote>{tr("등록된 에이전트가 없어요.", "No agents yet.")}</CardNote>
      ) : isMobile ? (
        <ul className="m-0 list-none border-t border-[var(--line-soft)] p-0">
          {shown.length === 0 ? (
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
