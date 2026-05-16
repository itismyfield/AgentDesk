import { useEffect, useMemo, useState } from "react";
import {
  getAgentDispatchedSessions,
  getAgentTranscripts,
  getAuditLogs,
  getDiscordChannelInfo,
  getSkillRanking,
  type DiscordChannelInfo,
} from "../../api";
import { getProviderMeta } from "../../app/providerTheme";
import { localeName } from "../../i18n";
import type {
  Agent,
  AuditLogEntry,
  Department,
  DispatchedSession,
  KanbanCard,
  UiLanguage,
} from "../../types";
import AgentAvatar from "../AgentAvatar";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import { Drawer } from "../common/overlay";
import {
  describeDispatchedSession,
  formatDiscordSummary,
} from "../agent-manager/discord-routing";
import { getAgentLevel, getAgentTitle } from "../agent-manager/agentProgress";
import type { OfficeManualIntervention } from "./officeAgentState";
import {
  buildCardIssueUrl,
  buildSkillRows,
  ensurePrimarySession,
  formatDateTime,
  formatElapsed,
  hydrateSession,
  normalizeTimestampMs,
  sessionSortValue,
  t,
  type OfficeSkillRow,
} from "./OfficeAgentDrawerModel";

interface OfficeAgentDrawerProps {
  open: boolean;
  agent: Agent;
  departments: Department[];
  locale: UiLanguage;
  isKo: boolean;
  spriteMap?: Map<string, number>;
  currentCard?: KanbanCard | null;
  manualIntervention?: OfficeManualIntervention | null;
  onClose: () => void;
}

