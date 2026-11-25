import { ArrowLeft, ChevronRight, CloudOff, FileQuestion, Search, Unplug } from "lucide-react";
import { useState, type ReactNode } from "react";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { Checkbox } from "../../kit/free/checkbox-clean";
import { EmptyState } from "../../kit/free/empty-state-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import type { Agent } from "../../types";
import AgentPersona from "../AgentPersona";
import { Cap, CardNote, CardSkeleton, KitCard, RowButton, StatusDot } from "../common/KitPrimitives";
import { matchesAgent, type AuditState } from "./auditModel";
import type { AuditSelection } from "./useAuditSelection";

type Tr = (ko: string, en: string) => string;

interface AuditWorkspaceProps {
  selection: AuditSelection;
  phone: boolean;
  isKo: boolean;
  tr: Tr;
  /** Second line of an agent row: what this tab already knows about the agent. */
  caption: (agent: Agent) => string;
  /** Optional warning dot when cached tab data flags items to inspect. */
  warning?: (agent: Agent) => boolean;
  /** The open agent's detail; phones show it instead of the list. */
  children: ReactNode;
}

/** Agent picker beside the detail on desktop; on phones a list that opens one agent at a time. */
export function AuditWorkspace({ selection, phone, isKo, tr, caption, warning, children }: AuditWorkspaceProps) {
  const [query, setQuery] = useState("");
  const { list, agent } = selection;
  const testCount = selection.testCount ?? 0;
  if (list.length === 0 && testCount === 0) {
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
    <nav
      aria-label={tr("에이전트", "Agents")}
      data-testid="audit-agent-list"
      className={phone ? "min-w-0" : "sticky top-4 flex max-h-[calc(100dvh-10rem)] flex-col"}
    >
      <KitCard className="flex min-h-0 flex-col">
        <div className="border-b border-[var(--line-soft)] px-3 py-2.5">
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
          {testCount > 0 ? (
            <div className="mt-2 flex items-center">
              <Checkbox
                data-testid="audit-include-tests"
                checked={Boolean(selection.includeTests)}
                onChange={(event) => selection.setIncludeTests?.(event.target.checked)}
                label={tr(`테스트 포함 (${testCount})`, `Tests (${testCount})`)}
              />
            </div>
          ) : null}
        </div>
        <ul className="m-0 min-h-0 list-none overflow-y-auto p-0">
          {rows.map((item) => {
            const current = !phone && item.id === agent?.id;
            return (
              <li key={item.id} className="border-t border-[var(--line-soft)] first:border-t-0">
                <RowButton
                  selected={current}
                  aria-current={current ? "true" : undefined}
                  data-testid={`audit-agent-${item.id}`}
                  onClick={() => selection.selectAgent(item.id)}
                  className={phone ? "min-h-14 py-2.5" : "min-h-12 px-3 py-2.5 sm:px-3"}
                >
                  <AgentPersona agent={item} size={32} />
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-sm leading-5 font-medium text-[var(--fg)]">{localeName(isKo ? "ko" : "en", item)}</span>
                    <Cap className="block truncate">{caption(item)}</Cap>
                  </span>
                  {!phone && warning?.(item) ? <StatusDot tone="warn" /> : null}
                  {phone ? <ChevronRight size={16} aria-hidden className="shrink-0 text-[var(--fg-muted)]" /> : null}
                </RowButton>
              </li>
            );
          })}
        </ul>
        {rows.length === 0 ? (
          <CardNote className="border-t-0">
            {list.length === 0 && testCount > 0
              ? tr("테스트 에이전트만 있어요. 테스트 포함을 켜면 보여요.", "Only test agents are bound; enable Tests above.")
              : tr("맞는 에이전트가 없어요.", "No matching agents.")}
          </CardNote>
        ) : null}
      </KitCard>
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

/** Detail head both tabs use: sticky 56px top bar on phones, inline card header on desktop. */
export function AuditDetailHeader({ agent, isKo, meta, actions, onBack, phone, tr }: {
  agent: Agent;
  isKo: boolean;
  meta: string;
  actions: ReactNode;
  onBack?: () => void;
  phone: boolean;
  tr: Tr;
}) {
  return (
    // Phones pin one 56px bar at the top of PageFrame's scrollport; negative margins undo PageFrame's padding.
    <div
      className={
        phone
          ? "sticky top-0 z-20 -mx-4 -mt-4 mb-1 flex h-14 min-w-0 items-center gap-1 border-b border-[var(--line-soft)] bg-[var(--bg)] px-2 sm:-mx-6"
          : "flex min-w-0 flex-wrap items-center gap-2.5"
      }
      data-testid="audit-detail-head"
    >
      {onBack ? (
        <IconButton
          className={phone ? "min-h-11 min-w-11" : undefined}
          icon={<ArrowLeft />}
          label={tr("에이전트 목록으로", "Back to agents")}
          onClick={onBack}
          data-testid="audit-back"
        />
      ) : null}
      <AgentPersona agent={agent} size={phone ? 32 : 40} />
      <div className={`min-w-0 flex-1 ${phone ? "ml-1.5" : ""}`}>
        <h2 className={`m-0 truncate font-semibold text-[var(--fg)] ${phone ? "text-[15px] leading-5" : "text-base leading-6"}`}>
          {localeName(isKo ? "ko" : "en", agent)}
        </h2>
        <Cap className={phone ? "block truncate" : "block leading-5 break-keep"} title={phone ? meta : undefined} data-testid="audit-detail-meta">
          {meta}
        </Cap>
      </div>
      <div className={`flex shrink-0 items-center ${phone ? "" : "gap-2"}`}>{actions}</div>
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
      <KitCard data-testid="audit-state-loading" className="[&>*]:border-t-0">
        <CardSkeleton label={tr("불러오는 중", "Loading")} rows={3} />
      </KitCard>
    );
  }
  const copy = {
    unavailable: [<FileQuestion aria-hidden />, tr("세션도 저장본도 없어요", "No session or saved copy"), tr("이 provider로 세션이 시작되면 여기에 보여요.", "It shows up here once a session starts on this provider.")],
    unbound: [<Unplug aria-hidden />, tr("이 provider에 연결된 채널이 없어요", "No channel for this provider"), tr("에이전트에 채널을 연결한 뒤 다시 시도해 주세요.", "Bind a channel to the agent, then try again.")],
    error: [<CloudOff aria-hidden />, tr("불러오지 못했어요", "Could not load"), tr("서버 연결을 확인하고 다시 시도해 주세요.", "Check the server connection and try again.")],
  }[state];
  return (
    <KitCard pad="md">
      <EmptyState
        icon={copy[0]}
        title={copy[1]}
        text={copy[2]}
        data-testid={`audit-state-${state}`}
        action={<Button variant="secondary" size="sm" loading={retrying} onClick={onRetry}>{tr("다시 시도", "Try again")}</Button>}
      />
    </KitCard>
  );
}
