import { useEffect, useMemo, useState } from "react";
import { getHealth, type HealthResponse } from "../../api";
import type { TFunction } from "./model";

type HealthMetricLevel = "normal" | "warning" | "danger";
type HealthPollState = "loading" | "live" | "stale" | "error" | "empty";

interface MetricThreshold {
  warning: number;
  danger: number;
}

interface HealthMetricCard {
  id: string;
  label: string;
  value: string;
  note: string;
  level: HealthMetricLevel;
}

interface HealthWidgetProps {
  t: TFunction;
}

interface PollStateArgs {
  data: HealthResponse | null;
  error: string | null;
  isRefreshing: boolean;
  lastSuccessAt: number | null;
  now: number;
}

const POLL_INTERVAL_MS = 30_000;
export const HEALTH_STALE_AFTER_MS = 75_000;

const THRESHOLDS = {
  deferred_hooks: { warning: 1, danger: 3 },
  outbox_age: { warning: 30, danger: 60 },
  queue_depth: { warning: 1, danger: 3 },
  watcher_count: { warning: 4, danger: 8 },
  recovery_duration: { warning: 180, danger: 600 },
} satisfies Record<string, MetricThreshold>;

const LEVEL_THEME: Record<HealthMetricLevel, { accent: string; surface: string; border: string; text: string }> = {
  normal: {
    accent: "#38bdf8",
    surface: "rgba(56,189,248,0.10)",
    border: "rgba(56,189,248,0.22)",
    text: "#7dd3fc",
  },
  warning: {
    accent: "#f59e0b",
    surface: "rgba(245,158,11,0.12)",
    border: "rgba(245,158,11,0.24)",
    text: "#fbbf24",
  },
  danger: {
    accent: "#ef4444",
    surface: "rgba(239,68,68,0.12)",
    border: "rgba(239,68,68,0.28)",
    text: "#fca5a5",
  },
};

export function metricLevel(value: number, threshold: MetricThreshold): HealthMetricLevel {
  if (value >= threshold.danger) return "danger";
  if (value >= threshold.warning) return "warning";
  return "normal";
}

export function isHealthResponseEmpty(data: HealthResponse | null): boolean {
  if (!data) return true;
  return [
    data.deferred_hooks,
    data.outbox_age ?? data.dispatch_outbox?.oldest_pending_age,
    data.queue_depth,
    data.watcher_count,
    data.recovery_duration,
  ].every((value) => value == null);
}

export function derivePollState({
  data,
  error,
  isRefreshing,
  lastSuccessAt,
  now,
}: PollStateArgs): HealthPollState {
  if (!data && isRefreshing) return "loading";
  if (!data && error) return "error";
  if (isHealthResponseEmpty(data)) return "empty";
  if (data && error) return "stale";
  if (data && lastSuccessAt && now - lastSuccessAt > HEALTH_STALE_AFTER_MS) return "stale";
  if (isRefreshing) return "loading";
  return "live";
}

export function describeDegradedReason(reason: string): string {
  const providerMatch = reason.match(/^provider:([^:]+):(.+)$/);
  if (providerMatch) {
    const provider = providerMatch[1].toUpperCase();
    const detail = providerMatch[2];
    if (detail === "disconnected") return `${provider} disconnected`;
    if (detail === "restart_pending") return `${provider} restart pending`;
    if (detail === "reconcile_in_progress") return `${provider} reconcile in progress`;

    const deferredMatch = detail.match(/^deferred_hooks_backlog:(\d+)$/);
    if (deferredMatch) return `${provider} deferred hooks ${deferredMatch[1]}`;

    const queueMatch = detail.match(/^pending_queue_depth:(\d+)$/);
    if (queueMatch) return `${provider} queue depth ${queueMatch[1]}`;

    const recoveryMatch = detail.match(/^recovering_channels:(\d+)$/);
    if (recoveryMatch) return `${provider} recovering ${recoveryMatch[1]} channels`;
  }

  const outboxMatch = reason.match(/^dispatch_outbox_oldest_pending_age:(\d+)$/);
  if (outboxMatch) return `Dispatch outbox age ${formatDurationCompact(Number(outboxMatch[1]))}`;

  if (reason === "db_unavailable") return "Database unavailable";
  if (reason === "no_providers_registered") return "No providers registered";
  return reason.replaceAll("_", " ");
}

