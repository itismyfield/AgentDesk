import { useEffect, useMemo, useState } from "react";
import {
  getAgentDispatchedSessions,
  getAgentTranscripts,
  getDiscordChannelInfo,
  getSkillRanking,
  type DiscordChannelInfo,
  type SessionTranscript,
  type SkillRankingByAgentRow,
} from "../../api";
import { getProviderMeta } from "../../app/providerTheme";
import { localeName } from "../../i18n";
import type { Agent, Department, DispatchedSession, KanbanCard, UiLanguage } from "../../types";
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
import { getAgentLevel, getAgentTitle } from "../agent-manager/AgentInfoCard";
import type { OfficeManualIntervention } from "./officeAgentState";

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

interface OfficeSkillRow {
  skillName: string;
  description: string | null;
  calls: number;
  lastUsedAt: number | null;
}

const SKILL_MARKDOWN_RE = /([A-Za-z0-9][A-Za-z0-9._-]*)\/SKILL\.md/g;
const SKILL_WINDOW_MS = 7 * 24 * 60 * 60 * 1000;

function t(isKo: boolean, ko: string, en: string): string {
  return isKo ? ko : en;
}

function formatDateTime(value: number | null, locale: UiLanguage): string {
  if (!value) return "-";
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(value);
}

function formatElapsed(value: number | null, isKo: boolean): string | null {
  if (!value) return null;
  const diff = Math.max(0, Date.now() - value);
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return t(isKo, "방금 시작", "Started just now");
  if (mins < 60) return t(isKo, `${mins}분째`, `${mins}m running`);
  const hours = Math.floor(mins / 60);
  const remainMins = mins % 60;
  if (hours < 24) {
    return remainMins > 0
      ? t(isKo, `${hours}시간 ${remainMins}분째`, `${hours}h ${remainMins}m running`)
      : t(isKo, `${hours}시간째`, `${hours}h running`);
  }
  const days = Math.floor(hours / 24);
  return t(isKo, `${days}일째`, `${days}d running`);
}

