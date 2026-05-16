import { useMemo } from "react";
import type { Agent, CompanySettings, DashboardStats, DispatchedSession } from "../../types";
import { timeAgo, type TFunction } from "./model";
import AgentAvatar from "../AgentAvatar";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";

interface HomeSignalRow {
  id: string;
  label: string;
  value: number;
  description: string;
  accent: string;
  tone: HomeSignalTone;
  onAction?: () => void;
}

interface HomeActivityItem {
  id: string;
  title: string;
  detail: string;
  timestamp: number;
  tone: "success" | "warn";
}

interface HomeAgentRow {
  agent: Agent;
  displayName: string;
  workSummary: string | null;
  elapsedLabel: string | null;
  linkedSessions: DispatchedSession[];
}

type HomeSignalTone = "info" | "warn" | "danger" | "success";

export function DashboardHomeMetricTile({
  title,
  value,
  badge,
  sub,
  accent,
  spark,
}: {
  title: string;
  value: string;
  badge: string;
  sub: string;
  accent: string;
  spark: number[];
}) {
  const maxValue = Math.max(1, ...spark);

  return (
    <SurfaceCard
      className="h-full rounded-[28px] p-4 sm:p-5"
      style={{
        borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
        background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, ${accent} 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)`,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div
            className="text-[11px] font-semibold uppercase tracking-[0.16em]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {title}
          </div>
          <div className="mt-3 text-[2rem] font-black leading-none tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {value}
          </div>
          <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {sub}
          </p>
        </div>
        <SurfaceMetaBadge
          tone="neutral"
          style={{
            borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
            background: `color-mix(in srgb, ${accent} 14%, var(--th-card-bg) 86%)`,
            color: accent,
          }}
        >
          {badge}
        </SurfaceMetaBadge>
      </div>

      <div className="mt-4 flex h-10 items-end gap-1">
        {spark.map((point, index) => (
          <div
            key={`${title}-${index}`}
            className="min-w-0 flex-1 rounded-full"
            style={{
              height: `${Math.max(18, (point / maxValue) * 100)}%`,
              background: `linear-gradient(180deg, color-mix(in srgb, ${accent} 78%, white 22%) 0%, ${accent} 100%)`,
              opacity: index === spark.length - 1 ? 1 : 0.72,
            }}
          />
        ))}
      </div>
    </SurfaceCard>
  );
}

export function DashboardHomeOfficeWidget({
  rows,
  stats,
  language,
  t,
  onSelectAgent,
}: {
  rows: HomeAgentRow[];
  stats: DashboardStats;
  language: CompanySettings["language"];
  t: TFunction;
  onSelectAgent?: (agent: Agent) => void;
}) {
  void language;
  const visibleRows = rows.slice(0, 8);

  return (
    <SurfaceSubsection
      title={t({ ko: "오피스 뷰", en: "Office View", ja: "オフィスビュー", zh: "办公室视图" })}
      description={t({
        ko: "지금 일하는 에이전트와 세션 상태를 한 화면에 압축해 보여줍니다.",
        en: "A compressed office snapshot of active agents and live sessions.",
        ja: "作業中エージェントとセッション状態を圧縮して見せます。",
        zh: "压缩展示当前工作中的代理与会话状态。",
      })}
      style={{
        minHeight: 320,
        borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
      actions={(
        <div className="flex flex-wrap gap-2">
          <SurfaceMetaBadge tone="success">
            {t({ ko: `${stats.agents.working} working`, en: `${stats.agents.working} working`, ja: `${stats.agents.working} working`, zh: `${stats.agents.working} working` })}
          </SurfaceMetaBadge>
          <SurfaceMetaBadge tone="neutral">
            {t({ ko: `${stats.agents.idle} idle`, en: `${stats.agents.idle} idle`, ja: `${stats.agents.idle} idle`, zh: `${stats.agents.idle} idle` })}
          </SurfaceMetaBadge>
        </div>
      )}
    >
      {visibleRows.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "표시할 에이전트가 없습니다.", en: "No agents available.", ja: "表示するエージェントがいません。", zh: "没有可显示的代理。" })}
        </SurfaceEmptyState>
      ) : (
        <>
          <div
            className="rounded-[24px] border p-4"
            style={{
              borderColor: "rgba(148,163,184,0.16)",
              background:
                "radial-gradient(circle at top, color-mix(in srgb, var(--th-accent-info) 12%, transparent), transparent 52%), linear-gradient(180deg, color-mix(in srgb, var(--th-bg-surface) 94%, transparent), color-mix(in srgb, var(--th-card-bg) 90%, transparent))",
            }}
          >
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              {visibleRows.map((row) => {
                const statusTone = getAgentStatusTone(row.agent.status);
                const accent =
                  statusTone === "success"
                    ? "var(--th-accent-success)"
                    : statusTone === "warn"
                      ? "var(--th-accent-warn)"
                      : statusTone === "danger"
                        ? "var(--th-accent-danger)"
                        : "var(--th-text-muted)";
                return (
                  <button
                    key={row.agent.id}
                    type="button"
                    onClick={onSelectAgent ? () => onSelectAgent(row.agent) : undefined}
                    className="rounded-2xl border p-3 text-left transition-transform hover:-translate-y-0.5"
                    style={{
                      borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
                      background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                    }}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="relative">
                        <AgentAvatar agent={row.agent} size={44} />
                        <span
                          className="absolute -right-0.5 -top-0.5 inline-flex h-3 w-3 rounded-full border-2"
                          style={{
                            borderColor: "var(--th-card-bg)",
                            background: accent,
                            boxShadow: `0 0 0 3px color-mix(in srgb, ${accent} 16%, transparent)`,
                          }}
                        />
                      </div>
                      <SurfaceMetaBadge tone={statusTone}>
                        {getAgentStatusLabel(row.agent.status, t)}
                      </SurfaceMetaBadge>
                    </div>
                    <div className="mt-3 truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.displayName}
                    </div>
                    <div className="mt-1 min-h-[2.5rem] text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {row.workSummary ?? t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "待机中" })}
                    </div>
                    <div className="mt-2 flex items-center justify-between text-[10px]" style={{ color: "var(--th-text-muted)" }}>
                      <span style={{ fontFamily: "var(--font-mono)" }}>
                        {row.elapsedLabel ?? "--"}
                      </span>
                      <span style={{ fontFamily: "var(--font-mono)" }}>
                        {row.linkedSessions.length} session
                      </span>
                    </div>
                  </button>
                );
              })}
            </div>
          </div>

          <div className="mt-4 flex flex-wrap gap-2">
            <SurfaceMetaBadge tone="success">
              {t({ ko: `${stats.agents.working} working`, en: `${stats.agents.working} working`, ja: `${stats.agents.working} working`, zh: `${stats.agents.working} working` })}
            </SurfaceMetaBadge>
            <SurfaceMetaBadge tone="neutral">
              {t({ ko: `${stats.agents.idle} idle`, en: `${stats.agents.idle} idle`, ja: `${stats.agents.idle} idle`, zh: `${stats.agents.idle} idle` })}
            </SurfaceMetaBadge>
            <SurfaceMetaBadge tone="warn">
              {t({ ko: `${stats.dispatched_count} dispatched`, en: `${stats.dispatched_count} dispatched`, ja: `${stats.dispatched_count} dispatched`, zh: `${stats.dispatched_count} dispatched` })}
            </SurfaceMetaBadge>
          </div>
        </>
      )}
    </SurfaceSubsection>
  );
}

export function DashboardHomeSignalsWidget({
  rows,
  maxValue,
  t,
}: {
  rows: HomeSignalRow[];
  maxValue: number;
  t: TFunction;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "운영 미션", en: "Ops Missions", ja: "運用ミッション", zh: "运营任务" })}
      description={t({
        ko: "지금 바로 처리할 운영 압력을 우선순위 카드로 정리했습니다.",
        en: "Priority cards for the operational pressure points that need action now.",
        ja: "今すぐ処理すべき運用圧力を優先カードで整理しました。",
        zh: "将需要立即处理的运营压力整理成优先级卡片。",
      })}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="mb-4 flex items-center justify-between gap-2">
        <SurfaceMetaBadge tone="neutral">
          {t({
            ko: `${rows.length}개 트래킹`,
            en: `${rows.length} tracked`,
            ja: `${rows.length}件を追跡中`,
            zh: `跟踪 ${rows.length} 项`,
          })}
        </SurfaceMetaBadge>
        <span
          className="text-[11px]"
          style={{
            color: "var(--th-text-muted)",
            fontFamily: "var(--font-mono)",
          }}
        >
          {t({ ko: "priority live", en: "priority live", ja: "priority live", zh: "priority live" })}
        </span>
      </div>

      <div className="space-y-2.5">
        {rows.map((row) => {
          const accent = getSignalAccent(row.tone);
          const tone = row.tone === "info" ? "info" : row.tone;
          const ratio = Math.max(0, Math.min(100, (row.value / maxValue) * 100));
          const body = (
            <div
              className="rounded-[22px] border p-4 text-left transition-transform duration-150"
              style={{
                borderColor: `color-mix(in srgb, ${accent} 24%, var(--th-border) 76%)`,
                background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 93%, ${accent} 7%) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)`,
              }}
            >
              <div className="flex items-start gap-3">
                <div
                  className="mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-lg border text-[11px] font-semibold"
                  style={{
                    borderColor: `color-mix(in srgb, ${accent} 26%, var(--th-border) 74%)`,
                    background: `color-mix(in srgb, ${accent} 12%, var(--th-card-bg) 88%)`,
                    color: accent,
                  }}
                >
                  {row.value > 0 ? "!" : "·"}
                </div>

                <div className="min-w-0 flex-1">
                  <div className="text-[10.5px] font-semibold uppercase tracking-[0.16em]" style={{ color: accent }}>
                    {row.label}
                  </div>
                  <div className="mt-2 flex items-end justify-between gap-3">
                    <div className="text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
                      {row.value}
                    </div>
                    <SurfaceMetaBadge tone={tone}>{row.description}</SurfaceMetaBadge>
                  </div>
                  <div className="mt-3 flex items-center justify-between gap-3 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    <span>
                      {t({
                        ko: row.value > 0 ? "지금 확인 필요" : "현재 추가 조치 없음",
                        en: row.value > 0 ? "Needs attention now" : "No extra action right now",
                        ja: row.value > 0 ? "今すぐ確認が必要" : "追加アクションなし",
                        zh: row.value > 0 ? "需要立即确认" : "当前无需额外处理",
                      })}
                    </span>
                    <span style={{ fontFamily: "var(--font-mono)" }}>
                      {t({ ko: "압력", en: "pressure", ja: "pressure", zh: "pressure" })} {Math.round(ratio)}%
                    </span>
                  </div>

                  <div className="mt-3 flex items-center gap-3">
                    <div className="h-[5px] flex-1 overflow-hidden rounded-full" style={{ background: "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)" }}>
                      <div
                        className="h-full rounded-full"
                        style={{
                          width: `${Math.max(8, ratio)}%`,
                          background: `linear-gradient(90deg, color-mix(in srgb, ${accent} 68%, white 32%), ${accent})`,
                        }}
                      />
                    </div>
                    <span className="text-[11px] font-medium" style={{ color: accent }}>
                      {row.onAction
                        ? t({ ko: "열기", en: "Open", ja: "開く", zh: "打开" })
                        : t({ ko: "모니터링", en: "Monitoring", ja: "監視中", zh: "监控中" })}
                    </span>
                  </div>
                </div>
              </div>
            </div>
          );

          return row.onAction ? (
            <button
              key={row.id}
              type="button"
              onClick={row.onAction}
              className="block w-full rounded-2xl text-left transition-transform hover:-translate-y-0.5"
            >
              {body}
            </button>
          ) : (
            <div key={row.id}>{body}</div>
          );
        })}
      </div>
    </SurfaceSubsection>
  );
}

