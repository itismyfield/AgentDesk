import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, RefreshCw } from "lucide-react";
import { Link } from "react-router";
import {
  getHealthDetail,
  getPromptManifestRetention,
  type HealthResponse,
  type PromptManifestRetentionStatus,
} from "../api";
import type { WSEvent } from "../types";
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
  formatBytes,
  formatDurationCompact,
  formatNumber,
  opsToneToHealth,
  surfaceStyleForSeverity,
  toneForSeverity,
  translateStatus,
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
      getHealthDetail(),
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

    // Background tabs skip polling; the visibility listener below catches up on return.
    const timer = window.setInterval(() => {
      if (!document.hidden) void refreshHealth();
    }, intervalMs);
    const onVisible = () => {
      if (!document.hidden) void refreshHealth();
    };
    document.addEventListener("visibilitychange", onVisible);

    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisible);
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
    () => (health ? buildBottlenecks(health, isKo) : []),
    [health, isKo],
  );
  const stale = lastSuccessAt != null && now - lastSuccessAt > STALE_AFTER_MS;
  const providerCount = health?.providers?.length ?? 0;
  const connectedProviders = (health?.providers ?? []).filter((provider) => provider.connected).length;
  const restartPendingProviders = (health?.providers ?? []).filter((provider) => provider.restart_pending).length;
  const disconnectedProviders = (health?.providers ?? []).filter((provider) => !provider.connected).length;
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
      ? tr(`${formatNumber(promptRetention.retention_days)}일`, `${formatNumber(promptRetention.retention_days)}d`)
      : tr("꺼짐", "Off")
    : "--";
  const promptRetentionConfigNote = promptRetentionError
    ? tr("보관 상태를 불러오지 못했어요", "Retention status unavailable")
    : promptRetention
      ? promptRetention.restart_required_for_config_changes
        ? tr("보관 설정을 바꾸면 서버를 다시 시작해야 반영돼요", "Changing retention settings needs a server restart")
        : tr("보관 설정을 바꾸면 바로 반영돼요", "Retention setting changes apply immediately")
      : tr("보관 상태를 확인하는 중", "Waiting for retention status");
  const promptRetentionStorageNote = promptRetention
    ? tr(
        `${formatBytes(promptRetention.total_stored_bytes)} 저장 · 조각 ${formatNumber(promptRetention.layer_count)}개 · 잘린 조각 ${formatNumber(promptRetention.truncated_count)}개`,
        `${formatBytes(promptRetention.total_stored_bytes)} stored · ${formatNumber(promptRetention.layer_count)} layers · ${formatNumber(promptRetention.truncated_count)} truncated`,
      )
    : promptRetentionError ?? tr("저장 현황 없음", "No storage snapshot");

  return (
    <div data-testid="ops-page" className="h-full min-w-0 overflow-y-auto px-4 pb-8 pt-5 sm:px-6 sm:pt-7">
      <div className="mx-auto max-w-[88rem] space-y-6 [overflow-wrap:anywhere]">
        <header className="flex flex-wrap items-start justify-between gap-4 border-b border-th-border-subtle pb-5">
          <div className="min-w-0">
            <h1 className="text-2xl font-semibold tracking-tight text-th-text-heading">{tr("운영", "Ops")}</h1>
            <p className="mt-2 text-xs leading-5 text-th-text-muted">{tr("처리 대기와 전달 흐름, 실행 장비 상태를 봐요.", "Pending work, delivery flow and execution machines.")}</p>
          </div>
          <div className="flex flex-wrap items-center gap-3">
            <StatusBadge tone={wsConnected ? "healthy" : "critical"} size="sm"
              title={wsConnected ? tr("이벤트를 실시간으로 받고 있어요", "Receiving live events") : tr("실시간 연결이 끊겨 주기적으로 확인 중이에요", "Live connection lost; polling instead")}>
              {wsConnected ? tr("실시간 연결", "Live") : tr("연결 끊김", "Disconnected")}
            </StatusBadge>
            {stale ? <StatusBadge tone="warning" size="sm" title={tr("상태 정보가 한동안 갱신되지 않았어요", "Health data has not refreshed for a while")}>{tr("갱신 늦음", "Stale")}</StatusBadge> : null}
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
            {recoverySignal ? <StatusBadge tone={opsToneToHealth(toneForSeverity(recoverySignal.severity))} size="sm" title={tr("재시작 뒤 이전 작업을 되살리는 데 걸린 시간", "Time spent restoring work after the last restart")}>
              {tr(`복구 ${recoverySignal.value}`, `Recovery ${recoverySignal.value}`)}
            </StatusBadge> : null}
            {health?.uptime_secs ? <span className="text-xs text-th-text-muted">{tr(`가동 ${formatDurationCompact(health.uptime_secs, true)}`, `Up ${formatDurationCompact(health.uptime_secs)}`)}</span> : null}
          </div>
          {error ? <div role="alert" className="flex items-start gap-3 rounded-lg border border-th-border-subtle bg-th-bg-surface p-4">
            <AlertTriangle size={16} className="mt-0.5 shrink-0 text-[var(--th-badge-amber-text)]" />
            <div className="min-w-0">
              <p className="text-sm font-medium text-th-text-primary">{health ? tr("최근 상태 요청이 실패해 마지막으로 받은 값을 보여주고 있어요.", "Latest health request failed, keeping the last successful snapshot.") : tr("health 응답을 아직 받지 못했습니다.", "Health response has not arrived yet.")}</p>
              <p className="mt-1 text-xs leading-5 text-th-text-muted">{error}</p>
            </div>
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
              <p className="font-medium text-th-text-primary">{tr("아직 상태 정보가 없어요.", "No health signals available yet.")}</p>
              <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("첫 응답이 오면 요약을 보여드릴게요.", "The health summary appears after the first response arrives.")}</p>
            </SurfaceEmptyState>}
          </div>
        </section>

        <section aria-labelledby="ops-watchlist-title" className="min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface">
          <div className="border-b border-th-border-subtle px-4 py-4 sm:px-5">
            <h2 id="ops-watchlist-title" className="text-sm font-semibold text-th-text-primary">{tr("확인이 필요한 항목", "Needs attention")}</h2>
            <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("주의 이상인 신호만 모았어요.", "Signals at warning level or above.")}</p>
          </div>
          {bottlenecks.length > 0 ? <div data-testid="ops-bottlenecks" className="divide-y divide-th-border-subtle px-4 sm:px-5">
            {bottlenecks.map(row => <div key={`${row.kind}-${row.detail}`} data-testid={`ops-bottleneck-${row.kind}`} className="grid min-w-0 grid-cols-[minmax(0,1fr)_auto] items-start gap-4 py-4">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2"><h3 className="text-sm font-medium text-th-text-primary" title={row.kind}>{row.label}</h3><StatusBadge tone={opsToneToHealth(toneForSeverity(row.severity))} size="xs">{row.severity === "danger" ? tr("위험", "Critical") : tr("주의", "Warning")}</StatusBadge></div>
                <p className="mt-1 text-xs leading-5 text-th-text-muted">{row.detail}</p>
              </div>
              <span className="text-lg font-semibold tabular-nums text-th-text-primary">{formatNumber(row.count)}</span>
            </div>)}
          </div> : <SurfaceEmptyState data-testid="ops-bottlenecks-empty" className="rounded-none border-0 px-4 py-5 sm:px-5">
            <p className="text-sm font-medium text-th-text-primary">{health ? tr("지금 확인할 항목이 없어요.", "Nothing needs attention.") : tr("상태를 확인하는 중이에요.", "Waiting for status.")}</p>
            <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("주의 이상인 신호가 생기면 여기에 보여요.", "Signals at warning level or above appear here.")}</p>
          </SurfaceEmptyState>}
        </section>

        <OpsConnectionPanel wsConnected={wsConnected} lastHealthAt={lastSuccessAt} health={health} connectedProviders={connectedProviders} providerCount={providerCount} disconnectedProviders={disconnectedProviders} restartPendingProviders={restartPendingProviders} promptRetention={promptRetention} promptRetentionError={promptRetentionError} promptRetentionTone={promptRetentionTone} promptRetentionValue={promptRetentionValue} promptRetentionConfigNote={promptRetentionConfigNote} promptRetentionStorageNote={promptRetentionStorageNote} tr={tr} />

        <ClusterNodesPanel isKo={isKo} />

        <section data-testid="ops-control-handoff" aria-labelledby="ops-handoff-title" className="border-t border-th-border-subtle pt-5">
          <h2 id="ops-handoff-title" className="text-sm font-semibold text-th-text-primary">{tr("관련 화면", "Related screens")}</h2>
          <p className="mt-1 text-xs leading-5 text-th-text-muted">{tr("상태를 본 뒤 설정은 여기서 바꿔요.", "Adjust settings from these screens.")}</p>
          <div className="mt-4 grid gap-3 sm:grid-cols-3">
            {[{to:"/agents",testId:"ops-handoff-agents",title:tr("에이전트·부서", "Agents & departments"),detail:tr("에이전트·부서·실행 중 세션 관리", "Agents, departments and live sessions")},{to:"/settings?settingsPanel=machine",title:tr("머신 설정", "Machine settings"),detail:tr("장치 자원과 연결 정보 확인", "Device resources and connection details")},{to:"/settings?settingsPanel=workflow",title:tr("작업 흐름 설정", "Workflow settings"),detail:tr("경고 시간·재시도·자동 진행 조정", "Warning times, retries and auto-run")}].map(link=><Link key={link.to} to={link.to} data-testid={link.testId} className="flex min-h-20 min-w-0 flex-col justify-center gap-1 rounded-lg border border-th-border-subtle px-4 py-3 hover:bg-surface-subtle"><span className="text-sm font-medium text-th-text-primary">{link.title}</span><span className="text-xs leading-5 text-th-text-muted">{link.detail}</span></Link>)}
          </div>
        </section>
      </div>
    </div>
  );
}