function formatInteger(value: number): string {
  return new Intl.NumberFormat("en-US").format(value);
}

function formatDurationCompact(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0s";
  const rounded = Math.round(value);
  if (rounded >= 3600) {
    const hours = Math.floor(rounded / 3600);
    const minutes = Math.floor((rounded % 3600) / 60);
    return `${hours}h ${minutes}m`;
  }
  if (rounded >= 60) {
    const minutes = Math.floor(rounded / 60);
    const seconds = rounded % 60;
    return `${minutes}m ${seconds}s`;
  }
  return `${rounded}s`;
}

function translateStatus(status: string, t: TFunction): string {
  switch (status) {
    case "healthy":
      return t({ ko: "정상", en: "Healthy", ja: "正常", zh: "正常" });
    case "degraded":
      return t({ ko: "주의", en: "Degraded", ja: "低下", zh: "降级" });
    case "unhealthy":
      return t({ ko: "장애", en: "Unhealthy", ja: "異常", zh: "异常" });
    default:
      return status.toUpperCase();
  }
}

function translatePollState(state: HealthPollState, t: TFunction): string {
  switch (state) {
    case "loading":
      return t({ ko: "동기화 중", en: "Syncing", ja: "同期中", zh: "同步中" });
    case "stale":
      return t({ ko: "지연", en: "Stale", ja: "遅延", zh: "延迟" });
    case "error":
      return t({ ko: "오류", en: "Error", ja: "エラー", zh: "错误" });
    case "empty":
      return t({ ko: "데이터 없음", en: "Empty", ja: "データなし", zh: "无数据" });
    case "live":
    default:
      return t({ ko: "실시간", en: "Live", ja: "ライブ", zh: "实时" });
  }
}

function buildMetricCards(data: HealthResponse, t: TFunction): HealthMetricCard[] {
  const deferredHooks = data.deferred_hooks ?? 0;
  const outboxAge = data.outbox_age ?? data.dispatch_outbox?.oldest_pending_age ?? 0;
  const queueDepth = data.queue_depth ?? 0;
  const watcherCount = data.watcher_count ?? 0;
  const recoveryDuration = data.recovery_duration ?? 0;
  const outboxStats = data.dispatch_outbox;

  return [
    {
      id: "deferred-hooks",
      label: t({ ko: "Deferred Hooks", en: "Deferred Hooks", ja: "Deferred Hooks", zh: "Deferred Hooks" }),
      value: formatInteger(deferredHooks),
      note: t({
        ko: deferredHooks > 0 ? `백로그 ${formatInteger(deferredHooks)}건` : "백로그 없음",
        en: deferredHooks > 0 ? `${formatInteger(deferredHooks)} backlog items` : "No backlog",
        ja: deferredHooks > 0 ? `バックログ ${formatInteger(deferredHooks)} 件` : "バックログなし",
        zh: deferredHooks > 0 ? `积压 ${formatInteger(deferredHooks)} 项` : "无积压",
      }),
      level: metricLevel(deferredHooks, THRESHOLDS.deferred_hooks),
    },
    {
      id: "outbox-age",
      label: t({ ko: "Outbox Age", en: "Outbox Age", ja: "Outbox Age", zh: "Outbox Age" }),
      value: formatDurationCompact(outboxAge),
      note: t({
        ko: `pending ${formatInteger(outboxStats?.pending ?? 0)} · retry ${formatInteger(outboxStats?.retrying ?? 0)} · fail ${formatInteger(outboxStats?.permanent_failures ?? 0)}`,
        en: `pending ${formatInteger(outboxStats?.pending ?? 0)} · retry ${formatInteger(outboxStats?.retrying ?? 0)} · fail ${formatInteger(outboxStats?.permanent_failures ?? 0)}`,
        ja: `pending ${formatInteger(outboxStats?.pending ?? 0)} · retry ${formatInteger(outboxStats?.retrying ?? 0)} · fail ${formatInteger(outboxStats?.permanent_failures ?? 0)}`,
        zh: `pending ${formatInteger(outboxStats?.pending ?? 0)} · retry ${formatInteger(outboxStats?.retrying ?? 0)} · fail ${formatInteger(outboxStats?.permanent_failures ?? 0)}`,
      }),
      level: metricLevel(outboxAge, THRESHOLDS.outbox_age),
    },
    {
      id: "queue-depth",
      label: t({ ko: "Pending Queue", en: "Pending Queue", ja: "Pending Queue", zh: "Pending Queue" }),
      value: formatInteger(queueDepth),
      note: t({
        ko: `active ${formatInteger(data.global_active ?? 0)} · finalizing ${formatInteger(data.global_finalizing ?? 0)}`,
        en: `active ${formatInteger(data.global_active ?? 0)} · finalizing ${formatInteger(data.global_finalizing ?? 0)}`,
        ja: `active ${formatInteger(data.global_active ?? 0)} · finalizing ${formatInteger(data.global_finalizing ?? 0)}`,
        zh: `active ${formatInteger(data.global_active ?? 0)} · finalizing ${formatInteger(data.global_finalizing ?? 0)}`,
      }),
      level: metricLevel(queueDepth, THRESHOLDS.queue_depth),
    },
    {
      id: "watchers",
      label: t({ ko: "Active Watchers", en: "Active Watchers", ja: "Active Watchers", zh: "Active Watchers" }),
      value: formatInteger(watcherCount),
      note: t({
        ko: `${formatInteger(data.providers?.length ?? 0)} providers 관찰 중`,
        en: `${formatInteger(data.providers?.length ?? 0)} providers in scope`,
        ja: `${formatInteger(data.providers?.length ?? 0)} providers in scope`,
        zh: `${formatInteger(data.providers?.length ?? 0)} providers in scope`,
      }),
      level: metricLevel(watcherCount, THRESHOLDS.watcher_count),
    },
    {
      id: "recovery-duration",
      label: t({ ko: "Recovery", en: "Recovery", ja: "Recovery", zh: "Recovery" }),
      value: formatDurationCompact(recoveryDuration),
      note: t({
        ko: `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
        en: `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
        ja: `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
        zh: `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
      }),
      level: metricLevel(recoveryDuration, THRESHOLDS.recovery_duration),
    },
  ];
}