export default function OfficeAgentDrawer({
  open,
  agent,
  departments,
  locale,
  isKo,
  spriteMap,
  currentCard,
  manualIntervention,
  onClose,
}: OfficeAgentDrawerProps) {
  const [loading, setLoading] = useState(true);
  const [sessions, setSessions] = useState<DispatchedSession[]>([]);
  const [sessionChannelsById, setSessionChannelsById] = useState<Record<string, DiscordChannelInfo>>({});
  const [skillRows, setSkillRows] = useState<OfficeSkillRow[]>([]);
  /* Restored "감사 / Audit" section: each entry is a kanban_card lifecycle
     event (state transition / hook fire) for cards assigned to this agent.
     Backend now enriches the row with the card title + GitHub issue number
     so the panel renders meaningful summaries instead of the raw
     `kanban_card:UUID` strings the previous implementation surfaced
     (deleted in #1258). */
  const [auditLogs, setAuditLogs] = useState<AuditLogEntry[]>([]);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);

    void (async () => {
      const [sessionsResult, rankingResult, transcriptsResult, auditResult] =
        await Promise.allSettled([
          getAgentDispatchedSessions(agent.id),
          getSkillRanking("7d", 100),
          getAgentTranscripts(agent.id, 80),
          getAuditLogs(20, { agentId: agent.id }),
        ]);

      if (cancelled) return;

      const loadedSessions =
        sessionsResult.status === "fulfilled"
          ? ensurePrimarySession(
              agent,
              sessionsResult.value
                .map((session) => hydrateSession(session, agent))
                .sort((left, right) => {
                  const leftWorking = left.status === "working" ? 0 : 1;
                  const rightWorking = right.status === "working" ? 0 : 1;
                  if (leftWorking !== rightWorking) return leftWorking - rightWorking;
                  return sessionSortValue(right) - sessionSortValue(left);
                }),
            )
          : ensurePrimarySession(agent, []);

      const rankingRows =
        rankingResult.status === "fulfilled" ? rankingResult.value.byAgent : [];
      const transcriptRows =
        transcriptsResult.status === "fulfilled" ? transcriptsResult.value : [];
      const loadedAuditLogs =
        auditResult.status === "fulfilled" ? auditResult.value : [];

      const channelIds = Array.from(
        new Set(
          loadedSessions
            .map((session) => session.thread_channel_id ?? null)
            .filter((value): value is string => Boolean(value)),
        ),
      );

      const channelEntries = await Promise.all(
        channelIds.map(async (channelId) => {
          try {
            return [channelId, await getDiscordChannelInfo(channelId)] as const;
          } catch {
            return null;
          }
        }),
      );

      if (cancelled) return;

      setSessions(loadedSessions);
      setSessionChannelsById(
        Object.fromEntries(
          channelEntries.filter(
            (entry): entry is readonly [string, DiscordChannelInfo] => entry !== null,
          ),
        ),
      );
      setSkillRows(buildSkillRows(agent, rankingRows, transcriptRows));
      setAuditLogs(loadedAuditLogs);
      setLoading(false);
    })();

    return () => {
      cancelled = true;
    };
  }, [agent, open]);

  /* Map a kanban_card_id → most recent dispatched session so each audit row
     can deeplink to the Discord turn the agent ran for that card. The
     dispatched-sessions endpoint already returns guild_id + channel deeplink
     URLs, so we just look up by entity_id and reuse the existing summary
     formatter (no extra round-trip needed). */
  const sessionByCardId = useMemo(() => {
    const map = new Map<string, DispatchedSession>();
    for (const session of sessions) {
      const cardId = session.kanban_card_id ?? null;
      if (!cardId) continue;
      const existing = map.get(cardId);
      if (!existing || sessionSortValue(session) > sessionSortValue(existing)) {
        map.set(cardId, session);
      }
    }
    return map;
  }, [sessions]);

  const department = departments.find((item) => item.id === agent.department_id) ?? null;
  const providerMeta = getProviderMeta(agent.cli_provider);
  const level = getAgentLevel(agent.stats_xp);
  const levelTitle = getAgentTitle(agent.stats_xp, isKo);
  const currentTaskStartedAt = useMemo(
    () => normalizeTimestampMs(currentCard?.started_at ?? currentCard?.requested_at ?? null),
    [currentCard],
  );
  const currentTaskIssueUrl = currentCard ? buildCardIssueUrl(currentCard) : null;

  return (
    <Drawer
      open={open}
      onClose={onClose}
      title={localeName(locale, agent)}
      width="min(440px, 100vw)"
      ariaLabel={`${localeName(locale, agent)} ${t(isKo, "오피스 상세", "Office details")}`}
    >
      <div className="space-y-4" data-testid="office-agent-drawer">
        <SurfaceSection
          eyebrow={t(isKo, "오피스 에이전트", "Office agent")}
          title={localeName(locale, agent)}
          description={agent.alias ? `aka ${agent.alias}` : undefined}
          actions={(
            <div className="flex flex-wrap items-center gap-2">
              <span
                className="rounded-full px-2.5 py-1 text-xs font-semibold"
                style={{
                  background: providerMeta.bg,
                  color: providerMeta.color,
                  border: `1px solid ${providerMeta.border}`,
                }}
              >
                {providerMeta.label}
              </span>
              {manualIntervention && (
                <span
                  className="rounded-full px-2.5 py-1 text-xs font-semibold"
                  style={{
                    background: "color-mix(in srgb, var(--warn) 14%, var(--th-card-bg) 86%)",
                    color: "var(--warn)",
                    border: "1px solid color-mix(in srgb, var(--warn) 24%, var(--th-border) 76%)",
                  }}
                >
                  {t(isKo, "수동 개입", "Manual intervention")}
                </span>
              )}
            </div>
          )}
        >
          <div className="mt-4 flex flex-col gap-3 sm:flex-row sm:items-center">
            <AgentAvatar agent={agent} spriteMap={spriteMap} size={56} rounded="2xl" />
            {agent.session_info && (
              <div
                className="min-w-0 flex-1 text-sm leading-6"
                style={{ color: "var(--th-text-muted)" }}
              >
                {agent.session_info}
              </div>
            )}
          </div>
          <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-2">
            <SurfaceMetricPill
              label={t(isKo, "레벨", "Level")}
              tone="accent"
              value={`Lv.${level.level} ${levelTitle}`}
            />
            <SurfaceMetricPill
              label="XP"
              tone="info"
              value={agent.stats_xp.toLocaleString(locale)}
            />
            <SurfaceMetricPill
              label={t(isKo, "완료", "Done")}
              tone="success"
              value={`${agent.stats_tasks_done}`}
            />
            <SurfaceMetricPill
              label={t(isKo, "상태", "Status")}
              tone="neutral"
              value={t(
                isKo,
                agent.status === "working"
                  ? "작업 중"
                  : agent.status === "offline"
                    ? "오프라인"
                    : agent.status === "break"
                      ? "휴식"
                      : "대기",
                agent.status === "working"
                  ? "Working"
                  : agent.status === "offline"
                    ? "Offline"
                    : agent.status === "break"
                      ? "Break"
                      : "Idle",
              )}
            />
            <SurfaceMetricPill
              label={t(isKo, "부서", "Department")}
              tone="neutral"
              value={
                department
                  ? `${department.icon} ${localeName(locale, department)}`
                  : t(isKo, "미배정", "Unassigned")
              }
            />
          </div>
        </SurfaceSection>

        <SurfaceSubsection
          title={t(isKo, "현재 작업", "Current task")}
          description={t(isKo, "칸반 카드와 수동 개입 상태를 함께 보여줍니다.", "Shows the active card and any manual intervention state together.")}
        >
          {currentCard ? (
            <div className="space-y-3">
              <SurfaceCard className="p-3">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {currentCard.title}
                    </div>
                    <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {t(isKo, "상태", "Status")}: {currentCard.status}
                      {currentTaskStartedAt
                        ? ` · ${formatElapsed(currentTaskStartedAt, isKo) ?? formatDateTime(currentTaskStartedAt, locale)}`
                        : ""}
                    </div>
                  </div>
                  {currentCard.github_issue_number && currentTaskIssueUrl ? (
                    <a
                      href={currentTaskIssueUrl}
                      target="_blank"
                      rel="noreferrer"
                      className="rounded-full border px-2.5 py-1 text-xs font-semibold transition hover:opacity-90"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-accent-info) 30%, var(--th-border) 70%)",
                        color: "var(--th-accent-info)",
                      }}
                    >
                      #{currentCard.github_issue_number}
                    </a>
                  ) : null}
                </div>
              </SurfaceCard>
              {manualIntervention && (
                <SurfaceCard
                  className="p-3"
                  style={{
                    borderColor: "color-mix(in srgb, var(--warn) 28%, var(--th-border) 72%)",
                    background: "color-mix(in srgb, var(--warn) 10%, var(--th-card-bg) 90%)",
                  }}
                >
                  <div className="text-xs font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--warn)" }}>
                    {t(isKo, "경고 사유", "Warning")}
                  </div>
                  <div className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
                    {manualIntervention.reason
                      ?? t(
                        isKo,
                        "수동 개입 상태지만 구체 사유는 아직 기록되지 않았습니다.",
                        "Manual intervention is active, but no explicit reason was recorded yet.",
                      )}
                  </div>
                </SurfaceCard>
              )}
            </div>
          ) : (
            <SurfaceEmptyState className="text-sm">
              {t(isKo, "열린 작업 카드가 없습니다.", "No active card is assigned right now.")}
            </SurfaceEmptyState>
          )}
        </SurfaceSubsection>

        <SurfaceSubsection
          title={t(isKo, "최근 7일 Top 스킬", "Top skills in the last 7 days")}
          description={t(isKo, "에이전트별 스킬 호출 집계 기준입니다.", "Based on per-agent skill usage aggregation.")}
        >
          {loading ? (
            <SurfaceNotice compact>{t(isKo, "스킬 사용량을 불러오는 중...", "Loading skill usage...")}</SurfaceNotice>
          ) : skillRows.length === 0 ? (
            <SurfaceEmptyState className="text-sm">
              {t(isKo, "최근 7일 스킬 호출이 없습니다.", "No skill calls in the last 7 days.")}
            </SurfaceEmptyState>
          ) : (
            <div className="space-y-2">
              {skillRows.map((row) => (
                <SurfaceCard key={row.skillName} className="flex items-start justify-between gap-3 p-3">
                  <div className="min-w-0">
                    <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.skillName}
                    </div>
                    <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {row.description || t(isKo, "설명 없음", "No description")}
                    </div>
                  </div>
                  <div className="shrink-0 text-right">
                    <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.calls}
                    </div>
                    <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {formatDateTime(row.lastUsedAt, locale)}
                    </div>
                  </div>
                </SurfaceCard>
              ))}
            </div>
          )}
        </SurfaceSubsection>

        <SurfaceSubsection
          title={t(isKo, "세션 링크", "Session links")}
          description={t(isKo, "현재 연결된 AgentDesk 세션과 Discord thread 링크입니다.", "Linked AgentDesk sessions and Discord thread links.")}
        >
          {loading ? (
            <SurfaceNotice compact>{t(isKo, "세션 정보를 불러오는 중...", "Loading sessions...")}</SurfaceNotice>
          ) : sessions.length === 0 ? (
            <SurfaceEmptyState className="text-sm">
              {t(isKo, "연결된 세션이 없습니다.", "No linked sessions.")}
            </SurfaceEmptyState>
          ) : (
            <div className="space-y-2">
              {sessions.map((session) => {
                const provider = getProviderMeta(session.provider);
                const channelInfo = session.thread_channel_id
                  ? sessionChannelsById[session.thread_channel_id] ?? null
                  : null;
                const parentInfo = channelInfo?.parent_id
                  ? sessionChannelsById[channelInfo.parent_id] ?? null
                  : null;
                const summary = describeDispatchedSession(session, channelInfo, parentInfo);
                return (
                  <SurfaceCard key={session.id} className="p-3">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                          {formatDiscordSummary(summary)}
                        </div>
                        <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                          {session.session_info || session.model || t(isKo, "세션 정보 없음", "No session detail")}
                        </div>
                      </div>
                      <div className="flex shrink-0 flex-wrap gap-2">
                        <span
                          className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                          style={{
                            background: provider.bg,
                            color: provider.color,
                            border: `1px solid ${provider.border}`,
                          }}
                        >
                          {provider.label}
                        </span>
                        <span
                          className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                          style={{
                            background: session.status === "working"
                              ? "color-mix(in srgb, var(--ok) 14%, var(--th-card-bg) 86%)"
                              : "color-mix(in srgb, var(--fg-muted) 14%, var(--th-card-bg) 86%)",
                            color: session.status === "working" ? "var(--ok)" : "var(--fg-muted)",
                          }}
                        >
                          {session.status === "working" ? t(isKo, "작업중", "Working") : t(isKo, "대기", "Idle")}
                        </span>
                      </div>
                    </div>
                    {(summary.webUrl || summary.deepLink) && (
                      <div className="mt-3 flex flex-wrap gap-2">
                        {summary.webUrl && (
                          <a href={summary.webUrl} target="_blank" rel="noreferrer">
                            <SurfaceActionButton tone="info" compact>
                              {t(isKo, "웹에서 열기", "Open web")}
                            </SurfaceActionButton>
                          </a>
                        )}
                        {summary.deepLink && (
                          <a href={summary.deepLink}>
                            <SurfaceActionButton tone="neutral" compact>
                              {t(isKo, "Discord 앱", "Discord app")}
                            </SurfaceActionButton>
                          </a>
                        )}
                      </div>
                    )}
                  </SurfaceCard>
                );
              })}
            </div>
          )}
        </SurfaceSubsection>

        {/* #1258 follow-up: restored "감사 / Audit" panel. The previous
            OfficeInsightPanel rendered raw `kanban_card:UUID` strings and was
            deleted as "no signal value". The backend now LEFT JOINs
            kanban_cards on the audit_logs query so each row carries the
            human-readable card title + GitHub issue number. Discord deeplinks
            for each row are resolved client-side via the dispatched-sessions
            data already loaded above (sessionByCardId map), so no extra
            round-trip is needed. */}
        <SurfaceSubsection
          title={t(isKo, "감사", "Audit")}
          description={t(
            isKo,
            "이 에이전트가 담당한 카드의 최근 상태 전환과 hook 실행 이력입니다.",
            "Recent state transitions and hook fires for cards assigned to this agent.",
          )}
        >
          {loading ? (
            <SurfaceNotice compact>
              {t(isKo, "감사 로그를 불러오는 중...", "Loading audit log...")}
            </SurfaceNotice>
          ) : auditLogs.length === 0 ? (
            <SurfaceEmptyState className="text-sm">
              {t(isKo, "기록된 감사 항목이 없습니다.", "No audit entries recorded yet.")}
            </SurfaceEmptyState>
          ) : (
            <div className="space-y-2">
              {auditLogs.map((entry) => {
                const session = entry.entity_id
                  ? sessionByCardId.get(entry.entity_id) ?? null
                  : null;
                const titleParts: string[] = [];
                if (typeof entry.card_issue_number === "number") {
                  titleParts.push(`#${entry.card_issue_number}`);
                }
                if (entry.card_title) titleParts.push(entry.card_title);
                const headline = titleParts.length > 0
                  ? titleParts.join(" ")
                  : entry.summary || entry.entity_id || t(isKo, "(라벨 없음)", "(no label)");
                const actorLabel = entry.actor || t(isKo, "시스템", "system");
                return (
                  <SurfaceCard key={entry.id} className="p-3">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div
                          className="truncate text-sm font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                          title={entry.summary}
                        >
                          {headline}
                        </div>
                        <div
                          className="mt-1 text-xs leading-5"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          <span>{entry.action}</span>
                          <span className="mx-1.5">·</span>
                          <span>{actorLabel}</span>
                          <span className="mx-1.5">·</span>
                          <span>{formatDateTime(entry.created_at, locale)}</span>
                        </div>
                      </div>
                    </div>
                    {(entry.card_issue_url || session?.channel_web_url || session?.channel_deeplink_url) && (
                      <div className="mt-3 flex flex-wrap gap-2">
                        {entry.card_issue_url && (
                          <a href={entry.card_issue_url} target="_blank" rel="noreferrer">
                            <SurfaceActionButton tone="neutral" compact>
                              {t(isKo, "GitHub 이슈", "GitHub issue")}
                            </SurfaceActionButton>
                          </a>
                        )}
                        {session?.channel_web_url && (
                          <a href={session.channel_web_url} target="_blank" rel="noreferrer">
                            <SurfaceActionButton tone="info" compact>
                              {t(isKo, "Discord 웹", "Discord web")}
                            </SurfaceActionButton>
                          </a>
                        )}
                        {session?.channel_deeplink_url && (
                          <a href={session.channel_deeplink_url}>
                            <SurfaceActionButton tone="neutral" compact>
                              {t(isKo, "Discord 앱", "Discord app")}
                            </SurfaceActionButton>
                          </a>
                        )}
                      </div>
                    )}
                  </SurfaceCard>
                );
              })}
            </div>
          )}
        </SurfaceSubsection>
      </div>
    </Drawer>
  );
}
