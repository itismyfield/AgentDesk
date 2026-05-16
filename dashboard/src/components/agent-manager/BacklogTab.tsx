import { useMemo, useState } from "react";
import type { Agent, KanbanCard, KanbanCardPriority, KanbanCardStatus, UiLanguage } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import {
  SurfaceCard,
  SurfaceEmptyState,
} from "../common/SurfacePrimitives";
import AgentAvatar from "../AgentAvatar";
import {
  formatAgeLabel,
  getCardStateEnteredAt,
  labelForStatus,
  priorityLabel,
} from "./kanban-utils";
import BacklogCardDrawer from "./BacklogCardDrawer";
import type { Translator } from "./types";

type ProviderFilter = "all" | string;
type SeverityFilter = "all" | KanbanCardPriority;
type StatusFilter = "all" | KanbanCardStatus;
type BacklogSortKey =
  | "id"
  | "title"
  | "assignee"
  | "status"
  | "provider"
  | "severity"
  | "age";
type SortDirection = "asc" | "desc";

interface BacklogTabProps {
  tr: Translator;
  locale: UiLanguage;
  cards: KanbanCard[];
  agents: Agent[];
}

function cardAgeMs(card: KanbanCard): number {
  const enteredAt = getCardStateEnteredAt(card);
  if (enteredAt == null) return 0;
  return Math.max(0, Date.now() - enteredAt);
}

function priorityRank(priority: KanbanCardPriority): number {
  switch (priority) {
    case "urgent":
      return 0;
    case "high":
      return 1;
    case "medium":
      return 2;
    case "low":
    default:
      return 3;
  }
}

function resolveCardAgent(card: KanbanCard, agentMap: Map<string, Agent>): Agent | null {
  return (
    (card.assignee_agent_id ? agentMap.get(card.assignee_agent_id) : undefined) ??
    (card.owner_agent_id ? agentMap.get(card.owner_agent_id) : undefined) ??
    (card.requester_agent_id ? agentMap.get(card.requester_agent_id) : undefined) ??
    null
  );
}

function compareStrings(left: string, right: string): number {
  return left.localeCompare(right, undefined, { numeric: true, sensitivity: "base" });
}

function compareCards(
  left: KanbanCard,
  right: KanbanCard,
  sortKey: BacklogSortKey,
  direction: SortDirection,
  locale: UiLanguage,
  tr: Translator,
  agentMap: Map<string, Agent>,
): number {
  const providerMetaLeft = getProviderMeta(resolveCardAgent(left, agentMap)?.cli_provider);
  const providerMetaRight = getProviderMeta(resolveCardAgent(right, agentMap)?.cli_provider);
  const assigneeLeft = resolveCardAgent(left, agentMap);
  const assigneeRight = resolveCardAgent(right, agentMap);

  let result = 0;
  switch (sortKey) {
    case "id":
      result = compareStrings(
        left.github_issue_number ? `${left.github_issue_number}` : left.id,
        right.github_issue_number ? `${right.github_issue_number}` : right.id,
      );
      break;
    case "title":
      result = compareStrings(left.title, right.title);
      break;
    case "assignee":
      result = compareStrings(
        assigneeLeft ? localeName(locale, assigneeLeft) : "",
        assigneeRight ? localeName(locale, assigneeRight) : "",
      );
      break;
    case "status":
      result = compareStrings(
        labelForStatus(left.status, tr),
        labelForStatus(right.status, tr),
      );
      break;
    case "provider":
      result = compareStrings(providerMetaLeft.label, providerMetaRight.label);
      break;
    case "severity":
      result = priorityRank(left.priority) - priorityRank(right.priority);
      break;
    case "age":
    default:
      result = cardAgeMs(left) - cardAgeMs(right);
      break;
  }

  if (result === 0) {
    result = right.updated_at - left.updated_at;
  }
  return direction === "asc" ? result : -result;
}

function SortHeader({
  label,
  sortKey,
  activeKey,
  direction,
  onToggle,
}: {
  label: string;
  sortKey: BacklogSortKey;
  activeKey: BacklogSortKey;
  direction: SortDirection;
  onToggle: (key: BacklogSortKey) => void;
}) {
  const isActive = sortKey === activeKey;
  return (
    <button
      type="button"
      onClick={() => onToggle(sortKey)}
      className="inline-flex items-center gap-1 rounded-full px-2 py-1 text-left text-[11px] font-semibold uppercase tracking-[0.16em] transition-opacity hover:opacity-100"
      style={{
        color: isActive ? "var(--th-text-heading)" : "var(--th-text-muted)",
        opacity: isActive ? 1 : 0.86,
      }}
    >
      {label}
      <span className="text-[10px]">{isActive ? (direction === "asc" ? "↑" : "↓") : "↕"}</span>
    </button>
  );
}