function headerLevel(status: string, metrics: HealthMetricCard[]): HealthMetricLevel {
  if (status === "unhealthy") return "danger";
  if (status === "degraded") return "warning";
  if (metrics.some((metric) => metric.level === "danger")) return "danger";
  if (metrics.some((metric) => metric.level === "warning")) return "warning";
  return "normal";
}

function buildSummary(data: HealthResponse, t: TFunction): string {
  const providers = data.providers ?? [];
  const connected = providers.filter((provider) => provider.connected).length;
  const activeTurns = providers.reduce((sum, provider) => sum + provider.active_turns, 0);
  const sessions = providers.reduce((sum, provider) => sum + provider.sessions, 0);
  const topReason = data.degraded_reasons?.[0];

  if (data.status === "unhealthy" && topReason) {
    return t({
      ko: `즉시 확인 필요: ${describeDegradedReason(topReason)}`,
      en: `Immediate action: ${describeDegradedReason(topReason)}`,
      ja: `即時確認: ${describeDegradedReason(topReason)}`,
      zh: `需要立即处理: ${describeDegradedReason(topReason)}`,
    });
  }

  if (data.status === "degraded" && topReason) {
    return t({
      ko: `경고: ${describeDegradedReason(topReason)}`,
      en: `Warning: ${describeDegradedReason(topReason)}`,
      ja: `警告: ${describeDegradedReason(topReason)}`,
      zh: `警告: ${describeDegradedReason(topReason)}`,
    });
  }

  return t({
    ko: `${connected}/${providers.length} provider 연결 · active ${formatInteger(activeTurns)} · session ${formatInteger(sessions)}`,
    en: `${connected}/${providers.length} providers connected · active ${formatInteger(activeTurns)} · sessions ${formatInteger(sessions)}`,
    ja: `${connected}/${providers.length} providers connected · active ${formatInteger(activeTurns)} · sessions ${formatInteger(sessions)}`,
    zh: `${connected}/${providers.length} providers connected · active ${formatInteger(activeTurns)} · sessions ${formatInteger(sessions)}`,
  });
}