function normalizeTimestampMs(value: number | string | null | undefined): number | null {
  if (value == null || value === "") return null;
  if (typeof value === "number") {
    return value < 1e12 ? value * 1000 : value;
  }
  const numeric = Number(value);
  if (Number.isFinite(numeric)) {
    return numeric < 1e12 ? numeric * 1000 : numeric;
  }
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

function extractSkillNameFromEventContent(content: string): string | null {
  try {
    const parsed = JSON.parse(content) as Record<string, unknown>;
    for (const key of ["skill", "name", "command"]) {
      const value = parsed[key];
      if (typeof value === "string" && value.trim()) {
        return value.trim().replace(/^\/+/, "");
      }
    }
  } catch {
    // Ignore malformed payloads and fall back to regex scanning.
  }
  return null;
}

function deriveFallbackSkillRows(transcripts: SessionTranscript[]): OfficeSkillRow[] {
  const cutoff = Date.now() - SKILL_WINDOW_MS;
  const aggregates = new Map<string, OfficeSkillRow>();

  for (const transcript of transcripts) {
    const usedAt = normalizeTimestampMs(transcript.created_at);
    if (!usedAt || usedAt < cutoff) continue;

    const used = new Set<string>();
    const searchableChunks = [transcript.assistant_message];

    for (const event of transcript.events) {
      searchableChunks.push(event.summary ?? "");
      searchableChunks.push(event.content);
      if (
        event.kind === "tool_use"
        && event.tool_name?.toLowerCase() === "skill"
      ) {
        const skillName = extractSkillNameFromEventContent(event.content);
        if (skillName) used.add(skillName);
      }
    }

    const searchable = searchableChunks.join("\n");
    for (const match of searchable.matchAll(SKILL_MARKDOWN_RE)) {
      if (match[1]) used.add(match[1]);
    }

    for (const skillName of used) {
      const current = aggregates.get(skillName);
      if (current) {
        current.calls += 1;
        current.lastUsedAt = current.lastUsedAt == null ? usedAt : Math.max(current.lastUsedAt, usedAt);
        continue;
      }
      aggregates.set(skillName, {
        skillName,
        description: null,
        calls: 1,
        lastUsedAt: usedAt,
      });
    }
  }

  return Array.from(aggregates.values())
    .sort((left, right) => {
      if (right.calls !== left.calls) return right.calls - left.calls;
      return (right.lastUsedAt ?? 0) - (left.lastUsedAt ?? 0);
    })
    .slice(0, 5);
}

function buildSkillLookupKeys(agent: Agent): Set<string> {
  return new Set(
    [
      agent.id,
      agent.role_id,
      agent.name,
      agent.name_ko,
      agent.alias,
    ]
      .filter((value): value is string => Boolean(value?.trim()))
      .map((value) => value.trim().toLowerCase()),
  );
}

function buildSkillRows(agent: Agent, rankingRows: SkillRankingByAgentRow[], transcripts: SessionTranscript[]): OfficeSkillRow[] {
  const lookupKeys = buildSkillLookupKeys(agent);
  const agentRows = rankingRows
    .filter((row) => {
      const roleId = row.agent_role_id.trim().toLowerCase();
      const agentName = row.agent_name.trim().toLowerCase();
      return lookupKeys.has(roleId) || lookupKeys.has(agentName);
    })
    .slice(0, 5)
    .map((row) => ({
      skillName: row.skill_name,
      description: row.skill_desc_ko || null,
      calls: row.calls,
      lastUsedAt: row.last_used_at ?? null,
    }));

  if (agentRows.length > 0) return agentRows;
  return deriveFallbackSkillRows(transcripts);
}

function hydrateSession(raw: DispatchedSession, agent: Agent): DispatchedSession {
  return {
    ...raw,
    session_key: raw.session_key ?? `agent:${agent.id}`,
    name: raw.name ?? agent.alias ?? agent.name_ko ?? agent.name,
    department_id: raw.department_id ?? agent.department_id ?? null,
    linked_agent_id: raw.linked_agent_id ?? agent.id,
    provider: raw.provider ?? agent.cli_provider ?? "claude",
    model: raw.model ?? null,
    status: raw.status ?? (agent.status === "working" ? "working" : "idle"),
    session_info: raw.session_info ?? agent.session_info ?? null,
    sprite_number: raw.sprite_number ?? agent.sprite_number ?? null,
    avatar_emoji: raw.avatar_emoji ?? agent.avatar_emoji,
    stats_xp: raw.stats_xp ?? agent.stats_xp,
    tokens: raw.tokens ?? 0,
    connected_at: raw.connected_at ?? agent.created_at,
    last_seen_at: raw.last_seen_at ?? null,
    department_name: raw.department_name ?? agent.department_name ?? null,
    department_name_ko: raw.department_name_ko ?? agent.department_name_ko ?? null,
    department_color: raw.department_color ?? agent.department_color ?? null,
    thread_channel_id: raw.thread_channel_id ?? agent.current_thread_channel_id ?? null,
  };
}

function ensurePrimarySession(agent: Agent, sessions: DispatchedSession[]): DispatchedSession[] {
  if (!agent.current_thread_channel_id) return sessions;
  if (sessions.some((session) => session.thread_channel_id === agent.current_thread_channel_id)) {
    return sessions;
  }
  return [
    {
      id: `office-primary:${agent.id}`,
      session_key: `office:${agent.id}`,
      name: agent.alias ?? agent.name_ko ?? agent.name,
      department_id: agent.department_id ?? null,
      linked_agent_id: agent.id,
      provider: agent.cli_provider ?? "claude",
      model: null,
      status: agent.status === "working" ? "working" : "idle",
      session_info: agent.session_info ?? null,
      sprite_number: agent.sprite_number ?? null,
      avatar_emoji: agent.avatar_emoji,
      stats_xp: agent.stats_xp,
      tokens: 0,
      connected_at: agent.created_at,
      last_seen_at: null,
      department_name: agent.department_name ?? null,
      department_name_ko: agent.department_name_ko ?? null,
      department_color: agent.department_color ?? null,
      thread_channel_id: agent.current_thread_channel_id,
    },
    ...sessions,
  ];
}

function sessionSortValue(session: DispatchedSession): number {
  return session.last_seen_at ?? session.connected_at ?? 0;
}

function buildCardIssueUrl(card: KanbanCard): string | null {
  if (card.github_issue_url) return card.github_issue_url;
  if (card.github_repo && card.github_issue_number) {
    return `https://github.com/${card.github_repo}/issues/${card.github_issue_number}`;
  }
  return null;
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

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setLoading(true);

    void (async () => {
      const [sessionsResult, rankingResult, transcriptsResult] = await Promise.allSettled([
        getAgentDispatchedSessions(agent.id),
        getSkillRanking("7d", 100),
        getAgentTranscripts(agent.id, 80),
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
      setLoading(false);
    })();

    return () => {
      cancelled = true;
    };
  }, [agent, open]);

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
          <div className="mt-4 flex items-start gap-3">
            <AgentAvatar agent={agent} spriteMap={spriteMap} size={56} rounded="2xl" />
            <div className="min-w-0 flex-1 space-y-3">
              <div className="flex flex-wrap gap-2">
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
              </div>
              <div className="grid gap-2 sm:grid-cols-2">
                <SummaryRow
                  label={t(isKo, "상태", "Status")}
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
                <SummaryRow
                  label={t(isKo, "부서", "Department")}
                  value={department ? `${department.icon} ${localeName(locale, department)}` : t(isKo, "미배정", "Unassigned")}
                />
              </div>
              {agent.session_info && (
                <div className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                  {agent.session_info}
                </div>
              )}
            </div>
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
      </div>
    </Drawer>
  );
}

function SummaryRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-2xl border px-3 py-2" style={{ borderColor: "color-mix(in srgb, var(--th-border) 66%, transparent)" }}>
      <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-1 text-sm" style={{ color: "var(--th-text-primary)" }}>
        {value}
      </div>
    </div>
  );
}