export function DashboardHomeRosterWidget({
  rows,
  t,
  numberFormatter,
  onSelectAgent,
  onOpenAchievements,
}: {
  rows: HomeAgentRow[];
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  onSelectAgent?: (agent: Agent) => void;
  onOpenAchievements?: () => void;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "에이전트 현황", en: "Agent Roster", ja: "エージェント現況", zh: "代理现况" })}
      description={t({
        ko: "활성 우선으로 상위 에이전트 상태를 요약합니다.",
        en: "A live-first roster summary of the top agents.",
        ja: "アクティブ優先で上位エージェントの状態を要約します。",
        zh: "按活跃优先总结头部代理状态。",
      })}
      actions={onOpenAchievements ? (
        <SurfaceActionButton tone="accent" onClick={onOpenAchievements}>
          {t({ ko: "업적 보기", en: "Open XP", ja: "XP を開く", zh: "查看 XP" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-success) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-success) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {rows.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "표시할 에이전트가 없습니다.", en: "No agents to show.", ja: "表示するエージェントがいません。", zh: "没有可显示的代理。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="space-y-2">
          {rows.map((row) => (
            <SurfaceListItem
              key={row.agent.id}
              tone={getAgentStatusTone(row.agent.status)}
              trailing={(
                <div className="flex items-center gap-2">
                  <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    <div style={{ color: "var(--th-text-heading)" }}>
                      XP {numberFormatter.format(row.agent.stats_xp)}
                    </div>
                    <div>{numberFormatter.format(row.agent.stats_tasks_done)} done</div>
                  </div>
                  {onSelectAgent ? (
                    <SurfaceActionButton compact tone="neutral" onClick={() => onSelectAgent(row.agent)}>
                      {t({ ko: "열기", en: "Open", ja: "開く", zh: "打开" })}
                    </SurfaceActionButton>
                  ) : null}
                </div>
              )}
            >
              <div className="flex items-start gap-3">
                <AgentAvatar agent={row.agent} size={34} />
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.displayName}
                    </span>
                    <SurfaceMetaBadge tone={getAgentStatusTone(row.agent.status)}>
                      {getAgentStatusLabel(row.agent.status, t)}
                    </SurfaceMetaBadge>
                  </div>
                  <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {row.workSummary ?? t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "待机中" })}
                  </div>
                  <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                    {row.elapsedLabel ? <SurfaceMetaBadge>{row.elapsedLabel}</SurfaceMetaBadge> : null}
                    <SurfaceMetaBadge>{row.linkedSessions.length} session</SurfaceMetaBadge>
                  </div>
                </div>
              </div>
            </SurfaceListItem>
          ))}
        </div>
      )}
    </SurfaceSubsection>
  );
}

