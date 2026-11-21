import { ChevronLeft, ChevronRight, CloudOff, FileQuestion, Search, Unplug } from "lucide-react";
import { useState, type ReactNode } from "react";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { Skeleton } from "../../kit/free/skeleton-clean";
import { TextField } from "../../kit/free/text-field-clean";
import type { Agent } from "../../types";
import AgentPersona from "../AgentPersona";
import { SETTINGS_CARD_CLASS } from "../settings/SettingsPrimitives";
import { matchesAgent, type AuditState } from "./auditModel";
import type { AuditSelection } from "./useAuditSelection";

type Tr = (ko: string, en: string) => string;

export const AUDIT_CARD_CLASS = SETTINGS_CARD_CLASS;

interface AuditWorkspaceProps {
  selection: AuditSelection;
  phone: boolean;
  isKo: boolean;
  tr: Tr;
  /** Second line of an agent row: what this tab already knows about the agent. */
  caption: (agent: Agent) => string;
  /** The open agent's detail; phones show it instead of the list. */
  children: ReactNode;
}

/** Agent picker beside the detail on desktop; on phones a list that opens one agent at a time. */
export function AuditWorkspace({ selection, phone, isKo, tr, caption, children }: AuditWorkspaceProps) {
  const [query, setQuery] = useState("");
  const { list, agent } = selection;
  if (list.length === 0) {
    return (
      <EmptyState
        icon={<Unplug aria-hidden />}
        title={tr("채널에 연결된 에이전트가 없어요", "No agents with a channel")}
        text={tr("Discord 채널에 연결된 에이전트만 여기에 보여요.", "Only agents bound to a Discord channel show up here.")}
        data-testid="audit-no-agents"
      />
    );
  }
  if (phone && agent) return <>{children}</>;

  const rows = list.filter((item) => matchesAgent(item, query));
  const picker = (
    <nav aria-label={tr("에이전트", "Agents")} data-testid="audit-agent-list" className={`${AUDIT_CARD_CLASS} flex flex-col overflow-hidden ${phone ? "" : "sticky top-4 max-h-[calc(100dvh-10rem)]"}`}>
      <div className="border-b border-[var(--line-soft)] p-2.5">
        <TextField
          type="search"
          size={phone ? "lg" : "sm"}
          className="pointer-coarse:[--h:44px]!"
          aria-label={tr("에이전트 찾기", "Find an agent")}
          placeholder={tr("에이전트 찾기", "Find an agent")}
          prefix={<Search aria-hidden />}
          value={query}
          onChange={(event) => setQuery(event.target.value)}
        />
      </div>
      <ul className="m-0 min-h-0 list-none overflow-y-auto p-0 [&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]">
        {rows.map((item) => {
          const current = !phone && item.id === agent?.id;
          return (
            <li key={item.id}>
              <button
                type="button"
                aria-current={current ? "true" : undefined}
                data-testid={`audit-agent-${item.id}`}
                onClick={() => selection.selectAgent(item.id)}
                className="flex min-h-14 w-full min-w-0 items-center gap-3 px-3 py-2 text-left hover:bg-surface-subtle aria-[current=true]:bg-[color-mix(in_oklab,var(--accent)_9%,transparent)] aria-[current=true]:shadow-[inset_2px_0_0_var(--accent)]"
              >
                <AgentPersona agent={item} size={32} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-medium text-th-text-primary">{localeName(isKo ? "ko" : "en", item)}</span>
                  <span className="block truncate text-xs text-th-text-muted">{caption(item)}</span>
                </span>
                {phone ? <ChevronRight size={16} aria-hidden className="shrink-0 text-th-text-muted" /> : null}
              </button>
            </li>
          );
        })}
      </ul>
      {rows.length === 0 ? <p className="m-0 px-3 py-4 text-[13px] text-th-text-muted">{tr("맞는 에이전트가 없어요.", "No matching agents.")}</p> : null}
    </nav>
  );
  if (phone) return picker;
  return (
    <div className="flex min-w-0 items-start gap-5">
      <div className="w-[248px] shrink-0 self-stretch">{picker}</div>
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  );
}

/** Detail head both tabs use: back on phones, the agent, a meta line and actions. */
export function AuditDetailHeader({ agent, isKo, meta, actions, onBack, tr }: {
  agent: Agent;
  isKo: boolean;
  meta: ReactNode;
  actions: ReactNode;
  onBack?: () => void;
  tr: Tr;
}) {
  return (
    <div className="flex min-w-0 items-center gap-3">
      {onBack ? <IconButton icon={<ChevronLeft />} label={tr("에이전트 목록으로", "Back to agents")} className="-ml-2" onClick={onBack} data-testid="audit-back" /> : null}
      <AgentPersona agent={agent} size={40} />
      <div className="min-w-0 flex-1">
        <h2 className="m-0 truncate text-base font-semibold text-th-text-heading">{localeName(isKo ? "ko" : "en", agent)}</h2>
        <p className="m-0 text-xs leading-5 break-keep text-th-text-muted" data-testid="audit-detail-meta">{meta}</p>
      </div>
      <div className="flex shrink-0 items-center gap-2">{actions}</div>
    </div>
  );
}

/** Loading, empty and failed reads; a missing channel and a failed request read differently and both offer a retry. */
export function AuditRequestState({ state, retrying, onRetry, tr }: {
  state: Exclude<AuditState, "ready">;
  retrying: boolean;
  onRetry: () => void;
  tr: Tr;
}) {
  if (state === "loading") {
    return (
      <div role="status" aria-label={tr("불러오는 중", "Loading")} className="flex flex-col gap-3 p-4" data-testid="audit-state-loading">
        {[0, 1, 2].map((row) => <Skeleton key={row} height={14} width={row === 1 ? "60%" : "85%"} />)}
      </div>
    );
  }
  const copy = {
    unavailable: [<FileQuestion aria-hidden />, tr("세션도 저장본도 없어요", "No session or saved copy"), tr("이 provider로 세션이 시작되면 여기에 보여요.", "It shows up here once a session starts on this provider.")],
    unbound: [<Unplug aria-hidden />, tr("이 provider에 연결된 채널이 없어요", "No channel for this provider"), tr("에이전트에 채널을 연결한 뒤 다시 시도해 주세요.", "Bind a channel to the agent, then try again.")],
    error: [<CloudOff aria-hidden />, tr("불러오지 못했어요", "Could not load"), tr("서버 연결을 확인하고 다시 시도해 주세요.", "Check the server connection and try again.")],
  }[state];
  return (
    <EmptyState
      icon={copy[0]}
      title={copy[1]}
      text={copy[2]}
      data-testid={`audit-state-${state}`}
      action={<Button variant="secondary" size="sm" loading={retrying} onClick={onRetry}>{tr("다시 시도", "Try again")}</Button>}
    />
  );
}
