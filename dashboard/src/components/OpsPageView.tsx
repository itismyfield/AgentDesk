import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, RefreshCw, Wifi } from "lucide-react";
import { Link } from "react-router-dom";
import {
  getHealth,
  getPromptManifestRetention,
  type HealthResponse,
  type PromptManifestRetentionStatus,
} from "../api";
import type { WSEvent } from "../types";
import { describeDegradedReason } from "./dashboard/HealthWidget";
import { SurfaceEmptyState } from "./common/SurfacePrimitives";
import OpsConnectionPanel from "./OpsConnectionPanel";
import {
  DISCONNECTED_POLL_BASE_MS,
  LIVE_POLL_INTERVAL_MS,
  MAX_DISCONNECTED_POLL_MS,
  OPS_SHELL_STYLES,
  SIGNAL_THRESHOLDS,
  STALE_AFTER_MS,
  WS_REFRESH_DEBOUNCE_MS,
  buildBottlenecks,
  buildSignalCards,
  chipClassFromTone,
  errorMessage,
  formatBottleneckLabel,
  formatBytes,
  formatDurationCompact,
  formatNumber,
  formatUpdatedAt,
  resolveSeverity,
  surfaceStyleForSeverity,
  toneForSeverity,
  translateStatus,
  type RuntimeSignalRow,
} from "./OpsPageModel";


interface OpsPageViewProps {
  wsConnected: boolean;
  isKo: boolean;
}