export default function BacklogTab({
  tr,
  locale,
  cards,
  agents,
}: BacklogTabProps) {
  const agentMap = useMemo(
    () => new Map(agents.map((agent) => [agent.id, agent])),
    [agents],
  );
  const [providerFilter, setProviderFilter] = useState<ProviderFilter>("all");
  const [severityFilter, setSeverityFilter] = useState<SeverityFilter>("all");
  const [statusFilter, setStatusFilter] = useState<StatusFilter>("all");
  const [sortKey, setSortKey] = useState<BacklogSortKey>("age");
  const [sortDirection, setSortDirection] = useState<SortDirection>("desc");
  const [selectedCard, setSelectedCard] = useState<KanbanCard | null>(null);

  const providerOptions = useMemo(() => {
    const providers = new Set<string>();
    for (const card of cards) {
      const provider = resolveCardAgent(card, agentMap)?.cli_provider;
      if (provider) providers.add(provider);
    }
    return [...providers].sort((left, right) => compareStrings(left, right));
  }, [agentMap, cards]);

  const filteredCards = useMemo(() => {
    const next = cards.filter((card) => {
      const provider = resolveCardAgent(card, agentMap)?.cli_provider ?? "";
      if (providerFilter !== "all" && provider !== providerFilter) return false;
      if (severityFilter !== "all" && card.priority !== severityFilter) return false;
      if (statusFilter !== "all" && card.status !== statusFilter) return false;
      return true;
    });
    next.sort((left, right) =>
      compareCards(left, right, sortKey, sortDirection, locale, tr, agentMap),
    );
    return next;
  }, [
    agentMap,
    cards,
    locale,
    providerFilter,
    severityFilter,
    sortDirection,
    sortKey,
    statusFilter,
    tr,
  ]);

  const activeCount = filteredCards.filter((card) =>
    ["requested", "in_progress", "review", "qa_pending", "qa_in_progress", "qa_failed"].includes(card.status),
  ).length;
  const urgentCount = filteredCards.filter((card) => card.priority === "urgent").length;

  const toggleSort = (nextKey: BacklogSortKey) => {
    if (sortKey === nextKey) {
      setSortDirection((prev) => (prev === "asc" ? "desc" : "asc"));
      return;
    }
    setSortKey(nextKey);
    setSortDirection(nextKey === "title" || nextKey === "assignee" ? "asc" : "desc");
  };

  return (
    <div data-testid="agents-backlog-tab" className="space-y-4">
      <SurfaceCard className="space-y-4 rounded-[28px] p-4 sm:p-5">
        <div className="flex flex-col gap-3 lg:flex-row lg:items-start lg:justify-between">
          <div className="min-w-0">
            <div className="text-lg font-semibold tracking-tight" style={{ color: "var(--th-text-heading)" }}>
              {tr("백로그 이슈", "Backlog Issues")}
            </div>
            <div className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "GitHub 미동기화 이슈와 백로그 카드를 한 표면에서 조회합니다. 행이나 카드를 누르면 상세 drawer가 열립니다.",
                "Browse GitHub backlog issues and backlog cards from one surface. Tap a row or card to open the detail drawer.",
              )}
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <span
              className="rounded-full border px-3 py-1 text-[11px] font-medium"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                color: "var(--th-text-secondary)",
              }}
            >
              {tr("총", "Total")} {filteredCards.length}
            </span>
            <span
              className="rounded-full border px-3 py-1 text-[11px] font-medium"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-info) 26%, var(--th-border) 74%)",
                background: "color-mix(in srgb, var(--th-badge-sky-bg) 82%, var(--th-card-bg) 18%)",
                color: "var(--th-text-secondary)",
              }}
            >
              {tr("활성", "Active")} {activeCount}
            </span>
            <span
              className="rounded-full border px-3 py-1 text-[11px] font-medium"
              style={{
                borderColor: "rgba(244,114,182,0.22)",
                background: "rgba(244,114,182,0.12)",
                color: "#f9a8d4",
              }}
            >
              {tr("긴급", "Urgent")} {urgentCount}
            </span>
          </div>
        </div>

        <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
            <select
              data-testid="agents-backlog-filter-provider"
              value={providerFilter}
              onChange={(event) => setProviderFilter(event.target.value)}
              className="rounded-xl px-3 py-2 text-xs outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="all">{tr("Provider: 전체", "Provider: All")}</option>
              {providerOptions.map((provider) => (
                <option key={provider} value={provider}>
                  {getProviderMeta(provider).label}
                </option>
              ))}
            </select>

            <select
              data-testid="agents-backlog-filter-severity"
              value={severityFilter}
              onChange={(event) => setSeverityFilter(event.target.value as SeverityFilter)}
              className="rounded-xl px-3 py-2 text-xs outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="all">{tr("심각도: 전체", "Severity: All")}</option>
              {(["urgent", "high", "medium", "low"] as KanbanCardPriority[]).map((priority) => (
                <option key={priority} value={priority}>
                  {priorityLabel(priority, tr)}
                </option>
              ))}
            </select>

            <select
              data-testid="agents-backlog-filter-status"
              value={statusFilter}
              onChange={(event) => setStatusFilter(event.target.value as StatusFilter)}
              className="rounded-xl px-3 py-2 text-xs outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="all">{tr("상태: 전체", "Status: All")}</option>
              {(["backlog", "ready", "requested", "in_progress", "review", "qa_pending", "qa_in_progress", "qa_failed", "done"] as KanbanCardStatus[]).map((status) => (
                <option key={status} value={status}>
                  {labelForStatus(status, tr)}
                </option>
              ))}
            </select>

            <select
              data-testid="agents-backlog-sort"
              value={`${sortKey}:${sortDirection}`}
              onChange={(event) => {
                const [nextKey, nextDirection] = event.target.value.split(":") as [BacklogSortKey, SortDirection];
                setSortKey(nextKey);
                setSortDirection(nextDirection);
              }}
              className="rounded-xl px-3 py-2 text-xs outline-none"
              style={{
                background: "var(--th-input-bg)",
                border: "1px solid var(--th-input-border)",
                color: "var(--th-text-primary)",
              }}
            >
              <option value="age:desc">{tr("정렬: Age ↓", "Sort: Age ↓")}</option>
              <option value="age:asc">{tr("정렬: Age ↑", "Sort: Age ↑")}</option>
              <option value="severity:asc">{tr("정렬: Severity ↑", "Sort: Severity ↑")}</option>
              <option value="severity:desc">{tr("정렬: Severity ↓", "Sort: Severity ↓")}</option>
              <option value="title:asc">{tr("정렬: 제목 A-Z", "Sort: Title A-Z")}</option>
              <option value="status:asc">{tr("정렬: 상태", "Sort: Status")}</option>
            </select>
        </div>

        {filteredCards.length === 0 ? (
          <SurfaceEmptyState className="py-14 text-center">
            <div className="text-3xl">🗂️</div>
            <div className="mt-2 text-sm">{tr("조건에 맞는 백로그 카드가 없습니다.", "No backlog cards match these filters.")}</div>
          </SurfaceEmptyState>
        ) : (
          <>
          <div
            data-testid="agents-backlog-table"
            className="hidden overflow-hidden rounded-[24px] border lg:block"
            style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)" }}
          >
            <div
              className="grid items-center gap-3 border-b px-4 py-3"
              style={{
                gridTemplateColumns: "88px minmax(260px,1.8fr) minmax(120px,0.9fr) 150px 110px 92px 96px",
                borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                background:
                  "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
              }}
            >
              <SortHeader label="ID" sortKey="id" activeKey={sortKey} direction={sortDirection} onToggle={toggleSort} />
              <SortHeader label={tr("제목", "Title")} sortKey="title" activeKey={sortKey} direction={sortDirection} onToggle={toggleSort} />
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("레포", "Repo")}
              </div>
              <SortHeader label={tr("담당 / 상태", "Assignee / Status")} sortKey="assignee" activeKey={sortKey} direction={sortDirection} onToggle={toggleSort} />
              <SortHeader label="Provider" sortKey="provider" activeKey={sortKey} direction={sortDirection} onToggle={toggleSort} />
              <SortHeader label={tr("경과", "Age")} sortKey="age" activeKey={sortKey} direction={sortDirection} onToggle={toggleSort} />
              <div className="text-right text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("상세", "Open")}
              </div>
            </div>

            <div className="divide-y" style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)" }}>
              {filteredCards.map((card) => {
                const assignee = card.assignee_agent_id ? agentMap.get(card.assignee_agent_id) : null;
                const providerMeta = getProviderMeta(resolveCardAgent(card, agentMap)?.cli_provider);
                return (
                  <button
                    data-testid={`agents-backlog-row-${card.id}`}
                    key={card.id}
                    type="button"
                    onClick={() => setSelectedCard(card)}
                    className="grid w-full items-center gap-3 px-4 py-3 text-left transition-colors hover:bg-white/5"
                    style={{
                      gridTemplateColumns: "88px minmax(260px,1.8fr) minmax(120px,0.9fr) 150px 110px 92px 96px",
                    }}
                  >
                    <div className="truncate text-sm font-medium tabular-nums" style={{ color: "var(--th-text-secondary)" }}>
                      {card.github_issue_number ? `#${card.github_issue_number}` : card.id.slice(0, 8)}
                    </div>
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {card.title}
                      </div>
                    </div>
                    <div className="truncate text-xs font-mono" style={{ color: "var(--th-text-muted)" }}>
                      {card.github_repo ?? tr("레포 없음", "No repo")}
                    </div>
                    <div className="flex min-w-0 items-center gap-2">
                      <AgentAvatar agent={assignee ?? undefined} size={28} rounded="xl" />
                      <div className="min-w-0">
                        <div className="truncate text-sm" style={{ color: "var(--th-text-primary)" }}>
                          {assignee ? localeName(locale, assignee) : tr("미할당", "Unassigned")}
                        </div>
                        <span
                          className="mt-1 inline-flex rounded-full border px-2 py-1 text-[11px] font-medium"
                          style={{
                            borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                            background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                            color: "var(--th-text-secondary)",
                          }}
                        >
                          {labelForStatus(card.status, tr)}
                        </span>
                      </div>
                    </div>
                    <div>
                      <span
                        className="inline-flex rounded-full border px-2 py-1 text-[11px] font-medium"
                        style={{
                          borderColor: providerMeta.border,
                          background: providerMeta.bg,
                          color: providerMeta.color,
                        }}
                      >
                        {providerMeta.label}
                      </span>
                    </div>
                    <div className="text-sm tabular-nums" style={{ color: "var(--th-text-secondary)" }}>
                      {formatAgeLabel(cardAgeMs(card), tr)}
                    </div>
                    <div className="text-right text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                      {tr("상세", "Open")}
                    </div>
                  </button>
                );
              })}
            </div>
          </div>

          <div data-testid="agents-backlog-cards" className="space-y-3 lg:hidden">
            {filteredCards.map((card) => {
              const assignee = card.assignee_agent_id ? agentMap.get(card.assignee_agent_id) : null;
              const providerMeta = getProviderMeta(resolveCardAgent(card, agentMap)?.cli_provider);
              return (
                <SurfaceCard
                  data-testid={`agents-backlog-card-${card.id}`}
                  key={card.id}
                  onClick={() => setSelectedCard(card)}
                  className="cursor-pointer space-y-3 rounded-[28px] p-4 transition-transform hover:-translate-y-0.5"
                >
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <div className="text-xs tabular-nums" style={{ color: "var(--th-text-muted)" }}>
                        {card.github_issue_number ? `#${card.github_issue_number}` : card.id.slice(0, 8)}
                      </div>
                      <div className="mt-1 line-clamp-2 text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {card.title}
                      </div>
                    </div>
                    <span
                      className="rounded-full border px-2 py-1 text-[11px] font-medium"
                      style={{
                        borderColor: providerMeta.border,
                        background: providerMeta.bg,
                        color: providerMeta.color,
                      }}
                    >
                      {providerMeta.label}
                    </span>
                  </div>

                  <div className="flex items-center gap-2">
                    <AgentAvatar agent={assignee ?? undefined} size={30} rounded="xl" />
                    <div className="min-w-0">
                      <div className="truncate text-sm" style={{ color: "var(--th-text-primary)" }}>
                        {assignee ? localeName(locale, assignee) : tr("미할당", "Unassigned")}
                      </div>
                      <div className="truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                        {card.github_repo ?? tr("레포 없음", "No repo")}
                      </div>
                    </div>
                  </div>

                  <div className="flex flex-wrap gap-2">
                    <span
                      className="rounded-full border px-2 py-1 text-[11px] font-medium"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
                        color: "var(--th-text-secondary)",
                      }}
                      >
                      {labelForStatus(card.status, tr)}
                    </span>
                    <span
                      className="rounded-full border px-2 py-1 text-[11px] font-medium"
                      style={{
                        borderColor: "rgba(244,114,182,0.22)",
                        background: "rgba(244,114,182,0.12)",
                        color: "#f9a8d4",
                      }}
                    >
                      {priorityLabel(card.priority, tr)}
                    </span>
                    <span
                      className="rounded-full border px-2 py-1 text-[11px] font-medium"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                        background: "color-mix(in srgb, var(--th-bg-surface) 86%, var(--th-card-bg) 14%)",
                        color: "var(--th-text-muted)",
                      }}
                    >
                      {tr("Age", "Age")} {formatAgeLabel(cardAgeMs(card), tr)}
                    </span>
                  </div>

                  <div
                    className="border-t pt-3 text-right text-xs font-medium"
                    style={{ borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)", color: "var(--th-text-muted)" }}
                  >
                    {tr("상세 보기", "Open Details")}
                  </div>
                </SurfaceCard>
              );
            })}
          </div>
          </>
        )}
      </SurfaceCard>

      {selectedCard ? (
        <BacklogCardDrawer
          card={selectedCard}
          locale={locale}
          tr={tr}
          agentMap={agentMap}
          onClose={() => setSelectedCard(null)}
        />
      ) : null}
    </div>
  );
}
