import type { Agent, KanbanCard, UiLanguage } from "../../types";
import { localeName } from "../../i18n";
import { getProviderMeta } from "../../app/providerTheme";
import MarkdownContent from "../common/MarkdownContent";
import {
  SurfaceActionButton,
  SurfaceCard,
} from "../common/SurfacePrimitives";
import AgentAvatar from "../AgentAvatar";
import {
  formatAgeLabel,
  formatIso,
  buildGitHubIssueUrl,
  getCardStateEnteredAt,
  labelForStatus,
  parseCardMetadata,
  priorityLabel,
} from "./kanban-utils";
import type { Translator } from "./types";

interface BacklogCardDrawerProps {
  card: KanbanCard;
  locale: UiLanguage;
  tr: Translator;
  agentMap: Map<string, Agent>;
  onClose: () => void;
}

function cardAgeMs(card: KanbanCard): number {
  const enteredAt = getCardStateEnteredAt(card);
  if (enteredAt == null) return 0;
  return Math.max(0, Date.now() - enteredAt);
}

function resolveCardAgent(card: KanbanCard, agentMap: Map<string, Agent>): Agent | null {
  return (
    (card.assignee_agent_id ? agentMap.get(card.assignee_agent_id) : undefined) ??
    (card.owner_agent_id ? agentMap.get(card.owner_agent_id) : undefined) ??
    (card.requester_agent_id ? agentMap.get(card.requester_agent_id) : undefined) ??
    null
  );
}