export function DashboardHomeActivityWidget({
  items,
  localeTag,
  t,
  onOpenMeetings,
}: {
  items: HomeActivityItem[];
  localeTag: string;
  t: TFunction;
  onOpenMeetings?: () => void;
}) {
  const formatter = useMemo(
    () =>
      new Intl.DateTimeFormat(localeTag, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      }),
    [localeTag],
  );

  return (
    <SurfaceSubsection
      title={t({ ko: "최근 활동", en: "Recent Activity", ja: "最近の活動", zh: "最近活动" })}
      description={t({
        ko: "회의와 세션 전환을 시간순으로 압축해 보여줍니다.",
        en: "A compressed activity stream across meetings and sessions.",
        ja: "会議とセッション遷移を時間順で圧縮表示します。",
        zh: "按时间顺序压缩展示会议与会话活动。",
      })}
      actions={onOpenMeetings ? (
        <SurfaceActionButton tone="neutral" onClick={onOpenMeetings}>
          {t({ ko: "회의 보기", en: "Open Meetings", ja: "会議を開く", zh: "打开会议" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-warn) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {items.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "최근 활동이 없습니다.", en: "No recent activity.", ja: "最近の活動はありません。", zh: "暂无最近活动。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="space-y-2">
          {items.map((item) => (
            <SurfaceListItem
              key={item.id}
              tone={item.tone === "success" ? "success" : "warn"}
              trailing={(
                <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  <div>{timeAgo(item.timestamp, localeTag)}</div>
                  <div style={{ fontFamily: "var(--font-mono)" }}>{formatter.format(item.timestamp)}</div>
                </div>
              )}
            >
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {item.title}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {item.detail}
                </div>
              </div>
            </SurfaceListItem>
          ))}
        </div>
      )}
    </SurfaceSubsection>
  );
}

function getAgentStatusTone(status: Agent["status"]): "neutral" | "success" | "warn" | "danger" {
  switch (status) {
    case "working":
      return "success";
    case "break":
      return "warn";
    case "offline":
      return "danger";
    case "idle":
    default:
      return "neutral";
  }
}

function getAgentStatusLabel(status: Agent["status"], t: TFunction): string {
  switch (status) {
    case "working":
      return t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" });
    case "break":
      return t({ ko: "휴식", en: "Break", ja: "休憩", zh: "休息" });
    case "offline":
      return t({ ko: "오프라인", en: "Offline", ja: "オフライン", zh: "离线" });
    case "idle":
    default:
      return t({ ko: "대기", en: "Idle", ja: "待機", zh: "待机" });
  }
}

function getSignalAccent(tone: HomeSignalTone): string {
  switch (tone) {
    case "success":
      return "#22c55e";
    case "warn":
      return "#f59e0b";
    case "danger":
      return "#ef4444";
    case "info":
    default:
      return "#14b8a6";
  }
}
