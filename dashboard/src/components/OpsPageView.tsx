import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Database, RefreshCw, Wifi } from "lucide-react";
import {
  getHealth,
  getPromptManifestRetention,
  type HealthResponse,
  type PromptManifestRetentionStatus,
} from "../api";
import type { Agent, Office, WSEvent } from "../types";
import OfficeManagerView from "./OfficeManagerView";
import { describeDegradedReason } from "./dashboard/HealthWidget";
import { SurfaceEmptyState } from "./common/SurfacePrimitives";

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

interface RuntimeSignalRow {
  key: string;
  label: string;
  value: string;
  hint: string;
  severity: SignalSeverity;
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

const OPS_SHELL_STYLES = `
  .ops-shell .page {
    padding: 24px 28px 48px;
    max-width: 1440px;
    width: 100%;
    margin: 0 auto;
    min-width: 0;
  }

  .ops-shell .page-header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 24px;
  }

  .ops-shell .page-title {
    font-family: var(--font-display);
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.5px;
    line-height: 1.2;
    color: var(--th-text-heading);
  }

  .ops-shell .page-sub {
    margin-top: 4px;
    font-size: 13px;
    color: var(--th-text-muted);
    line-height: 1.6;
  }

  .ops-shell .grid {
    display: grid;
    gap: 14px;
  }

  .ops-shell .grid-4 {
    grid-template-columns: repeat(4, minmax(0, 1fr));
  }

  .ops-shell .ops-main-grid {
    grid-template-columns: minmax(0, 2fr) minmax(320px, 1fr);
  }

  .ops-shell .ops-secondary-grid {
    grid-template-columns: minmax(0, 1fr) minmax(320px, 1fr);
  }

  .ops-shell .card {
    background:
      linear-gradient(
        180deg,
        color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%,
        color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%
      );
    border: 1px solid color-mix(in srgb, var(--th-border-subtle) 88%, transparent);
    border-radius: 18px;
    overflow: hidden;
    box-shadow: 0 1px 0 color-mix(in srgb, var(--th-text-primary) 4%, transparent) inset;
  }

  .ops-shell .card-head {
    padding: 14px 16px 0;
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }

  .ops-shell .card-title {
    font-size: 12.5px;
    font-weight: 500;
    color: var(--th-text-secondary);
    letter-spacing: -0.1px;
  }

  .ops-shell .card-body {
    padding: 10px 16px 16px;
  }

  .ops-shell .btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 12px;
    border-radius: 7px;
    font-size: 12.5px;
    font-weight: 500;
    color: var(--th-text-secondary);
    background: color-mix(in srgb, var(--th-surface-alt) 84%, transparent);
    border: 1px solid var(--th-border-subtle);
    transition: background 0.14s ease, color 0.14s ease, border-color 0.14s ease;
  }

  .ops-shell .btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--th-surface-alt) 94%, transparent);
    color: var(--th-text-primary);
    border-color: var(--th-border);
  }

  .ops-shell .btn:disabled {
    opacity: 0.58;
    cursor: default;
  }

  .ops-shell .btn.sm {
    padding: 4px 9px;
    font-size: 11.5px;
  }

  .ops-shell .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--th-border-subtle);
    background: color-mix(in srgb, var(--th-surface-alt) 86%, transparent);
    color: var(--th-text-secondary);
    font-size: 11px;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
  }

  .ops-shell .chip .dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    background: currentColor;
  }

  .ops-shell .chip.ok {
    color: var(--color-success);
    border-color: var(--color-success-border);
    background: var(--color-success-soft);
  }

  .ops-shell .chip.warn {
    color: var(--color-warning);
    border-color: var(--color-warning-border);
    background: var(--color-warning-soft);
  }

  .ops-shell .chip.err {
    color: var(--color-danger);
    border-color: var(--color-danger-border);
    background: var(--color-danger-soft);
  }

  .ops-shell .chip.codex {
    color: var(--codex);
    border-color: color-mix(in srgb, var(--codex) 32%, var(--th-border-subtle) 68%);
    background: color-mix(in srgb, var(--codex) 14%, var(--th-surface-alt) 86%);
  }

  .ops-shell .pulse {
    animation: ops-chip-pulse 1.6s ease-in-out infinite;
  }

  .ops-shell .ops-inline-alert {
    border-color: color-mix(in oklch, var(--warn) 30%, var(--th-border) 70%);
    background:
      linear-gradient(
        180deg,
        color-mix(in oklch, var(--warn) 8%, var(--th-surface) 92%) 0%,
        var(--th-surface) 100%
      );
  }

  .ops-shell .ops-signal-card {
    border-width: 1px;
  }

  .ops-shell .ops-mini-card {
    border-radius: 16px;
  }

  .ops-shell .ops-panel-card {
    padding: 12px;
    border-radius: 10px;
    border: 1px solid color-mix(in srgb, var(--th-border-subtle) 88%, transparent);
    background: color-mix(in srgb, var(--th-bg-surface) 92%, transparent);
  }

  .ops-shell .metric-label {
    display: flex;
    align-items: center;
    gap: 4px;
    font-size: 10.5px;
    font-weight: 600;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--th-text-muted);
  }

  .ops-shell .metric-value {
    margin-top: 10px;
    font-family: var(--font-display);
    font-size: 28px;
    font-weight: 600;
    letter-spacing: -1px;
    line-height: 1.1;
    font-variant-numeric: tabular-nums;
  }

  .ops-shell .metric-sub {
    margin-top: 4px;
    font-size: 12px;
    line-height: 1.6;
    color: var(--th-text-muted);
    font-variant-numeric: tabular-nums;
  }

  @keyframes ops-chip-pulse {
    0%, 100% { opacity: 0.65; transform: scale(0.92); }
    50% { opacity: 1; transform: scale(1); }
  }

  @media (max-width: 1180px) {
    .ops-shell .ops-main-grid,
    .ops-shell .ops-secondary-grid {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  @media (max-width: 1024px) {
    .ops-shell .page-header {
      align-items: flex-start;
      flex-direction: column;
    }
  }

  @media (max-width: 768px) {
    .ops-shell .page {
      padding: 16px 16px calc(9rem + env(safe-area-inset-bottom));
    }

    .ops-shell .grid-4 {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 520px) {
    .ops-shell .grid-4 {
      grid-template-columns: minmax(0, 1fr);
    }
  }
`;

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

function chipClassFromTone(tone: "info" | "warn" | "danger" | "success"): string {
  switch (tone) {
    case "success":
      return "chip ok";
    case "warn":
      return "chip warn";
    case "danger":
      return "chip err";
    case "info":
    default:
      return "chip codex";
  }
}

function surfaceStyleForSeverity(severity: SignalSeverity): { borderColor: string; background: string; valueColor: string } {
  switch (severity) {
    case "danger":
      return {
        borderColor: "color-mix(in srgb, var(--color-danger) 18%, var(--th-border-subtle) 82%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        valueColor: "var(--color-danger)",
      };
    case "warning":
      return {
        borderColor: "color-mix(in srgb, var(--color-warning) 18%, var(--th-border-subtle) 82%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        valueColor: "var(--color-warning)",
      };
    case "normal":
    default:
      return {
        borderColor: "color-mix(in srgb, var(--th-border-subtle) 88%, transparent)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        valueColor: "var(--color-info)",
      };
  }
}

function formatNumber(value: number): string {
  return new Intl.NumberFormat("en-US").format(value);
}

function formatBytes(value: number): string {
  const safe = Math.max(0, value);
  if (safe >= 1024 * 1024 * 1024) return `${(safe / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
  if (safe >= 1024 * 1024) return `${(safe / (1024 * 1024)).toFixed(1)} MiB`;
  if (safe >= 1024) return `${(safe / 1024).toFixed(1)} KiB`;
  return `${formatNumber(safe)} B`;
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

function formatBottleneckLabel(kind: string): string {
  return kind
    .replaceAll("_", " ")
    .replaceAll("provider disconnects", "provider disconnects")
    .replace(/\b\w/g, (char) => char.toUpperCase());
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
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

          <div className="card">
            <div className="card-head">
              <div className="min-w-0">
                <div className="card-title">{tr("Connection & Delivery", "Connection & Delivery")}</div>
                <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    "연결 상태와 전달 흐름의 건강도를 한눈에 확인합니다.",
                    "Track connectivity and delivery health at a glance.",
                  )}
                </div>
              </div>
            </div>
            <div data-testid="ops-connection-panel" className="card-body space-y-3">
              <div
                data-testid="ops-websocket-card"
                className="card ops-mini-card"
                style={{
                  borderColor: wsConnected
                    ? "color-mix(in srgb, var(--color-info) 18%, var(--th-border-subtle) 82%)"
                    : "color-mix(in srgb, var(--color-danger) 18%, var(--th-border-subtle) 82%)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
                }}
              >
                <div className="card-body">
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
                    <span className={chipClassFromTone(wsConnected ? "success" : "danger")}>
                      {wsConnected ? "LIVE" : "DISCONNECTED"}
                    </span>
                  </div>
                </div>
              </div>

              <div className="grid sm:grid-cols-2">
                <div data-testid="ops-dispatch-outbox-card" className="card ops-mini-card">
                  <div className="card-body">
                    <div className="metric-label">
                      dispatch_outbox
                    </div>
                    <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: "var(--th-text-primary)" }}>
                      {formatNumber(health?.dispatch_outbox?.pending ?? 0)}
                    </div>
                    <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                      {tr(
                        `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
                        `retry ${formatNumber(health?.dispatch_outbox?.retrying ?? 0)} · fail ${formatNumber(health?.dispatch_outbox?.permanent_failures ?? 0)}`,
                      )}
                    </div>
                  </div>
                </div>

                <div data-testid="ops-providers-card" className="card ops-mini-card">
                  <div className="card-body">
                    <div className="metric-label">
                      providers
                    </div>
                    <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: "var(--th-text-primary)" }}>
                      {connectedProviders}/{providerCount}
                    </div>
                    <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                      {tr(
                        `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
                        `disconnect ${formatNumber(disconnectedProviders)} · restart ${formatNumber(restartPendingProviders)}`,
                      )}
                    </div>
                  </div>
                </div>
              </div>

              <div data-testid="ops-prompt-retention-card" className="card ops-mini-card">
                <div className="card-body">
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <div className="metric-label">
                        <Database size={12} />
                        prompt_manifest_retention
                      </div>
                      <div className="metric-value" style={{ marginTop: 8, fontSize: 22, color: "var(--th-text-primary)" }}>
                        {promptRetentionValue}
                      </div>
                      <div className="metric-sub" style={{ marginTop: 6, fontSize: 12 }}>
                        {promptRetentionConfigNote}
                      </div>
                      <div className="metric-sub" style={{ marginTop: 4, fontSize: 12 }}>
                        {promptRetentionStorageNote}
                      </div>
                    </div>
                    <span className={chipClassFromTone(promptRetentionTone)}>
                      {promptRetention?.hot_reload ? "HOT" : promptRetention?.config_applied_at?.toUpperCase() ?? "BOOT"}
                    </span>
                  </div>
                  {promptRetention?.config_source ? (
                    <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {promptRetention.config_source}
                    </div>
                  ) : null}
                  {promptRetentionError ? (
                    <div className="mt-2 text-[11px] leading-5" style={{ color: "var(--color-warning)" }}>
                      {promptRetentionError}
                    </div>
                  ) : null}
                </div>
              </div>
            </div>
          </div>
        </div>

        <div className="card mt-4">
          <div className="card-head">
            <div className="min-w-0">
              <div className="card-title">{tr("오피스 운영", "Office Operations")}</div>
              <div className="mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "오피스 공간, 좌석, 배치를 한곳에서 관리합니다.",
                  "Manage spaces, seats, and layouts in one place.",
                )}
              </div>
            </div>
          </div>
          <div className="card-body">
            <div className="-mx-4 sm:-mx-5">
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
      </div>
    </div>
  );
}