export default function BacklogCardDrawer({
  card,
  locale,
  tr,
  agentMap,
  onClose,
}: BacklogCardDrawerProps) {
  const assignee = card.assignee_agent_id ? agentMap.get(card.assignee_agent_id) : null;
  const owner = card.owner_agent_id ? agentMap.get(card.owner_agent_id) : null;
  const requester = card.requester_agent_id ? agentMap.get(card.requester_agent_id) : null;
  const providerMeta = getProviderMeta(resolveCardAgent(card, agentMap)?.cli_provider);
  const metadata = parseCardMetadata(card.metadata_json);
  const ageLabel = formatAgeLabel(cardAgeMs(card), tr);
  const githubIssueUrl = buildGitHubIssueUrl(
    card.github_repo,
    card.github_issue_number,
    card.github_issue_url,
  );

  const infoItems = [
    {
      label: tr("상태", "Status"),
      value: labelForStatus(card.status, tr),
    },
    {
      label: tr("심각도", "Severity"),
      value: priorityLabel(card.priority, tr),
    },
    {
      label: tr("담당", "Assignee"),
      value: assignee ? localeName(locale, assignee) : tr("미할당", "Unassigned"),
    },
    {
      label: tr("Provider", "Provider"),
      value: providerMeta.label,
    },
    {
      label: tr("Stage Age", "Stage Age"),
      value: ageLabel,
    },
    {
      label: tr("업데이트", "Updated"),
      value: formatIso(card.updated_at, locale),
    },
  ];

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-black/60 p-0 sm:items-center sm:p-4"
      onClick={onClose}
    >
      <div
        data-testid="agents-backlog-drawer"
        role="dialog"
        aria-modal="true"
        aria-label={card.title}
        onClick={(event) => event.stopPropagation()}
        className="max-h-[88svh] w-full max-w-4xl overflow-y-auto rounded-t-[32px] border p-5 sm:max-h-[90vh] sm:rounded-[32px] sm:p-6"
        style={{
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
          paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
        }}
      >
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <span
                className="rounded-full border px-2 py-1 text-[11px] font-medium"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
                  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                  color: "var(--th-text-secondary)",
                }}
              >
                {card.github_issue_number ? `#${card.github_issue_number}` : card.id.slice(0, 8)}
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
                  borderColor: providerMeta.border,
                  background: providerMeta.bg,
                  color: providerMeta.color,
                }}
              >
                {providerMeta.label}
              </span>
            </div>
            <h3
              className="mt-3 text-xl font-semibold tracking-tight"
              style={{ color: "var(--th-text-heading)" }}
            >
              {card.title}
            </h3>
          </div>
          <SurfaceActionButton onClick={onClose} tone="neutral">
            {tr("닫기", "Close")}
          </SurfaceActionButton>
        </div>

        <div className="mt-5 grid gap-3 md:grid-cols-3">
          {infoItems.map((item) => (
            <SurfaceCard
              key={item.label}
              className="space-y-1.5 p-3"
              style={{
                background:
                  "color-mix(in srgb, var(--th-bg-surface) 84%, var(--th-card-bg) 16%)",
              }}
            >
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {item.label}
              </div>
              <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                {item.value}
              </div>
            </SurfaceCard>
          ))}
        </div>

        <div className="mt-5 grid gap-4 xl:grid-cols-[minmax(0,1.3fr)_minmax(280px,0.7fr)]">
          <SurfaceCard className="min-w-0 space-y-3">
            <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
              {tr("설명", "Description")}
            </div>
            {card.description ? (
              <div className="text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
                <MarkdownContent content={card.description} />
              </div>
            ) : (
              <div className="text-sm" style={{ color: "var(--th-text-muted)" }}>
                {tr("설명이 없습니다.", "No description provided.")}
              </div>
            )}

            {card.review_notes ? (
              <SurfaceCard
                className="space-y-2 p-3"
                style={{
                  background:
                    "color-mix(in srgb, var(--th-badge-sky-bg) 82%, var(--th-card-bg) 18%)",
                  borderColor:
                    "color-mix(in srgb, var(--th-accent-info) 26%, var(--th-border) 74%)",
                }}
              >
                <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-accent-info)" }}>
                  {tr("Review Notes", "Review Notes")}
                </div>
                <div className="text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
                  {card.review_notes}
                </div>
              </SurfaceCard>
            ) : null}

            {card.blocked_reason ? (
              <SurfaceCard
                className="space-y-2 p-3"
                style={{
                  background:
                    "color-mix(in srgb, rgba(248,113,113,0.16) 82%, var(--th-card-bg) 18%)",
                  borderColor: "rgba(248,113,113,0.24)",
                }}
              >
                <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "#fca5a5" }}>
                  {tr("Blocked Reason", "Blocked Reason")}
                </div>
                <div className="text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
                  {card.blocked_reason}
                </div>
              </SurfaceCard>
            ) : null}
          </SurfaceCard>

          <div className="space-y-4">
            <SurfaceCard className="space-y-3">
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("참여 에이전트", "Agents")}
              </div>
              {[
                { label: tr("담당", "Assignee"), agent: assignee },
                { label: tr("오너", "Owner"), agent: owner },
                { label: tr("요청자", "Requester"), agent: requester },
              ].map((item) => (
                <div
                  key={item.label}
                  className="flex items-center gap-3 rounded-2xl border px-3 py-2"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                    background:
                      "color-mix(in srgb, var(--th-bg-surface) 84%, var(--th-card-bg) 16%)",
                  }}
                >
                  <div className="text-[11px] font-semibold uppercase tracking-[0.12em]" style={{ color: "var(--th-text-muted)" }}>
                    {item.label}
                  </div>
                  {item.agent ? (
                    <div className="ml-auto flex min-w-0 items-center gap-2">
                      <AgentAvatar agent={item.agent} size={28} rounded="xl" />
                      <div className="min-w-0">
                        <div className="truncate text-sm" style={{ color: "var(--th-text-primary)" }}>
                          {localeName(locale, item.agent)}
                        </div>
                        <div className="truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {getProviderMeta(item.agent.cli_provider).label}
                        </div>
                      </div>
                    </div>
                  ) : (
                    <div className="ml-auto text-sm" style={{ color: "var(--th-text-muted)" }}>
                      {tr("없음", "None")}
                    </div>
                  )}
                </div>
              ))}
            </SurfaceCard>

            <SurfaceCard className="space-y-3">
              <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                {tr("메타", "Metadata")}
              </div>
              <div className="grid gap-2">
                {[
                  { label: tr("생성", "Created"), value: formatIso(card.created_at, locale) },
                  { label: tr("진행 시작", "Started"), value: formatIso(card.started_at, locale) },
                  { label: tr("요청 시각", "Requested"), value: formatIso(card.requested_at, locale) },
                  { label: tr("재시도", "Retries"), value: `${metadata.retry_count ?? 0}` },
                  { label: tr("재디스패치", "Redispatch"), value: `${metadata.redispatch_count ?? 0}` },
                  { label: tr("Failover", "Failover"), value: `${metadata.failover_count ?? 0}` },
                ].map((item) => (
                  <div key={item.label} className="flex items-center justify-between gap-3 text-sm">
                    <span style={{ color: "var(--th-text-muted)" }}>{item.label}</span>
                    <span className="text-right tabular-nums" style={{ color: "var(--th-text-primary)" }}>
                      {item.value}
                    </span>
                  </div>
                ))}
              </div>

              {githubIssueUrl ? (
                <a
                  href={githubIssueUrl}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex items-center rounded-full px-3 py-2 text-sm font-medium hover:underline"
                  style={{ color: "#93c5fd" }}
                >
                  {tr("GitHub 이슈 열기", "Open GitHub Issue")}
                </a>
              ) : null}
            </SurfaceCard>
          </div>
        </div>
      </div>
    </div>
  );
}
