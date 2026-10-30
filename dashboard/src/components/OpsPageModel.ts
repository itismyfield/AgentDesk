import type { HealthResponse } from "../api";
import { describeDegradedReason } from "./dashboard/HealthWidget";

export type SignalSeverity = "normal" | "warning" | "danger";

interface Threshold {
  warning: number;
  danger: number;
}

export interface SignalCard {
  key: "deferred_hooks" | "outbox_age" | "pending_queue" | "active_watchers" | "recovery_seconds";
  label: string;
  value: string;
  rawValue: number;
  severity: SignalSeverity;
  note: string;
}

export interface BottleneckRow {
  kind: string;
  count: number;
  severity: Exclude<SignalSeverity, "normal">;
  detail: string;
}

export interface RuntimeSignalRow {
  key: string;
  label: string;
  value: string;
  hint: string;
  severity: SignalSeverity;
}

export const STALE_AFTER_MS = 75_000;
export const LIVE_POLL_INTERVAL_MS = 5_000;
export const DISCONNECTED_POLL_BASE_MS = 5_000;
export const MAX_DISCONNECTED_POLL_MS = 30_000;
export const WS_REFRESH_DEBOUNCE_MS = 800;

export const SIGNAL_THRESHOLDS: Record<SignalCard["key"], Threshold> = {
  deferred_hooks: { warning: 1, danger: 3 },
  outbox_age: { warning: 30, danger: 60 },
  pending_queue: { warning: 1, danger: 3 },
  active_watchers: { warning: 4, danger: 8 },
  recovery_seconds: { warning: 180, danger: 600 },
};

