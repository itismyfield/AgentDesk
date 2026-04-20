import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, RefreshCw, Wifi, WifiOff } from "lucide-react";
import { getHealth, type HealthResponse } from "../api";
import type { Agent, Office, WSEvent } from "../types";
import OfficeManagerView from "./OfficeManagerView";
import { describeDegradedReason } from "./dashboard/HealthWidget";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetaBadge,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";

interface OpsPageViewProps {
  wsConnected: boolean;
  offices: Office[];
  allAgents: Agent[];
  selectedOfficeId?: string | null;
  isKo: boolean;
  onChanged: () => void;
}

type SignalSeverity = "normal" | "warning" | "danger";

interface Threshold {
  warning: number;
  danger: number;
}

interface SignalCard {
  key: "deferred_hooks" | "outbox_age" | "pending_queue" | "active_watchers" | "recovery_seconds";
  label: string;
  value: string;
  rawValue: number;
  severity: SignalSeverity;
  note: string;
}

interface BottleneckRow {
  kind: string;
  count: number;
  severity: Exclude<SignalSeverity, "normal">;
  detail: string;
}

const STALE_AFTER_MS = 75_000;
const LIVE_POLL_INTERVAL_MS = 5_000;
const DISCONNECTED_POLL_BASE_MS = 5_000;
const MAX_DISCONNECTED_POLL_MS = 30_000;
const WS_REFRESH_DEBOUNCE_MS = 800;

const SIGNAL_THRESHOLDS: Record<SignalCard["key"], Threshold> = {
  deferred_hooks: { warning: 1, danger: 3 },
  outbox_age: { warning: 30, danger: 60 },
  pending_queue: { warning: 1, danger: 3 },
  active_watchers: { warning: 4, danger: 8 },
  recovery_seconds: { warning: 180, danger: 600 },
};

function resolveSeverity(value: number, threshold: Threshold): SignalSeverity {
  if (value >= threshold.danger) return "danger";
  if (value >= threshold.warning) return "warning";
  return "normal";
}

function severityRank(severity: SignalSeverity): number {
  switch (severity) {
    case "danger":
      return 2;
    case "warning":
      return 1;
    default:
      return 0;
  }
}

function toneForSeverity(severity: SignalSeverity): "info" | "warn" | "danger" | "success" {
  switch (severity) {
    case "danger":
      return "danger";
    case "warning":
      return "warn";
    default:
      return "success";
  }
}

function formatNumber(value: number): string {
  return new Intl.NumberFormat("en-US").format(value);
}

function formatDurationCompact(seconds: number): string {
  const safe = Math.max(0, Math.round(seconds));
  if (safe >= 3600) {
    const hours = Math.floor(safe / 3600);
    const minutes = Math.floor((safe % 3600) / 60);
    return `${hours}h ${minutes}m`;
  }
  if (safe >= 60) {
    const minutes = Math.floor(safe / 60);
    const remainSeconds = safe % 60;
    return `${minutes}m ${remainSeconds}s`;
  }
  return `${safe}s`;
}

