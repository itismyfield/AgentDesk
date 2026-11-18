import type { KeyboardEvent } from "react";
import type { Agent } from "../../types";
import { localeName } from "../../i18n";
import { getCurrentTaskSummary } from "../../lib/agentHelpers";
import { getProviderMeta } from "../../app/providerTheme";
import { Badge, type BadgeTone } from "../../kit/free/badge-clean";
import AgentPersona from "../AgentPersona";
import type { Translator } from "./types";

export type AgentCardViewMode = "grid" | "list";

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

/** Status as text plus tone; working agents get a soft pulse (reduced-motion is handled by the kit badge). */
export function AgentStatusBadge({ status, tr }: { status: Agent["status"]; tr: Translator }) {
  return (
    <Badge tone={STATUS_TONE[status] ?? "neutral"} size="md" dot live={status === "working"}>
      {agentStatusLabel(status, tr)}
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

interface AgentCardProps {
  agent: Agent;
  locale: string;
  tr: Translator;
  topSkills: string[];
  viewMode: AgentCardViewMode;
  selected: boolean;
  onSelect: () => void;
  onKeyDown?: (event: KeyboardEvent<HTMLButtonElement>) => void;
}

/** One agent in the list: a card in the grid, a dense row in the list. Opens the detail on press. */
export default function AgentCard({ agent, locale, tr, topSkills, viewMode, selected, onSelect, onKeyDown }: AgentCardProps) {
  const name = localeName(locale, agent);
  const secondary = agentSecondaryLine(agent, locale);
  const providerMeta = getProviderMeta(agent.cli_provider);
  const task = getCurrentTaskSummary(agent, tr);
  const status = agentStatusLabel(agent.status, tr);
  const done = tr(`완료 ${agent.stats_tasks_done.toLocaleString()}건`, `${agent.stats_tasks_done.toLocaleString()} done`);
  const label = [name, secondary, status, task.value].filter(Boolean).join(", ");
  const muted = agent.status === "offline";

  if (viewMode === "grid") {
    return (
      <button
        type="button"
        data-testid={`agents-grid-card-${agent.id}`}
        data-agent-item={agent.id}
        aria-current={selected || undefined}
        aria-label={label}
        onClick={onSelect}
        onKeyDown={onKeyDown}
        className={`group flex w-full min-w-0 flex-col gap-3 rounded-(--r-lg) bg-(--bg-1) p-3.5 text-left text-[13px] transition-[background-color,box-shadow] duration-150 hover:bg-[color-mix(in_oklch,var(--fg)_3%,var(--bg-1))] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-(--accent) motion-reduce:transition-none ${
          selected ? "shadow-[inset_0_0_0_1.5px_var(--accent)]" : "shadow-[inset_0_0_0_1px_var(--line-soft)]"
        }`}
      >
        <span className="flex min-w-0 items-start gap-2.5">
          <AgentPersona agent={agent} size={32} />
          <span className="flex min-w-0 flex-1 flex-col">
            <strong className={`truncate text-[13px] font-medium leading-5 ${muted ? "text-(--fg-muted)" : "text-(--fg)"}`} title={name}>
              {name}
            </strong>
            <small className="truncate text-xs leading-4 text-(--fg-muted)" title={secondary ?? providerMeta.label}>
              {secondary ?? providerMeta.label}
            </small>
          </span>
          <AgentStatusBadge status={agent.status} tr={tr} />
        </span>
        <span className="flex min-w-0 items-baseline gap-2 text-xs leading-5">
          <span className="shrink-0 text-(--fg-muted)">{task.label}</span>
          <span className="min-w-0 truncate text-(--fg-dim)" title={task.value}>
            {task.value}
          </span>
        </span>
        <span className="flex min-w-0 items-center justify-between gap-2">
          <span className="inline-flex min-w-0 max-w-[60%] items-center gap-1.5 rounded-(--r-sm) bg-(--bg-2) px-1.5 text-[11.5px] leading-[18px] text-(--fg-dim)">
            <span aria-hidden className="size-1.5 shrink-0 rounded-full" style={{ background: providerMeta.color }} />
            <span className="truncate">{providerMeta.label}</span>
          </span>
          <span className="flex min-w-0 items-center gap-2 text-xs text-(--fg-muted)">
            {topSkills[0] ? (
              <span className="hidden min-w-0 truncate @xs:inline" title={topSkills.join(", ")}>
                {topSkills[0]}
              </span>
            ) : null}
            <span className="shrink-0 tabular-nums">{done}</span>
          </span>
        </span>
      </button>
    );
  }

  return (
    <button
      type="button"
      data-testid={`agents-card-${agent.id}`}
      data-agent-item={agent.id}
      aria-current={selected || undefined}
      aria-label={label}
      onClick={onSelect}
      onKeyDown={onKeyDown}
      className={`flex min-h-14 w-full min-w-0 items-center gap-3 rounded-(--r-md) px-3 py-2 text-left text-[13px] transition-colors duration-100 hover:bg-(--bg-2) focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-(--accent) motion-reduce:transition-none ${
        selected ? "bg-(--accent-soft)" : ""
      }`}
    >
      <AgentPersona agent={agent} size={32} />
      <span className="flex min-w-0 flex-1 flex-col @2xl:flex-none @2xl:basis-48">
        <strong className={`truncate font-medium leading-5 ${muted ? "text-(--fg-muted)" : "text-(--fg)"}`} title={name}>
          {name}
        </strong>
        <small className="truncate text-xs leading-4 text-(--fg-muted) @2xl:hidden" title={task.value}>
          {secondary ? `${secondary} · ` : ""}
          {task.value}
        </small>
        <small className="hidden truncate text-xs leading-4 text-(--fg-muted) @2xl:block" title={secondary ?? undefined}>
          {secondary ?? " "}
        </small>
      </span>
      <span className="hidden min-w-0 flex-1 truncate text-(--fg-dim) @2xl:block" title={`${task.label}: ${task.value}`}>
        {task.value}
      </span>
      <span className="flex shrink-0">
        <AgentStatusBadge status={agent.status} tr={tr} />
      </span>
      <span className="hidden w-20 shrink-0 truncate text-xs text-(--fg-dim) @lg:block" title={providerMeta.label}>
        {providerMeta.label}
      </span>
      <span className="hidden min-w-16 shrink-0 whitespace-nowrap text-right text-xs tabular-nums text-(--fg-muted) @lg:block">{done}</span>
    </button>
  );
}
