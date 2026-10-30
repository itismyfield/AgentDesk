import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, RefreshCw, Wifi } from "lucide-react";
import { Link } from "react-router";
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
import ClusterNodesPanel from "./cluster/ClusterNodesPanel";
import {
  DISCONNECTED_POLL_BASE_MS,
  LIVE_POLL_INTERVAL_MS,
  MAX_DISCONNECTED_POLL_MS,
  SIGNAL_THRESHOLDS,
  STALE_AFTER_MS,
  WS_REFRESH_DEBOUNCE_MS,
  buildBottlenecks,
  buildSignalCards,
  errorMessage,
  formatBottleneckLabel,
  formatBytes,
  formatDurationCompact,
  formatNumber,
  opsToneToHealth,
  resolveSeverity,
  surfaceStyleForSeverity,
  toneForSeverity,
  translateStatus,
  type RuntimeSignalRow,
} from "./OpsPageModel";
import { StatusBadge } from "./common/StatusBadge";
import { FreshnessIndicator } from "./common/FreshnessIndicator";


interface OpsPageViewProps {
  wsConnected: boolean;
  isKo: boolean;
}

export default function OpsPageView({
  wsConnected,
  isKo,
}: OpsPageViewProps) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);

  const [health, setHealth] = useState<HealthResponse | null>(null);
  const [promptRetention, setPromptRetention] = useState<PromptManifestRetentionStatus | null>(null);
  const [promptRetentionError, setPromptRetentionError] = useState<string | null>(null);
  const [lastSuccessAt, setLastSuccessAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [failureCount, setFailureCount] = useState(0);
  const [now, setNow] = useState(() => Date.now());
  const refreshInFlightRef = useRef(false);
  // Track mount status so async refresh callbacks that resolve after the Ops
  // route is unmounted don't write to state and leak warnings.
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const refreshHealth = useCallback(async () => {
    if (refreshInFlightRef.current) return;
    if (!mountedRef.current) return;
    refreshInFlightRef.current = true;
    setIsRefreshing(true);
    const [healthResult, retentionResult] = await Promise.allSettled([
      getHealth(),
      getPromptManifestRetention(),
    ]);

    if (!mountedRef.current) {
      refreshInFlightRef.current = false;
      return;
    }

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
  const restartPendingProviders = (health?.providers ?? []).filter((provider) => provider.restart_pending).length;
  const disconnectedProviders = (health?.providers ?? []).filter((provider) => !provider.connected).length;
  const runtimeSignals = useMemo<RuntimeSignalRow[]>(
    () => [
      {
        key: "websocket",
        label: tr("실시간 연결", "Live Transport"),
        value: wsConnected ? "LIVE" : "DOWN",
        hint: wsConnected
          ? tr("WS 이벤트 기반 refresh 활성", "Event-driven refresh active")
          : tr("fallback polling으로 health 유지", "Fallback polling keeps health alive"),
        severity: wsConnected ? "normal" : "danger",
      },
      {
        key: "queue",
        label: tr("처리 대기", "Pending Queue"),
        value: formatNumber(health?.queue_depth ?? 0),
        hint: tr(
          `active ${formatNumber(health?.global_active ?? 0)} · finalizing ${formatNumber(health?.global_finalizing ?? 0)}`,
          `active ${formatNumber(health?.global_active ?? 0)} · finalizing ${formatNumber(health?.global_finalizing ?? 0)}`,
        ),
        severity: resolveSeverity(health?.queue_depth ?? 0, SIGNAL_THRESHOLDS.pending_queue),
      },
      {
        key: "outbox",
        label: tr("전달 대기", "Outbox Pending"),
        value: formatNumber(health?.dispatch_outbox?.pending ?? 0),
        hint: tr(
          `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
          `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
        ),
        severity: resolveSeverity(health?.dispatch_outbox?.pending ?? 0, SIGNAL_THRESHOLDS.pending_queue),
      },
      {
        key: "providers",
        label: tr("프로바이더 연결", "Provider Links"),
        value: `${connectedProviders}/${providerCount}`,
        hint: tr(
          `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
          `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
        ),
        severity: disconnectedProviders > 0 ? (disconnectedProviders >= 2 ? "danger" : "warning") : "normal",
      },
      {
        key: "watchers",
        label: tr("감시 중", "Watchers"),
        value: formatNumber(health?.watcher_count ?? 0),
        hint: tr(
          `${formatNumber(providerCount)}개 provider 추적 중`,
          `${formatNumber(providerCount)} providers in scope`,
        ),
        severity: resolveSeverity(health?.watcher_count ?? 0, SIGNAL_THRESHOLDS.active_watchers),
      },
      {
        key: "recovery",
        label: tr("복구 시간", "Recovery Window"),
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
    <div data-testid="ops-page" className="h-full min-w-0 overflow-y-auto px-4 pb-8 pt-5 sm:px-6 sm:pt-7">
      <div className="mx-auto max-w-[88rem] space-y-6 [overflow-wrap:anywhere]">
        <header className="flex flex-wrap items-start justify-between gap-4 border-b border-th-border-subtle pb-5">
          <div className="min-w-0">
            <h1 className="text-2xl font-semibold tracking-tight text-th-text-heading">{tr("운영 상태", "Ops Health")}</h1>
            <p className="mt-2 text-xs leading-5 text-th-text-muted">{tr("처리 대기와 전달 흐름, 실행 장비의 상태를 확인합니다.", "Monitor pending work, delivery and execution devices.")}</p>
          </div>
          <div className="flex flex-wrap items-center gap-3">
            <StatusBadge tone={wsConnected ? "healthy" : "critical"} size="sm" pulse={wsConnected}
              title={wsConnected ? tr("실시간 WS 연결", "Live WS connection") : tr("WS 끊김", "WS disconnected")}>
              {wsConnected ? "LIVE" : "DISCONNECTED"}
            </StatusBadge>
            {stale ? <StatusBadge tone="warning" size="sm" title={tr("최근 health 데이터가 stale 임계값을 초과했습니다", "Health data exceeded the stale threshold")}>STALE</StatusBadge> : null}
            <button className="inline-flex min-h-11 items-center justify-center gap-2 rounded-md border border-th-border px-3 py-2 text-sm text-th-text-secondary hover:bg-surface-subtle disabled:opacity-50" type="button" onClick={() => void refreshHealth()} disabled={isRefreshing}>
              <RefreshCw size={14} className={isRefreshing ? "animate-spin" : undefined} />
              {isRefreshing ? tr("동기화 중", "Refreshing") : tr("새로고침", "Refresh")}
            </button>
          </div>
        </header>

        <section aria-label={tr("현재 상태", "Current health")} className="space-y-4">
          <div className="flex flex-wrap items-center gap-3">
            <StatusBadge tone={health ? opsToneToHealth(statusTone) : "unknown"} size="sm">{health ? translateStatus(health.status, isKo) : tr("응답 대기", "Pending")}</StatusBadge>
            <FreshnessIndicator timestamp={lastSuccessAt} label={tr("업데이트", "Updated")} staleAfterSeconds={STALE_AFTER_MS / 1000} criticalAfterSeconds={(STALE_AFTER_MS * 3) / 1000} />
            {recoverySignal ? <StatusBadge tone={opsToneToHealth(toneForSeverity(recoverySignal.severity))} size="sm" title={`recovery_seconds: ${recoverySignal.value}`}>
              {tr(`복구 ${recoverySignal.value}`, `Recovery ${recoverySignal.value}`)}
            </StatusBadge> : null}
          </div>
          {error ? <div role="alert" className="flex items-start gap-3 rounded-lg border border-th-border-subtle bg-th-bg-surface p-4">
            <AlertTriangle size={16} className="mt-0.5 shrink-0 text-[var(--th-badge-amber-text)]" />
            <div className="min-w-0">
              <p className="text-sm font-medium text-th-text-primary">{health ? tr("최근 health 요청이 실패해 마지막 정상값을 유지 중입니다.", "Latest health request failed, keeping the last successful snapshot.") : tr("health 응답을 아직 받지 못했습니다.", "Health response has not arrived yet.")}</p>
              <p className="mt-1 text-xs leading-5 text-th-text-muted">{error}</p>
            </div>
          </div> : null}
          {health?.degraded_reasons && health.degraded_reasons.length > 0 ? <div className="flex flex-wrap gap-2">
            {health.degraded_reasons.slice(0, 4).map(reason => <span key={reason} className={`rounded-md border border-th-border-subtle px-2 py-1 text-xs leading-5 ${health.status === "unhealthy" ? "text-[var(--th-accent-danger)]" : "text-[var(--th-badge-amber-text)]"}`}>{describeDegradedReason(reason)}</span>)}
          </div> : null}
          <div data-testid="ops-signal-grid" className="grid grid-cols-2 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface sm:grid-cols-4">
            {primarySignals.length > 0 ? primarySignals.map(signal => {
              const chrome = surfaceStyleForSeverity(signal.severity);
              return <div key={signal.key} data-testid={`ops-signal-${signal.key}`} className="min-w-0 border-b border-r border-th-border-subtle p-4 even:border-r-0 sm:border-b-0 sm:even:border-r sm:last:border-r-0">
                <p className="text-xs text-th-text-muted">{signal.label}</p>
                <p className="mt-2 text-2xl font-semibold tabular-nums tracking-tight" style={{ color: chrome.valueColor }}>{signal.value}</p>
                <p className="mt-2 text-xs leading-5 text-th-text-muted">{signal.note}</p>
              </div>;
            }) : <SurfaceEmptyState className="col-span-2 rounded-none border-0 px-4 py-5 text-sm sm:col-span-4">
              <p className="font-medium text-th-text-primary">{tr("표시할 health signal이 아직 없습니다.", "No health signals available yet.")}</p>
              <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("초기 health 응답이 도착하면 상태 요약을 표시합니다.", "The health summary appears after the first response arrives.")}</p>
            </SurfaceEmptyState>}
          </div>
        </section>

        <section aria-labelledby="ops-watchlist-title" className="min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface">
          <div className="border-b border-th-border-subtle px-4 py-4 sm:px-5">
            <h2 id="ops-watchlist-title" className="text-sm font-semibold text-th-text-primary">{tr("확인이 필요한 항목", "Runtime Watchlist")}</h2>
            <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("주의 수준 이상의 런타임 병목입니다.", "Runtime bottlenecks at warning level or above.")}</p>
          </div>
          {bottlenecks.length > 0 ? <div data-testid="ops-bottlenecks" className="divide-y divide-th-border-subtle px-4 sm:px-5">
            {bottlenecks.map(row => <div key={`${row.kind}-${row.detail}`} data-testid={`ops-bottleneck-${row.kind}`} className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-start gap-4 py-4">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2"><h3 className="text-sm font-medium text-th-text-primary">{formatBottleneckLabel(row.kind)}</h3><StatusBadge tone={opsToneToHealth(toneForSeverity(row.severity))} size="xs">{row.severity.toUpperCase()}</StatusBadge></div>
                <p className="mt-1 text-xs leading-5 text-th-text-muted">{row.detail}</p>
              </div>
              <span className="text-lg font-semibold tabular-nums text-th-text-primary">{formatNumber(row.count)}</span>
            </div>)}
          </div> : <SurfaceEmptyState data-testid="ops-bottlenecks-empty" className="rounded-none border-0 px-4 py-5 sm:px-5">
            <p className="text-sm font-medium text-th-text-primary">{health ? tr("현재 감지된 운영 병목이 없습니다.", "No active ops bottlenecks detected.") : tr("병목 상태를 확인 중입니다.", "Waiting for bottleneck status.")}</p>
            <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("주의 수준 이상의 신호가 생기면 여기에 표시합니다.", "Signals at warning level or above appear here.")}</p>
          </SurfaceEmptyState>}
        </section>

        <div className="grid min-w-0 gap-5 xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
          <section aria-labelledby="ops-runtime-title" className="min-w-0 self-start rounded-lg border border-th-border-subtle bg-th-bg-surface">
            <div className="border-b border-th-border-subtle px-4 py-4 sm:px-5">
              <h2 id="ops-runtime-title" className="text-sm font-semibold text-th-text-primary">{tr("운영 시그널", "Ops Signals")}</h2>
              <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("연결, 대기열, 감시와 복구 상태를 함께 확인합니다.", "Connectivity, queues, watchers and recovery status.")}</p>
            </div>
            <dl className="divide-y divide-th-border-subtle px-4 sm:px-5">
              {runtimeSignals.map(signal => <div key={signal.key} className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] gap-x-3 gap-y-1 py-4">
                <dt className="text-xs text-th-text-muted">{signal.label}</dt>
                <dd className="row-span-2 text-lg font-semibold tabular-nums" style={{ color: surfaceStyleForSeverity(signal.severity).valueColor }}>{health || signal.key === "websocket" ? signal.value : "—"}</dd>
                <dd className="text-xs leading-5 text-th-text-muted">{health || signal.key === "websocket" ? signal.hint : tr("health 응답 대기", "Awaiting health response")}</dd>
              </div>)}
            </dl>
          </section>
          <OpsConnectionPanel wsConnected={wsConnected} lastHealthAt={lastSuccessAt} health={health} connectedProviders={connectedProviders} providerCount={providerCount} disconnectedProviders={disconnectedProviders} restartPendingProviders={restartPendingProviders} promptRetention={promptRetention} promptRetentionError={promptRetentionError} promptRetentionTone={promptRetentionTone} promptRetentionValue={promptRetentionValue} promptRetentionConfigNote={promptRetentionConfigNote} promptRetentionStorageNote={promptRetentionStorageNote} tr={tr} />
        </div>

        <ClusterNodesPanel isKo={isKo} />

        <section aria-labelledby="ops-meeting-title" className="rounded-lg border border-th-border-subtle bg-th-bg-surface p-4 sm:p-5">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div><h2 id="ops-meeting-title" className="text-sm font-semibold text-th-text-primary">{tr("회의 타임라인", "Meeting Timeline")}</h2><p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("0 진행 · 후속 0 미정리", "0 active · 0 follow-ups pending")}</p></div>
            <button className="min-h-11 rounded-md border border-th-border px-3 py-2 text-sm text-th-text-muted opacity-50" type="button" disabled>{tr("회의록", "Records")}</button>
          </div>
          <div className="mt-3 flex flex-wrap gap-2">
            {recoverySignal ? <StatusBadge tone={opsToneToHealth(toneForSeverity(recoverySignal.severity))} size="sm">{tr(`복구 ${recoverySignal.value}`, `Recovery ${recoverySignal.value}`)}</StatusBadge> : null}
            <StatusBadge tone={wsConnected ? "healthy" : "critical"} size="sm">{tr(`provider ${connectedProviders}/${providerCount}`, `providers ${connectedProviders}/${providerCount}`)}</StatusBadge>
          </div>
          <SurfaceEmptyState className="mt-4 rounded-md px-4 py-4"><div className="flex items-start gap-3"><Wifi size={16} className="mt-0.5 shrink-0 text-th-text-muted" /><div><p className="text-sm font-medium text-th-text-primary">{tr("최근 회의가 없습니다.", "No recent meetings.")}</p><p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("회의 타임라인 데이터가 연결되면 이 영역이 채워집니다.", "This area will populate once meeting timeline data is wired in.")}</p></div></div></SurfaceEmptyState>
        </section>

        <section data-testid="ops-control-handoff" aria-labelledby="ops-handoff-title" className="border-t border-th-border-subtle pt-5">
          <h2 id="ops-handoff-title" className="text-sm font-semibold text-th-text-primary">{tr("관리 화면", "Management surfaces")}</h2>
          <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("운영 상태를 확인한 뒤 전용 화면에서 설정을 조정합니다.", "Use the dedicated screens to manage settings after reviewing health.")}</p>
          <div className="mt-4 grid gap-3 sm:grid-cols-3">
            {[{to:"/agents",testId:"ops-handoff-agents",title:tr("에이전트·부서", "Agents & departments"),detail:tr("조직 구조와 파견 세션 관리", "Organization and dispatch-session management")},{to:"/settings?settingsPanel=machine",title:tr("머신 설정", "Machine settings"),detail:tr("장치 자원과 연결 정보 확인", "Device resources and connection details")},{to:"/settings?settingsPanel=runtime",title:tr("런타임 설정", "Runtime settings"),detail:tr("대기열·알림·캐시 임계값 조정", "Queue, alert and cache thresholds")}].map(link=><Link key={link.to} to={link.to} data-testid={link.testId} className="flex min-h-20 min-w-0 flex-col justify-center gap-1 rounded-lg border border-th-border-subtle px-4 py-3 hover:bg-surface-subtle"><span className="text-sm font-medium text-th-text-primary">{link.title}</span><span className="text-xs leading-5 text-th-text-muted">{link.detail}</span></Link>)}
          </div>
        </section>
      </div>
    </div>
  );
}