export default function OpsPageView({
  wsConnected,
  isKo,
}: OpsPageViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const localeTag = isKo ? "ko-KR" : "en-US";

  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [promptRetention, setPromptRetention] = useState<PromptManifestRetentionStatus | null>(null);
  const [promptRetentionError, setPromptRetentionError] = useState<string | null>(null);
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
    const [healthResult, retentionResult] = await Promise.allSettled([
      getHealth(),
      getPromptManifestRetention(),
    ]);

    if (retentionResult.status === "fulfilled") {
      setPromptRetention(retentionResult.value);
      setPromptRetentionError(null);
    } else {
      setPromptRetentionError(errorMessage(retentionResult.reason));
    }

    if (healthResult.status === "fulfilled") {
      setHealth(healthResult.value);
      setLastSuccessAt(Date.now());
      setError(null);
      setFailureCount(0);
    } else {
      setError(errorMessage(healthResult.reason));
      setFailureCount((current) => current + 1);
    }

    refreshInFlightRef.current = false;
    setIsRefreshing(false);
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
  const primarySignals = useMemo(
    () =>
      signals.filter((signal) =>
        ["deferred_hooks", "outbox_age", "pending_queue", "active_watchers"].includes(signal.key),
      ),
    [signals],
  );
  const recoverySignal = useMemo(
    () => signals.find((signal) => signal.key === "recovery_seconds") ?? null,
    [signals],
  );
  const bottlenecks = useMemo(
    () => (health ? buildBottlenecks(health) : []),
    [health],
  );
  const stale = lastSuccessAt != null && now - lastSuccessAt > STALE_AFTER_MS;
  const providerCount = health?.providers?.length ?? 0;
  const connectedProviders = (health?.providers ?? []).filter((provider) => provider.connected).length;
  const lastUpdatedLabel = formatUpdatedAt(lastSuccessAt, localeTag);
  const restartPendingProviders = (health?.providers ?? []).filter((provider) => provider.restart_pending).length;
  const disconnectedProviders = (health?.providers ?? []).filter((provider) => !provider.connected).length;
  const runtimeSignals = useMemo<RuntimeSignalRow[]>(
    () => [
      {
        key: "websocket",
        label: tr("Live Transport", "Live Transport"),
        value: wsConnected ? "LIVE" : "DOWN",
        hint: wsConnected
          ? tr("WS 이벤트 기반 refresh 활성", "Event-driven refresh active")
          : tr("fallback polling으로 health 유지", "Fallback polling keeps health alive"),
        severity: wsConnected ? "normal" : "danger",
      },
      {
        key: "queue",
        label: tr("Pending Queue", "Pending Queue"),
        value: formatNumber(health?.queue_depth ?? 0),
        hint: tr(
          `active ${formatNumber(health?.global_active ?? 0)} · finalizing ${formatNumber(health?.global_finalizing ?? 0)}`,
          `active ${formatNumber(health?.global_active ?? 0)} · finalizing ${formatNumber(health?.global_finalizing ?? 0)}`,
        ),
        severity: resolveSeverity(health?.queue_depth ?? 0, SIGNAL_THRESHOLDS.pending_queue),
      },
      {
        key: "outbox",
        label: tr("Outbox Pending", "Outbox Pending"),
        value: formatNumber(health?.dispatch_outbox?.pending ?? 0),
        hint: tr(
          `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
          `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
        ),
        severity: resolveSeverity(health?.dispatch_outbox?.pending ?? 0, SIGNAL_THRESHOLDS.pending_queue),
      },
      {
        key: "providers",
        label: tr("Provider Links", "Provider Links"),
        value: `${connectedProviders}/${providerCount}`,
        hint: tr(
          `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
          `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
        ),
        severity: disconnectedProviders > 0 ? (disconnectedProviders >= 2 ? "danger" : "warning") : "normal",
      },
      {
        key: "watchers",
        label: tr("Watchers", "Watchers"),
        value: formatNumber(health?.watcher_count ?? 0),
        hint: tr(
          `${formatNumber(providerCount)}개 provider 추적 중`,
          `${formatNumber(providerCount)} providers in scope`,
        ),
        severity: resolveSeverity(health?.watcher_count ?? 0, SIGNAL_THRESHOLDS.active_watchers),
      },
      {
        key: "recovery",
        label: tr("Recovery Window", "Recovery Window"),
        value: formatDurationCompact(health?.recovery_duration ?? 0),
        hint: tr(
          `uptime ${formatDurationCompact(health?.uptime_secs ?? 0)}`,
          `uptime ${formatDurationCompact(health?.uptime_secs ?? 0)}`,
        ),
        severity: resolveSeverity(health?.recovery_duration ?? 0, SIGNAL_THRESHOLDS.recovery_seconds),
      },
    ],
    [connectedProviders, disconnectedProviders, health, providerCount, restartPendingProviders, tr, wsConnected],
  );
  const statusTone =
    health?.status === "unhealthy"
      ? "danger"
      : health?.status === "degraded"
        ? "warn"
        : "success";
  const promptRetentionTone = promptRetentionError
    ? "warn"
    : promptRetention?.enabled === false
      ? "info"
      : "success";
  const promptRetentionValue = promptRetention
    ? promptRetention.enabled
      ? `${formatNumber(promptRetention.retention_days)}d`
      : tr("꺼짐", "Off")
    : "--";
  const promptRetentionConfigNote = promptRetentionError
    ? tr("retention 상태 요청 실패", "Retention status unavailable")
    : promptRetention
      ? promptRetention.restart_required_for_config_changes
        ? tr(
            "retention config 변경은 재시작 필요 · boot snapshot",
            "restart required for retention config changes · boot snapshot",
          )
        : tr("retention config hot reload 활성", "retention config hot reload enabled")
      : tr("retention 상태 수신 대기", "Waiting for retention status");
  const promptRetentionStorageNote = promptRetention
    ? tr(
        `${formatBytes(promptRetention.total_stored_bytes)} 저장 · layer ${formatNumber(promptRetention.layer_count)} · truncated ${formatNumber(promptRetention.truncated_count)}`,
        `${formatBytes(promptRetention.total_stored_bytes)} stored · ${formatNumber(promptRetention.layer_count)} layers · ${formatNumber(promptRetention.truncated_count)} truncated`,
      )
    : promptRetentionError ?? tr("storage snapshot 없음", "No storage snapshot");

  return (
    <div
      data-testid="ops-page"
      className="page fade-in ops-shell mx-auto h-full w-full min-w-0 overflow-x-hidden overflow-y-auto"
    >
      <style>{OPS_SHELL_STYLES}</style>
      <div className="page fade-in">
        <div className="page-header">
          <div className="min-w-0">
            <div className="page-title">{tr("운영 상태", "Ops Health")}</div>
            <div className="page-sub">
              {tr(
                "Deferred / outbox / queue / watcher / recovery",
                "Deferred / outbox / queue / watcher / recovery",
              )}
            </div>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <span className={chipClassFromTone(wsConnected ? "success" : "danger")}>
              <span className={wsConnected ? "dot pulse" : "dot"} />
              {wsConnected ? "LIVE" : "DISCONNECTED"}
            </span>
            {stale ? <span className="chip warn">STALE</span> : null}
            <button className="btn sm" type="button" onClick={() => void refreshHealth()} disabled={isRefreshing}>
              <RefreshCw size={12} className={isRefreshing ? "animate-spin" : undefined} />
              {isRefreshing ? tr("동기화 중", "Refreshing") : tr("새로고침", "Refresh")}
            </button>
          </div>
        </div>

        <div className="mb-4 flex flex-wrap items-center gap-2">
          <span className={chipClassFromTone(statusTone)}>
            {health ? translateStatus(health.status, isKo) : tr("대기 중", "Pending")}
          </span>
          <span className="chip">{tr(`업데이트 ${lastUpdatedLabel}`, `Updated ${lastUpdatedLabel}`)}</span>
          {recoverySignal ? (
            <span
              className={chipClassFromTone(toneForSeverity(recoverySignal.severity))}
              data-testid="ops-signal-recovery_seconds"
            >
              {tr(`복구 ${recoverySignal.value}`, `Recovery ${recoverySignal.value}`)}
            </span>
          ) : null}
        </div>

        {error ? (
          <div className="card ops-inline-alert">
            <div className="card-body flex items-start gap-3">
              <AlertTriangle size={16} style={{ color: "var(--th-accent-warn)", flexShrink: 0, marginTop: 2 }} />
              <div className="min-w-0">
                <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                  {health
                    ? tr("최근 health 요청이 실패해 마지막 정상값을 유지 중입니다.", "Latest health request failed, keeping the last successful snapshot.")
                    : tr("health 응답을 아직 받지 못했습니다.", "Health response has not arrived yet.")}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {error}
                </div>
              </div>
            </div>
          </div>
        ) : null}

        {health?.degraded_reasons && health.degraded_reasons.length > 0 ? (
          <div className="mb-4 flex flex-wrap gap-2">
            {health.degraded_reasons.slice(0, 4).map((reason) => (
              <span
                key={reason}
                className={health.status === "unhealthy" ? "chip err" : "chip warn"}
              >
                {describeDegradedReason(reason)}
              </span>
            ))}
          </div>
        ) : null}

        <div data-testid="ops-signal-grid" className="grid grid-4">
          {primarySignals.length > 0 ? (
            primarySignals.map((signal) => {
              const chrome = surfaceStyleForSeverity(signal.severity);
              return (
                <div
                  key={signal.key}
                  data-testid={`ops-signal-${signal.key}`}
                  className="card ops-signal-card"
                  style={{
                    borderColor: chrome.borderColor,
                    background: chrome.background,
                  }}
                >
                  <div className="card-body">
                    <div className="metric-label">{signal.label}</div>
                    <div className="metric-value" style={{ color: chrome.valueColor }}>
                      {signal.value}
                    </div>
                    <div className="metric-sub">{signal.note}</div>
                  </div>
                </div>
              );
            })
          ) : (
            <div className="card md:col-span-2 xl:col-span-4">
              <div className="card-body">
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
            </div>
          )}
        </div>

        <div className="grid ops-main-grid mt-4">
          <div className="card">
            <div className="card-head">
              <div className="min-w-0">
                <div className="card-title">{tr("운영 시그널", "Ops Signals")}</div>
                <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    "세션 · 리뷰 · 블록 · 회의 · 후속 — 한 줄 요약",
                    "Session · review · block · meetings · follow-up — quick summary",
                  )}
                </div>
              </div>
            </div>
            <div className="card-body">
              <div className="grid sm:grid-cols-2 xl:grid-cols-3">
                {runtimeSignals.map((signal) => {
                  const chrome = surfaceStyleForSeverity(signal.severity);
                  return (
                    <div
                      key={signal.key}
                      className="ops-panel-card"
                      style={{
                        borderColor: chrome.borderColor,
                        background: chrome.background,
                      }}
                    >
                      <div className="metric-label">
                        {signal.label}
                      </div>
                      <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: chrome.valueColor }}>
                        {signal.value}
                      </div>
                      <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                        {signal.hint}
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>
          </div>

          <div className="card">
            <div className="card-head">
              <div className="min-w-0">
                <div className="card-title">{tr("회의 타임라인", "Meeting Timeline")}</div>
                <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr("0 진행 · 후속 0 미정리", "0 active · 0 follow-ups pending")}
                </div>
              </div>
              <button className="btn sm" type="button" disabled>
                {tr("회의록", "Records")}
              </button>
            </div>
            <div className="card-body">
              <div className="flex flex-wrap gap-2">
                {recoverySignal ? (
                  <span className={chipClassFromTone(toneForSeverity(recoverySignal.severity))}>
                    {tr(`복구 ${recoverySignal.value}`, `Recovery ${recoverySignal.value}`)}
                  </span>
                ) : null}
                <span className={chipClassFromTone(wsConnected ? "success" : "danger")}>
                  {tr(`provider ${connectedProviders}/${providerCount}`, `providers ${connectedProviders}/${providerCount}`)}
                </span>
              </div>
              <div className="mt-4 min-h-[220px]">
                <SurfaceEmptyState className="grid min-h-[220px] place-items-center py-10">
                  <div className="flex flex-col items-center gap-2 text-center">
                    <Wifi size={20} style={{ color: "var(--th-text-muted)" }} />
                    <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                      {tr("최근 회의가 없습니다.", "No recent meetings.")}
                    </div>
                    <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {tr("회의 타임라인 데이터가 연결되면 이 영역이 채워집니다.", "This area will populate once meeting timeline data is wired in.")}
                    </div>
                  </div>
                </SurfaceEmptyState>
              </div>
            </div>
          </div>
        </div>

        <div className="grid ops-secondary-grid mt-4">
          <div className="card">
            <div className="card-head">
              <div className="min-w-0">
                <div className="card-title">{tr("Runtime Watchlist", "Runtime Watchlist")}</div>
                <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    "warning 이상으로 올라온 런타임 병목을 별도 목록으로 유지합니다.",
                    "Keeps a dedicated list of runtime bottlenecks that are currently warning level or above.",
                  )}
                </div>
              </div>
            </div>
            <div className="card-body">
              {bottlenecks.length > 0 ? (
                <div data-testid="ops-bottlenecks" className="space-y-2">
                  {bottlenecks.map((row) => (
                    <div
                      key={`${row.kind}-${row.detail}`}
                      data-testid={`ops-bottleneck-${row.kind}`}
                      className="grid gap-3 rounded-2xl border px-3 py-3 md:grid-cols-[minmax(0,1fr)_auto] md:items-center"
                      style={{
                        borderColor: row.severity === "danger" ? "var(--color-danger-border)" : "var(--color-warning-border)",
                        background: row.severity === "danger" ? "var(--color-danger-soft)" : "var(--color-warning-soft)",
                      }}
                    >
                      <div className="min-w-0">
                        <div className="flex flex-wrap items-center gap-2">
                          <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                            {formatBottleneckLabel(row.kind)}
                          </div>
                          <span className={chipClassFromTone(toneForSeverity(row.severity))}>
                            {row.severity.toUpperCase()}
                          </span>
                        </div>
                        <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                          {row.detail}
                        </div>
                      </div>
                      <div className="text-sm font-semibold" style={{ color: "var(--th-text-primary)" }}>
                        {formatNumber(row.count)}
                      </div>
                    </div>
                  ))}
                </div>
              ) : (
                <SurfaceEmptyState data-testid="ops-bottlenecks-empty" className="py-8">
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
            </div>
          </div>

          <OpsConnectionPanel
            wsConnected={wsConnected}
            health={health}
            connectedProviders={connectedProviders}
            providerCount={providerCount}
            disconnectedProviders={disconnectedProviders}
            restartPendingProviders={restartPendingProviders}
            promptRetention={promptRetention}
            promptRetentionError={promptRetentionError}
            promptRetentionTone={promptRetentionTone}
            promptRetentionValue={promptRetentionValue}
            promptRetentionConfigNote={promptRetentionConfigNote}
            promptRetentionStorageNote={promptRetentionStorageNote}
            tr={tr}
          />
        </div>

        <div className="card mt-4" data-testid="ops-control-handoff">
          <div className="card-head">
            <div className="min-w-0">
              <div className="card-title">{tr("관리 표면", "Management surfaces")}</div>
              <div className="ops-copy mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "조직 편집은 전용 화면으로 보내고, 이 페이지는 health와 전달 상태만 유지합니다.",
                  "Organization edits move to dedicated screens so this page stays focused on health and delivery.",
                )}
              </div>
            </div>
          </div>
          <div className="card-body">
            <div className="ops-handoff-grid">
              <Link
                to="/agents"
                className="ops-handoff-link"
                data-testid="ops-handoff-agents"
              >
                <span className="block truncate text-sm font-semibold">
                  {tr("에이전트·부서", "Agents & departments")}
                </span>
                <span className="ops-copy block text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr("조직 구조와 파견 세션 관리", "Organization and dispatch-session management")}
                </span>
              </Link>
              <Link
                to="/office"
                className="ops-handoff-link"
                data-testid="ops-handoff-office"
              >
                <span className="block truncate text-sm font-semibold">
                  {tr("오피스", "Office")}
                </span>
                <span className="ops-copy block text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr("공간, 좌석, 실시간 에이전트 보기", "Space, seats, and live agent view")}
                </span>
              </Link>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