function formatUpdatedAt(timestamp: number | null): string {
  if (!timestamp) return "n/a";
  return new Date(timestamp).toLocaleTimeString("ko-KR", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

export default function HealthWidget({ t }: HealthWidgetProps) {
  const [data, setData] = useState<HealthResponse | null>(null);
  const [lastSuccessAt, setLastSuccessAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(true);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      if (mounted) setIsRefreshing(true);
      try {
        const next = await getHealth();
        if (!mounted) return;
        setData(next);
        setLastSuccessAt(Date.now());
        setError(null);
      } catch (nextError) {
        if (!mounted) return;
        const resolved = nextError instanceof Error ? nextError.message : String(nextError);
        setError(resolved);
      } finally {
        if (mounted) setIsRefreshing(false);
      }
    };

    void load();
    const pollTimer = window.setInterval(() => void load(), POLL_INTERVAL_MS);
    const staleTimer = window.setInterval(() => setNow(Date.now()), 15_000);
    return () => {
      mounted = false;
      window.clearInterval(pollTimer);
      window.clearInterval(staleTimer);
    };
  }, []);

  const pollState = derivePollState({ data, error, isRefreshing, lastSuccessAt, now });
  const metrics = useMemo(() => (data ? buildMetricCards(data, t) : []), [data, t]);
  const topLevel = headerLevel(data?.status ?? "healthy", metrics);
  const theme = LEVEL_THEME[topLevel];
  const summary = data ? buildSummary(data, t) : t({
    ko: "런타임 health metric을 불러오는 중입니다.",
    en: "Loading runtime health metrics.",
    ja: "ランタイム health metric を読み込み中です。",
    zh: "正在加载运行时健康指标。",
  });

  return (
    <div className="game-panel relative overflow-hidden px-3 py-3 sm:px-4 sm:py-3.5">
      <div
        className="pointer-events-none absolute inset-x-0 top-0 h-px"
        style={{ background: `linear-gradient(90deg, transparent, ${theme.accent}, transparent)` }}
      />

      <div className="mb-3 flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-[10px] sm:text-xs font-bold uppercase tracking-wider" style={{ color: theme.text }}>
            {t({ ko: "운영 Health", en: "OPERATIONS HEALTH", ja: "運用 HEALTH", zh: "运营 HEALTH" })}
          </div>
          <div className="text-[10px]" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "deferred/outbox/queue/watcher/recovery 상태를 한 카드에서 봅니다",
              en: "Deferred, outbox, queue, watcher, and recovery state in one card",
              ja: "deferred / outbox / queue / watcher / recovery を 1 枚で確認",
              zh: "在一张卡里查看 deferred / outbox / queue / watcher / recovery",
            })}
          </div>
        </div>

        <div className="flex flex-wrap items-center justify-end gap-1.5">
          <span
            className="rounded-full px-2 py-1 text-[9px] font-bold uppercase tracking-[0.18em]"
            style={{
              color: theme.text,
              background: theme.surface,
              border: `1px solid ${theme.border}`,
            }}
          >
            {translateStatus(data?.status ?? "healthy", t)}
          </span>
          <span
            className="rounded-full px-2 py-1 text-[9px] font-bold uppercase tracking-[0.18em]"
            style={{
              color: pollState === "error" ? "#fca5a5" : pollState === "stale" ? "#fbbf24" : "#93c5fd",
              background: pollState === "error" ? "rgba(239,68,68,0.12)" : pollState === "stale" ? "rgba(245,158,11,0.12)" : "rgba(59,130,246,0.12)",
              border: pollState === "error" ? "1px solid rgba(239,68,68,0.24)" : pollState === "stale" ? "1px solid rgba(245,158,11,0.24)" : "1px solid rgba(59,130,246,0.24)",
            }}
          >
            {translatePollState(pollState, t)}
          </span>
        </div>
      </div>

      <div
        className="rounded-2xl px-3 py-2.5"
        style={{
          background: `linear-gradient(135deg, ${theme.surface}, rgba(15,23,42,0.22))`,
          border: `1px solid ${theme.border}`,
        }}
      >
        <div className="text-[11px] font-semibold leading-5" style={{ color: "var(--th-text-primary)" }}>
          {summary}
        </div>
        <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[10px]" style={{ color: "var(--th-text-muted)" }}>
          <span>{t({ ko: `업데이트 ${formatUpdatedAt(lastSuccessAt)}`, en: `Updated ${formatUpdatedAt(lastSuccessAt)}`, ja: `Updated ${formatUpdatedAt(lastSuccessAt)}`, zh: `Updated ${formatUpdatedAt(lastSuccessAt)}` })}</span>
          {data?.db === false ? <span>{t({ ko: "DB 비정상", en: "DB down", ja: "DB down", zh: "DB down" })}</span> : null}
          {data?.dashboard === false ? <span>{t({ ko: "Dashboard dist 없음", en: "Dashboard dist missing", ja: "Dashboard dist missing", zh: "Dashboard dist missing" })}</span> : null}
        </div>
      </div>

      {data?.degraded_reasons && data.degraded_reasons.length > 0 ? (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {data.degraded_reasons.slice(0, 3).map((reason) => (
            <span
              key={reason}
              className="rounded-full px-2 py-1 text-[9px] font-medium"
              style={{
                color: topLevel === "danger" ? "#fca5a5" : "#fbbf24",
                background: topLevel === "danger" ? "rgba(239,68,68,0.12)" : "rgba(245,158,11,0.10)",
                border: topLevel === "danger" ? "1px solid rgba(239,68,68,0.22)" : "1px solid rgba(245,158,11,0.20)",
              }}
            >
              {describeDegradedReason(reason)}
            </span>
          ))}
        </div>
      ) : null}

      {error ? (
        <div
          className="mt-2 rounded-xl px-3 py-2 text-[10px]"
          style={{
            color: data ? "#fbbf24" : "#fca5a5",
            background: data ? "rgba(245,158,11,0.10)" : "rgba(239,68,68,0.12)",
            border: data ? "1px solid rgba(245,158,11,0.22)" : "1px solid rgba(239,68,68,0.24)",
          }}
        >
          {data
            ? t({
              ko: `최근 요청 실패. 마지막 정상값 유지 중: ${error}`,
              en: `Latest request failed. Showing cached values: ${error}`,
              ja: `直近のリクエスト失敗。キャッシュを表示中: ${error}`,
              zh: `最近请求失败，显示缓存值: ${error}`,
            })
            : t({
              ko: `health 응답을 받지 못했습니다: ${error}`,
              en: `Health request failed: ${error}`,
              ja: `health リクエスト失敗: ${error}`,
              zh: `health 请求失败: ${error}`,
            })}
        </div>
      ) : null}

      {pollState === "empty" ? (
        <div
          className="mt-3 rounded-xl px-3 py-5 text-center text-[11px]"
          style={{
            color: "var(--th-text-muted)",
            border: "1px dashed rgba(148,163,184,0.22)",
            background: "rgba(15,23,42,0.18)",
          }}
        >
          {t({
            ko: "표시할 health metric이 아직 없습니다.",
            en: "No health metrics available yet.",
            ja: "表示できる health metric がまだありません。",
            zh: "暂时没有可显示的健康指标。",
          })}
        </div>
      ) : (
        <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-2">
          {metrics.map((metric) => {
            const metricTheme = LEVEL_THEME[metric.level];
            return (
              <div
                key={metric.id}
                className="rounded-2xl px-3 py-2.5"
                style={{
                  border: `1px solid ${metricTheme.border}`,
                  background: `linear-gradient(180deg, ${metricTheme.surface}, rgba(15,23,42,0.22))`,
                }}
              >
                <div className="flex items-start justify-between gap-2">
                  <span className="text-[10px] font-semibold uppercase tracking-[0.16em]" style={{ color: metricTheme.text }}>
                    {metric.label}
                  </span>
                  <span
                    className="mt-0.5 inline-block h-2 w-2 shrink-0 rounded-full"
                    style={{ background: metricTheme.accent, boxShadow: `0 0 10px ${metricTheme.accent}66` }}
                  />
                </div>
                <div className="mt-2 text-xl font-black leading-none sm:text-2xl" style={{ color: "var(--th-text-primary)" }}>
                  {metric.value}
                </div>
                <div className="mt-1 text-[10px] leading-4" style={{ color: "var(--th-text-muted)" }}>
                  {metric.note}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