function formatUpdatedAt(timestamp: number | null, localeTag: string): string {
  if (!timestamp) return "n/a";
  return new Date(timestamp).toLocaleTimeString(localeTag, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function buildSignalCards(data: HealthResponse, isKo: boolean): SignalCard[] {
  const deferredHooks = data.deferred_hooks ?? 0;
  const outboxAge = data.outbox_age ?? data.dispatch_outbox?.oldest_pending_age ?? 0;
  const pendingQueue = data.queue_depth ?? 0;
  const activeWatchers = data.watcher_count ?? 0;
  const recoverySeconds = data.recovery_duration ?? 0;
  const providerCount = data.providers?.length ?? 0;

  return [
    {
      key: "deferred_hooks",
      label: "Deferred Hooks",
      value: formatNumber(deferredHooks),
      rawValue: deferredHooks,
      severity: resolveSeverity(deferredHooks, SIGNAL_THRESHOLDS.deferred_hooks),
      note: isKo
        ? `후처리 대기 ${formatNumber(deferredHooks)}건`
        : `${formatNumber(deferredHooks)} waiting for post-processing`,
    },
    {
      key: "outbox_age",
      label: "Outbox Age",
      value: formatDurationCompact(outboxAge),
      rawValue: outboxAge,
      severity: resolveSeverity(outboxAge, SIGNAL_THRESHOLDS.outbox_age),
      note: isKo
        ? `pending ${formatNumber(data.dispatch_outbox?.pending ?? 0)} · retry ${formatNumber(data.dispatch_outbox?.retrying ?? 0)}`
        : `pending ${formatNumber(data.dispatch_outbox?.pending ?? 0)} · retry ${formatNumber(data.dispatch_outbox?.retrying ?? 0)}`,
    },
    {
      key: "pending_queue",
      label: "Pending Queue",
      value: formatNumber(pendingQueue),
      rawValue: pendingQueue,
      severity: resolveSeverity(pendingQueue, SIGNAL_THRESHOLDS.pending_queue),
      note: isKo
        ? `active ${formatNumber(data.global_active ?? 0)} · finalizing ${formatNumber(data.global_finalizing ?? 0)}`
        : `active ${formatNumber(data.global_active ?? 0)} · finalizing ${formatNumber(data.global_finalizing ?? 0)}`,
    },
    {
      key: "active_watchers",
      label: "Active Watchers",
      value: formatNumber(activeWatchers),
      rawValue: activeWatchers,
      severity: resolveSeverity(activeWatchers, SIGNAL_THRESHOLDS.active_watchers),
      note: isKo
        ? `${formatNumber(providerCount)}개 provider 추적 중`
        : `${formatNumber(providerCount)} providers in scope`,
    },
    {
      key: "recovery_seconds",
      label: "Recovery",
      value: formatDurationCompact(recoverySeconds),
      rawValue: recoverySeconds,
      severity: resolveSeverity(recoverySeconds, SIGNAL_THRESHOLDS.recovery_seconds),
      note: isKo
        ? `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`
        : `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
    },
  ];
}

function buildBottlenecks(data: HealthResponse): BottleneckRow[] {
  const rows: BottleneckRow[] = [];
  const outboxAge = data.outbox_age ?? data.dispatch_outbox?.oldest_pending_age ?? 0;
  const disconnectedProviders = (data.providers ?? []).filter((provider) => !provider.connected).length;
  const restartPendingProviders = (data.providers ?? []).filter((provider) => provider.restart_pending).length;
  const providerQueueDepth = (data.providers ?? []).reduce(
    (sum, provider) => sum + Math.max(provider.queue_depth ?? 0, 0),
    0,
  );

  const addMetricBottleneck = (
    kind: string,
    count: number,
    key: SignalCard["key"],
    detail: string,
  ) => {
    const severity = resolveSeverity(count, SIGNAL_THRESHOLDS[key]);
    if (severity === "normal" || count <= 0) return;
    rows.push({ kind, count, severity, detail });
  };

  addMetricBottleneck("deferred_hooks", data.deferred_hooks ?? 0, "deferred_hooks", "deferred hook backlog");
  addMetricBottleneck("outbox_age", outboxAge, "outbox_age", "oldest outbox item age (seconds)");
  addMetricBottleneck("pending_queue", data.queue_depth ?? 0, "pending_queue", "global pending queue depth");
  addMetricBottleneck("recovery_seconds", data.recovery_duration ?? 0, "recovery_seconds", "long-running recovery window");
  addMetricBottleneck("active_watchers", data.watcher_count ?? 0, "active_watchers", "watcher load");

  if (disconnectedProviders > 0) {
    rows.push({
      kind: "provider_disconnects",
      count: disconnectedProviders,
      severity: disconnectedProviders >= 2 ? "danger" : "warning",
      detail: "providers disconnected",
    });
  }

  if (restartPendingProviders > 0) {
    rows.push({
      kind: "restart_pending",
      count: restartPendingProviders,
      severity: restartPendingProviders >= 2 ? "danger" : "warning",
      detail: "providers waiting for restart",
    });
  }

  if (providerQueueDepth > 0) {
    rows.push({
      kind: "provider_queue",
      count: providerQueueDepth,
      severity: resolveSeverity(providerQueueDepth, SIGNAL_THRESHOLDS.pending_queue) === "danger" ? "danger" : "warning",
      detail: "aggregate provider queue depth",
    });
  }

  if (rows.length === 0 && (data.degraded_reasons?.length ?? 0) > 0) {
    for (const reason of data.degraded_reasons ?? []) {
      rows.push({
        kind: reason,
        count: 1,
        severity: data.status === "unhealthy" ? "danger" : "warning",
        detail: describeDegradedReason(reason),
      });
    }
  }

  return rows.sort((left, right) => {
    const severityDelta = severityRank(right.severity) - severityRank(left.severity);
    if (severityDelta !== 0) return severityDelta;
    return right.count - left.count || left.kind.localeCompare(right.kind);
  });
}

function translateStatus(status: string, isKo: boolean): string {
  if (status === "healthy") return isKo ? "정상" : "Healthy";
  if (status === "degraded") return isKo ? "주의" : "Degraded";
  if (status === "unhealthy") return isKo ? "장애" : "Unhealthy";
  return status.toUpperCase();
}

export default function OpsPageView({
  wsConnected,
  offices,
  allAgents,
  selectedOfficeId,
  isKo,
  onChanged,
}: OpsPageViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const localeTag = isKo ? "ko-KR" : "en-US";

  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [lastSuccessAt, setLastSuccessAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [failureCount, setFailureCount] = useState(0);
  const [now, setNow] = useState(() => Date.now());
  const refreshInFlightRef = useRef(false);

  const refreshHealth = useCallback(async () => {
    if (refreshInFlightRef.current) return;
    refreshInFlightRef.current = true;
    setIsRefreshing(true);
    try {
      const next = await getHealth();
      setHealth(next);
      setLastSuccessAt(Date.now());
      setError(null);
      setFailureCount(0);
    } catch (nextError) {
      setError(nextError instanceof Error ? nextError.message : String(nextError));
      setFailureCount((current) => current + 1);
    } finally {
      refreshInFlightRef.current = false;
      setIsRefreshing(false);
    }
  }, []);

  useEffect(() => {
    void refreshHealth();
  }, [refreshHealth]);

  useEffect(() => {
    if (!wsConnected) return;
    void refreshHealth();
  }, [refreshHealth, wsConnected]);

  useEffect(() => {
    const intervalMs = wsConnected
      ? LIVE_POLL_INTERVAL_MS
      : Math.min(MAX_DISCONNECTED_POLL_MS, DISCONNECTED_POLL_BASE_MS * 2 ** Math.min(failureCount, 4));

    const timer = window.setInterval(() => {
      void refreshHealth();
    }, intervalMs);

    return () => {
      window.clearInterval(timer);
    };
  }, [failureCount, refreshHealth, wsConnected]);

  useEffect(() => {
    const staleTimer = window.setInterval(() => {
      setNow(Date.now());
    }, 15_000);

    return () => {
      window.clearInterval(staleTimer);
    };
  }, []);

  useEffect(() => {
    let timer: number | null = null;

    const handleWsEvent = (event: Event) => {
      const detail = (event as CustomEvent<WSEvent>).detail;
      if (!detail?.type) return;
      if (timer != null) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        void refreshHealth();
      }, WS_REFRESH_DEBOUNCE_MS);
    };

    window.addEventListener("pcd-ws-event", handleWsEvent);
    return () => {
      if (timer != null) window.clearTimeout(timer);
      window.removeEventListener("pcd-ws-event", handleWsEvent);
    };
  }, [refreshHealth]);

  const signals = useMemo(
    () => (health ? buildSignalCards(health, isKo) : []),
    [health, isKo],
  );
  const bottlenecks = useMemo(
    () => (health ? buildBottlenecks(health) : []),
    [health],
  );
  const stale = lastSuccessAt != null && now - lastSuccessAt > STALE_AFTER_MS;
  const providerCount = health?.providers?.length ?? 0;
  const connectedProviders = (health?.providers ?? []).filter((provider) => provider.connected).length;
  const lastUpdatedLabel = formatUpdatedAt(lastSuccessAt, localeTag);
  const disconnectedPollMs = Math.min(
    MAX_DISCONNECTED_POLL_MS,
    DISCONNECTED_POLL_BASE_MS * 2 ** Math.min(failureCount, 4),
  );

  return (
    <div
      data-testid="ops-page"
      className="mx-auto w-full max-w-6xl min-w-0 space-y-4 overflow-x-hidden p-4 pb-40 sm:h-full sm:overflow-y-auto sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <SurfaceSection
        eyebrow="OPS"
        title={tr("운영 컨트롤", "Ops Control")}
        description={tr(
          "런타임 헬스와 병목 신호를 먼저 보여주고, 실시간 WS 이벤트가 들어오면 health를 다시 읽습니다. WS가 끊기면 내부 polling 간격을 늘려가며 계속 갱신합니다.",
          "Runtime health and bottleneck pressure come first. Incoming WS events trigger health refreshes, and when WS drops the page falls back to a widening internal polling interval.",
        )}
        badge={health ? translateStatus(health.status, isKo) : tr("초기 로드", "Initial load")}
        actions={
          <>
            <SurfaceMetaBadge tone={wsConnected ? "success" : "danger"}>
              <span className="inline-flex items-center gap-1.5">
                {wsConnected ? <Wifi size={12} /> : <WifiOff size={12} />}
                {wsConnected ? "LIVE" : "DISCONNECTED"}
              </span>
            </SurfaceMetaBadge>
            {stale ? <SurfaceMetaBadge tone="warn">STALE</SurfaceMetaBadge> : null}
            <SurfaceActionButton onClick={() => void refreshHealth()} disabled={isRefreshing}>
              <span className="inline-flex items-center gap-1.5">
                <RefreshCw size={13} className={isRefreshing ? "animate-spin" : undefined} />
                {isRefreshing ? tr("동기화 중", "Refreshing") : tr("새로고침", "Refresh")}
              </span>
            </SurfaceActionButton>
          </>
        }
      >
        <div className="mt-5 flex flex-wrap items-center gap-2">
          <SurfaceMetaBadge tone={health?.status === "unhealthy" ? "danger" : health?.status === "degraded" ? "warn" : "success"}>
            {health ? translateStatus(health.status, isKo) : tr("대기 중", "Pending")}
          </SurfaceMetaBadge>
          <SurfaceMetaBadge>{tr(`업데이트 ${lastUpdatedLabel}`, `Updated ${lastUpdatedLabel}`)}</SurfaceMetaBadge>
          <SurfaceMetaBadge>{tr(`provider ${connectedProviders}/${providerCount}`, `providers ${connectedProviders}/${providerCount}`)}</SurfaceMetaBadge>
          <SurfaceMetaBadge>{tr(`fallback poll ${Math.round(disconnectedPollMs / 1000)}s`, `fallback poll ${Math.round(disconnectedPollMs / 1000)}s`)}</SurfaceMetaBadge>
        </div>

        {error ? (
          <SurfaceNotice tone={health ? "warn" : "danger"} className="mt-4" leading={<AlertTriangle size={16} />}>
            <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
              {health
                ? tr("최근 health 요청이 실패해 마지막 정상값을 유지 중입니다.", "Latest health request failed, keeping the last successful snapshot.")
                : tr("health 응답을 아직 받지 못했습니다.", "Health response has not arrived yet.")}
            </div>
            <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {error}
            </div>
          </SurfaceNotice>
        ) : null}

        {health?.degraded_reasons && health.degraded_reasons.length > 0 ? (
          <div className="mt-4 flex flex-wrap gap-2">
            {health.degraded_reasons.slice(0, 4).map((reason) => (
              <SurfaceMetaBadge
                key={reason}
                tone={health.status === "unhealthy" ? "danger" : "warn"}
              >
                {describeDegradedReason(reason)}
              </SurfaceMetaBadge>
            ))}
          </div>
        ) : null}

        <div data-testid="ops-signal-grid" className="mt-5 grid gap-3 sm:grid-cols-2 xl:grid-cols-5">
          {signals.length > 0 ? (
            signals.map((signal) => (
              <SurfaceCard
                key={signal.key}
                data-testid={`ops-signal-${signal.key}`}
                className="min-w-0 rounded-3xl p-4"
                style={{
                  borderColor:
                    signal.severity === "danger"
                      ? "var(--color-danger-border)"
                      : signal.severity === "warning"
                        ? "var(--color-warning-border)"
                        : "var(--color-info-border)",
                  background:
                    signal.severity === "danger"
                      ? "var(--color-danger-soft)"
                      : signal.severity === "warning"
                        ? "var(--color-warning-soft)"
                        : "var(--color-info-soft)",
                }}
              >
                <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  {signal.key}
                </div>
                <div className="mt-3 text-2xl font-semibold tracking-tight" style={{ color: "var(--th-text-primary)" }}>
                  {signal.value}
                </div>
                <div className="mt-1 text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                  {signal.label}
                </div>
                <div className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {signal.note}
                </div>
              </SurfaceCard>
            ))
          ) : (
            <div className="sm:col-span-2 xl:col-span-5">
              <SurfaceEmptyState className="py-8">
                <div className="flex flex-col items-center gap-2 text-center">
                  <AlertTriangle size={20} style={{ color: "var(--th-text-muted)" }} />
                  <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                    {tr("표시할 health signal이 아직 없습니다.", "No health signals available yet.")}
                  </div>
                  <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {tr("초기 health 응답이 도착하면 signal grid가 채워집니다.", "The signal grid will populate after the first health response arrives.")}
                  </div>
                </div>
              </SurfaceEmptyState>
            </div>
          )}
        </div>
      </SurfaceSection>

      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.15fr)_minmax(320px,0.85fr)]">
        <SurfaceSubsection
          title={tr("Ops Bottlenecks", "Ops Bottlenecks")}
          description={tr(
            "헬스 응답에서 실제로 위험 신호가 난 항목만 추려 kind / count / severity로 정렬합니다.",
            "Only active risk signals from the health response are surfaced here, sorted by kind / count / severity.",
          )}
        >
          {bottlenecks.length > 0 ? (
            <div data-testid="ops-bottlenecks" className="mt-4 space-y-2">
              <div
                className="hidden items-center gap-3 rounded-2xl px-3 py-2 text-[11px] font-semibold uppercase tracking-[0.16em] md:grid"
                style={{
                  gridTemplateColumns: "minmax(0, 1.5fr) 96px 110px",
                  color: "var(--th-text-muted)",
                  background: "color-mix(in srgb, var(--th-overlay-medium) 84%, transparent)",
                }}
              >
                <span>kind</span>
                <span>count</span>
                <span>severity</span>
              </div>
              {bottlenecks.map((row) => (
                <div
                  key={`${row.kind}-${row.detail}`}
                  data-testid={`ops-bottleneck-${row.kind}`}
                  className="grid gap-3 rounded-2xl border px-3 py-3 md:items-center"
                  style={{
                    gridTemplateColumns: "minmax(0, 1fr)",
                    borderColor: row.severity === "danger" ? "var(--color-danger-border)" : "var(--color-warning-border)",
                    background: row.severity === "danger" ? "var(--color-danger-soft)" : "var(--color-warning-soft)",
                  }}
                >
                  <div className="md:hidden">
                    <div className="flex flex-wrap items-center gap-2">
                      <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                        {row.kind}
                      </div>
                      <SurfaceMetaBadge tone={toneForSeverity(row.severity)}>
                        {row.severity.toUpperCase()}
                      </SurfaceMetaBadge>
                    </div>
                    <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {row.detail}
                    </div>
                    <div className="mt-2 text-xs font-medium" style={{ color: "var(--th-text-primary)" }}>
                      count {formatNumber(row.count)}
                    </div>
                  </div>

                  <div
                    className="hidden md:grid md:items-center md:gap-3"
                    style={{ gridTemplateColumns: "minmax(0, 1.5fr) 96px 110px" }}
                  >
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                        {row.kind}
                      </div>
                      <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                        {row.detail}
                      </div>
                    </div>
                    <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                      {formatNumber(row.count)}
                    </div>
                    <div>
                      <SurfaceMetaBadge tone={toneForSeverity(row.severity)}>
                        {row.severity.toUpperCase()}
                      </SurfaceMetaBadge>
                    </div>
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <SurfaceEmptyState data-testid="ops-bottlenecks-empty" className="mt-4 py-8">
              <div className="flex flex-col items-center gap-2 text-center">
                <Wifi size={20} style={{ color: "var(--th-text-muted)" }} />
                <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                  {tr("현재 감지된 운영 병목이 없습니다.", "No active ops bottlenecks detected.")}
                </div>
                <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr("warning 이상 신호가 생기면 이 목록에 즉시 올라옵니다.", "Signals at warning level or above will appear here immediately.")}
                </div>
              </div>
            </SurfaceEmptyState>
          )}
        </SurfaceSubsection>

        <SurfaceSubsection
          title={tr("Connection & Delivery", "Connection & Delivery")}
          description={tr(
            "WS 연결 상태와 outbox/provider 요약을 빠르게 확인하는 보조 패널입니다.",
            "A compact side panel for WS connectivity and outbox/provider delivery status.",
          )}
        >
          <div data-testid="ops-connection-panel" className="mt-4 space-y-3">
            <SurfaceCard
              data-testid="ops-websocket-card"
              className="rounded-3xl p-4"
              style={{
                borderColor: wsConnected ? "var(--color-info-border)" : "var(--color-danger-border)",
                background: wsConnected ? "var(--color-info-soft)" : "var(--color-danger-soft)",
              }}
            >
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                  <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                    websocket
                  </div>
                  <div className="mt-2 text-lg font-semibold" style={{ color: "var(--th-text-primary)" }}>
                    {wsConnected ? tr("실시간 연결됨", "Connected live") : tr("연결 끊김", "Disconnected")}
                  </div>
                  <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {wsConnected
                      ? tr("pcd-ws-event 수신 시 health refresh를 즉시 재스케줄합니다.", "Incoming pcd-ws-event messages reschedule health refreshes immediately.")
                      : tr("WS가 복구될 때까지 내부 polling으로 health를 유지합니다.", "Internal polling keeps health current until WS recovers.")}
                  </div>
                </div>
                <SurfaceMetaBadge tone={wsConnected ? "success" : "danger"}>
                  {wsConnected ? "LIVE" : "DISCONNECTED"}
                </SurfaceMetaBadge>
              </div>
            </SurfaceCard>

            <div className="grid gap-3 sm:grid-cols-2">
              <SurfaceCard data-testid="ops-dispatch-outbox-card" className="rounded-3xl p-4">
                <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  dispatch_outbox
                </div>
                <div className="mt-3 text-xl font-semibold" style={{ color: "var(--th-text-primary)" }}>
                  {formatNumber(health?.dispatch_outbox?.pending ?? 0)}
                </div>
                <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
                    `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
                  )}
                </div>
              </SurfaceCard>

              <SurfaceCard data-testid="ops-providers-card" className="rounded-3xl p-4">
                <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                  providers
                </div>
                <div className="mt-3 text-xl font-semibold" style={{ color: "var(--th-text-primary)" }}>
                  {connectedProviders}/{providerCount}
                </div>
                <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    `restart pending ${formatNumber((health?.providers ?? []).filter((provider) => provider.restart_pending).length)}`,
                    `restart pending ${formatNumber((health?.providers ?? []).filter((provider) => provider.restart_pending).length)}`,
                  )}
                </div>
              </SurfaceCard>
            </div>
          </div>
        </SurfaceSubsection>
      </div>

      <div className="border-t pt-2" style={{ borderColor: "var(--th-border-subtle)" }}>
        <div className="-mx-4 sm:-mx-6">
          <OfficeManagerView
            offices={offices}
            allAgents={allAgents}
            selectedOfficeId={selectedOfficeId}
            isKo={isKo}
            onChanged={onChanged}
          />
        </div>
      </div>
    </div>
  );
}