export function resolveSeverity(value: number, threshold: Threshold): SignalSeverity {
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

export function toneForSeverity(severity: SignalSeverity): "info" | "warn" | "danger" | "success" {
  switch (severity) {
    case "danger":
      return "danger";
    case "warning":
      return "warn";
    default:
      return "success";
  }
}

export function chipClassFromTone(tone: "info" | "warn" | "danger" | "success"): string {
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

/**
 * Maps Ops-local tone vocabulary to the shared SystemHealthTone the rest of
 * the dashboard uses. Lets Ops surfaces opt into StatusBadge without rewriting
 * every consumer at once.
 */
export function opsToneToHealth(
  tone: "info" | "warn" | "danger" | "success",
): "info" | "warning" | "critical" | "healthy" {
  switch (tone) {
    case "success":
      return "healthy";
    case "warn":
      return "warning";
    case "danger":
      return "critical";
    case "info":
    default:
      return "info";
  }
}

export function surfaceStyleForSeverity(severity: SignalSeverity): { borderColor: string; background: string; valueColor: string } {
  switch (severity) {
    case "danger":
      return {
        borderColor: "color-mix(in srgb, var(--color-danger) 18%, var(--th-border-subtle) 82%)",
        background:
          "var(--th-bg-surface)",
        valueColor: "var(--color-danger)",
      };
    case "warning":
      return {
        borderColor: "color-mix(in srgb, var(--color-warning) 18%, var(--th-border-subtle) 82%)",
        background:
          "var(--th-bg-surface)",
        valueColor: "var(--color-warning)",
      };
    case "normal":
    default:
      return {
        borderColor: "color-mix(in srgb, var(--th-border-subtle) 88%, transparent)",
        background:
          "var(--th-bg-surface)",
        valueColor: "var(--th-text-primary)",
      };
  }
}

export function formatNumber(value: number): string {
  return new Intl.NumberFormat("en-US").format(value);
}

export function formatBytes(value: number): string {
  const safe = Math.max(0, value);
  if (safe >= 1024 * 1024 * 1024) return `${(safe / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
  if (safe >= 1024 * 1024) return `${(safe / (1024 * 1024)).toFixed(1)} MiB`;
  if (safe >= 1024) return `${(safe / 1024).toFixed(1)} KiB`;
  return `${formatNumber(safe)} B`;
}

export function formatDurationCompact(seconds: number): string {
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

export function formatUpdatedAt(timestamp: number | null, localeTag: string): string {
  if (!timestamp) return "n/a";
  return new Date(timestamp).toLocaleTimeString(localeTag, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

export function buildSignalCards(data: HealthResponse, isKo: boolean): SignalCard[] {
  const deferredHooks = data.deferred_hooks ?? 0;
  const outboxAge = data.outbox_age ?? data.dispatch_outbox?.oldest_pending_age ?? 0;
  const pendingQueue = data.queue_depth ?? 0;
  const activeWatchers = data.watcher_count ?? 0;
  const recoverySeconds = data.recovery_duration ?? 0;
  const providerCount = data.providers?.length ?? 0;

  return [
    {
      key: "deferred_hooks",
      label: isKo ? "후처리 대기" : "Deferred Hooks",
      value: formatNumber(deferredHooks),
      rawValue: deferredHooks,
      severity: resolveSeverity(deferredHooks, SIGNAL_THRESHOLDS.deferred_hooks),
      note: isKo
        ? `후처리 대기 ${formatNumber(deferredHooks)}건`
        : `${formatNumber(deferredHooks)} waiting for post-processing`,
    },
    {
      key: "outbox_age",
      label: isKo ? "전달 지연" : "Outbox Age",
      value: formatDurationCompact(outboxAge),
      rawValue: outboxAge,
      severity: resolveSeverity(outboxAge, SIGNAL_THRESHOLDS.outbox_age),
      note: isKo
        ? `pending ${formatNumber(data.dispatch_outbox?.pending ?? 0)} · retry ${formatNumber(data.dispatch_outbox?.retrying ?? 0)}`
        : `pending ${formatNumber(data.dispatch_outbox?.pending ?? 0)} · retry ${formatNumber(data.dispatch_outbox?.retrying ?? 0)}`,
    },
    {
      key: "pending_queue",
      label: isKo ? "처리 대기" : "Pending Queue",
      value: formatNumber(pendingQueue),
      rawValue: pendingQueue,
      severity: resolveSeverity(pendingQueue, SIGNAL_THRESHOLDS.pending_queue),
      note: isKo
        ? `active ${formatNumber(data.global_active ?? 0)} · finalizing ${formatNumber(data.global_finalizing ?? 0)}`
        : `active ${formatNumber(data.global_active ?? 0)} · finalizing ${formatNumber(data.global_finalizing ?? 0)}`,
    },
    {
      key: "active_watchers",
      label: isKo ? "감시 중" : "Active Watchers",
      value: formatNumber(activeWatchers),
      rawValue: activeWatchers,
      severity: resolveSeverity(activeWatchers, SIGNAL_THRESHOLDS.active_watchers),
      note: isKo
        ? `${formatNumber(providerCount)}개 provider 추적 중`
        : `${formatNumber(providerCount)} providers in scope`,
    },
    {
      key: "recovery_seconds",
      label: isKo ? "복구 시간" : "Recovery",
      value: formatDurationCompact(recoverySeconds),
      rawValue: recoverySeconds,
      severity: resolveSeverity(recoverySeconds, SIGNAL_THRESHOLDS.recovery_seconds),
      note: isKo
        ? `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`
        : `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
    },
  ];
}

export function buildBottlenecks(data: HealthResponse): BottleneckRow[] {
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

export function translateStatus(status: string, isKo: boolean): string {
  if (status === "healthy") return isKo ? "정상" : "Healthy";
  if (status === "degraded") return isKo ? "주의" : "Degraded";
  if (status === "unhealthy") return isKo ? "장애" : "Unhealthy";
  return status.toUpperCase();
}

export function formatBottleneckLabel(kind: string): string {
  return kind
    .replaceAll("_", " ")
    .replaceAll("provider disconnects", "provider disconnects")
    .replace(/\b\w/g, (char) => char.toUpperCase());
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
