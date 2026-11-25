import { ChevronRight } from "lucide-react";
import type { KeyboardEvent } from "react";
import type { Agent, Department } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import { Badge, type BadgeTone } from "../../kit/free/badge-clean";
import AgentPersona, { type PersonaState } from "../AgentPersona";
import { RowButton, StatusDot } from "../common/KitPrimitives";
import { agentStateLabel, type HomeAgent, type HomeAgentState } from "../home/homeModel";
import type { Translator } from "./types";

export function agentStatusLabel(status: Agent["status"], tr: Translator): string {
  if (status === "working") return tr("작업 중", "Working");
  if (status === "offline") return tr("오프라인", "Offline");
  if (status === "break") return tr("휴식", "Break");
  return tr("대기", "Idle");
}

const STATUS_TONE: Record<Agent["status"], BadgeTone> = {
  working: "info",
  idle: "neutral",
  break: "warning",
  offline: "danger",
};

const STATE_TONE: Record<HomeAgentState, BadgeTone> = { working: "info", background: "warning", resting: "neutral", noSession: "neutral", unknown: "neutral" };

/** Status as text plus tone; working agents get a soft pulse (reduced-motion is handled by the kit badge). A given
 * `state` shows home's session-based words, as the roster beside it does. */
export function AgentStatusBadge({ status, state, tr }: { status: Agent["status"]; state?: HomeAgentState; tr: Translator }) {
  return (
    <Badge tone={state ? STATE_TONE[state] : STATUS_TONE[status] ?? "neutral"} size="md" dot live={state ? state === "working" : status === "working"}>
      {state ? agentStateLabel(state, tr) : agentStatusLabel(status, tr)}
    </Badge>
  );
}

/** Second line only when it adds something beyond the primary name (which is the alias when one is set). */
export function agentSecondaryLine(agent: Agent, locale: string): string | null {
  const primary = localeName(locale, agent);
  const real = localeName(locale, { ...agent, alias: null });
  if (real !== primary) return real;
  const other = locale === "en" ? agent.name_ko || agent.name : agent.name;
  return other && other !== primary ? other : null;
}

// Home's avatar dots, so a state reads the same on both screens.
export const PERSONA: Record<HomeAgentState, PersonaState> = { working: "working", background: "waiting", resting: "idle", noSession: "idle", unknown: "offline" };

interface AgentRowProps {
  entry: HomeAgent;
  /** State and what it does now, e.g. home's agent line or the live tool line. */
  line: string;
  department?: Department;
  /** Settings to check (no department or Discord route): an orange dot. */
  warn: boolean;
  locale: string;
  tr: Translator;
  selected: boolean;
  onSelect: () => void;
  onKeyDown?: (event: KeyboardEvent<HTMLButtonElement>) => void;
}

/** One agent in the roster. Columns join as the list grows (container queries); narrow lists keep the name, the
 * state line and a chevron. */
export default function AgentRow({ entry, line, warn, locale, tr, selected, onSelect, onKeyDown }: AgentRowProps) {
  const { agent } = entry;
  const name = localeName(locale, agent);
  const secondary = agentSecondaryLine(agent, locale);
  const provider = getProviderMeta(agent.cli_provider).label;
  const checkLabel = tr("설정 확인 필요", "Settings to check");
  return (
    <RowButton
      data-testid={`agents-row-${agent.id}`}
      data-agent-item={agent.id}
      selected={selected}
      aria-label={[name, secondary, line, warn ? checkLabel : null].filter(Boolean).join(", ")}
      onClick={onSelect}
      onKeyDown={onKeyDown}
      className="min-h-14 py-2.5 text-[13px]"
    >
      <span aria-hidden className="flex"><AgentPersona agent={agent} state={PERSONA[entry.state]} size={32} /></span>
      <span className="min-w-0 flex-1 @2xl:flex-none @2xl:basis-44">
        <span className="block truncate text-sm leading-5 font-medium" title={name}>{name}</span>
        <span className="block truncate text-xs leading-4 text-[var(--fg-muted)] @2xl:hidden" title={line}>{line}</span>
        <span className="hidden truncate text-xs leading-4 text-[var(--fg-muted)] @2xl:block">{secondary ?? " "}</span>
      </span>
      <span className="hidden min-w-0 flex-1 truncate text-[var(--fg-dim)] @2xl:block" title={line}>{line}</span>
      <span className="hidden shrink-0 text-right text-xs whitespace-nowrap text-[var(--fg-dim)] @lg:block">{provider}</span>
      <span className="hidden w-18 shrink-0 text-right text-xs whitespace-nowrap text-[var(--fg-muted)] tabular-nums @xl:block">
        {tr(`완료 ${agent.stats_tasks_done.toLocaleString()}`, `${agent.stats_tasks_done.toLocaleString()} done`)}
      </span>
      <span className="flex w-2 shrink-0 justify-center" title={warn ? checkLabel : undefined}>{warn ? <StatusDot tone="warn" /> : null}</span>
      <ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)] @2xl:hidden" />
    </RowButton>
  );
}
