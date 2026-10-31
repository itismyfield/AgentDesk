import type { HealthResponse } from "../api";

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
  /** Stable id (metric key or raw degraded reason); shown as the tooltip. */
  kind: string;
  label: string;
  count: number;
  severity: Exclude<SignalSeverity, "normal">;
  detail: string;
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

export function formatDurationCompact(seconds: number, isKo = false): string {
  const safe = Math.max(0, Math.round(seconds));
  if (safe >= 86_400) {
    const days = Math.floor(safe / 86_400);
    const hours = Math.floor((safe % 86_400) / 3600);
    return isKo ? `${days}일 ${hours}시간` : `${days}d ${hours}h`;
  }
  if (safe >= 3600) {
    const hours = Math.floor(safe / 3600);
    const minutes = Math.floor((safe % 3600) / 60);
    return isKo ? `${hours}시간 ${minutes}분` : `${hours}h ${minutes}m`;
  }
  if (safe >= 60) {
    const minutes = Math.floor(safe / 60);
    const remainSeconds = safe % 60;
    return isKo ? `${minutes}분 ${remainSeconds}초` : `${minutes}m ${remainSeconds}s`;
  }
  return isKo ? `${safe}초` : `${safe}s`;
}

/** Turns a runtime degraded-reason key into a short human label; callers keep the raw key as a tooltip. */
export function describeDegradedReason(reason: string, isKo: boolean): string {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const provider = reason.match(/^provider:([^:]+):(.+)$/);
  if (provider) {
    const name = provider[1];
    const detail = provider[2];
    if (detail === "disconnected") return tr(`${name} 연결 끊김`, `${name} disconnected`);
    if (detail === "restart_pending") return tr(`${name} 재시작 대기`, `${name} restart pending`);
    if (detail === "reconcile_in_progress") return tr(`${name} 상태 맞추는 중`, `${name} reconciling`);
    const deferred = detail.match(/^deferred_hooks_backlog:(\d+)$/);
    if (deferred) return tr(`${name} 후처리 대기 ${deferred[1]}건`, `${name} deferred hooks ${deferred[1]}`);
    const queue = detail.match(/^pending_queue_depth:(\d+)$/);
    if (queue) return tr(`${name} 처리 대기 ${queue[1]}건`, `${name} queue depth ${queue[1]}`);
    const recovering = detail.match(/^recovering_channels:(\d+)$/);
    if (recovering) return tr(`${name} 채널 ${recovering[1]}개 복구 중`, `${name} recovering ${recovering[1]} channels`);
  }
  const outbox = reason.match(/^dispatch_outbox_oldest_pending_age:(\d+)$/);
  if (outbox) {
    const age = formatDurationCompact(Number(outbox[1]), isKo);
    return tr(`전달 대기 ${age}째`, `Dispatch outbox waiting ${age}`);
  }
  if (reason === "db_unavailable") return tr("데이터베이스 연결 안 됨", "Database unavailable");
  if (reason === "no_providers_registered") return tr("등록된 프로바이더 없음", "No providers registered");
  const relay = reason.match(/^relay_verdict_unknown_([a-z]+)_\d+$/);
  if (relay) return tr(`${relay[1]} 응답 전달 확인 안 됨`, `${relay[1]} relay delivery unconfirmed`);
  if (reason.startsWith("tui_o:boundary_pending")) return tr("TUI 출력 마무리 대기", "TUI output waiting to finish");
  if (reason.startsWith("tui_o:unresolved")) return tr("TUI 출력 확인 필요", "TUI output unresolved");
  if (reason.startsWith("tui_o:cleanup_retired")) return tr("끝난 TUI 출력 정리 대기", "Retired TUI output awaiting cleanup");
  // Other TUI writer alarms share one readable title; the exact key stays in the tooltip.
  if (reason.startsWith("tui_o:")) return tr("TUI 출력 경보", "TUI output alarm");
  const presence = reason.match(/^turn_presence_unknown:([^:]+):([^:]+)/);
  if (presence) return tr(`${presence[2]} 턴 상태 확인 불가 (${presence[1]})`, `${presence[2]} turn state unknown (${presence[1]})`);
  // Unknown keys: keep the leading segment and drop channel ids and counters.
  return reason.split(":")[0].replace(/_\d{6,}$/, "").replaceAll("_", " ");
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
      label: isKo ? "후처리 대기" : "Deferred hooks",
      value: formatNumber(deferredHooks),
      rawValue: deferredHooks,
      severity: resolveSeverity(deferredHooks, SIGNAL_THRESHOLDS.deferred_hooks),
      note: isKo
        ? `후처리 대기 ${formatNumber(deferredHooks)}건`
        : `${formatNumber(deferredHooks)} waiting for post-processing`,
    },
    {
      key: "outbox_age",
      label: isKo ? "전달 지연" : "Outbox age",
      value: formatDurationCompact(outboxAge, isKo),
      rawValue: outboxAge,
      severity: resolveSeverity(outboxAge, SIGNAL_THRESHOLDS.outbox_age),
      note: isKo
        ? `대기 ${formatNumber(data.dispatch_outbox?.pending ?? 0)} · 재시도 ${formatNumber(data.dispatch_outbox?.retrying ?? 0)}`
        : `pending ${formatNumber(data.dispatch_outbox?.pending ?? 0)} · retry ${formatNumber(data.dispatch_outbox?.retrying ?? 0)}`,
    },
    {
      key: "pending_queue",
      label: isKo ? "처리 대기" : "Pending queue",
      value: formatNumber(pendingQueue),
      rawValue: pendingQueue,
      severity: resolveSeverity(pendingQueue, SIGNAL_THRESHOLDS.pending_queue),
      note: isKo
        ? `진행 중 ${formatNumber(data.global_active ?? 0)} · 마무리 중 ${formatNumber(data.global_finalizing ?? 0)}`
        : `active ${formatNumber(data.global_active ?? 0)} · finalizing ${formatNumber(data.global_finalizing ?? 0)}`,
    },
    {
      key: "active_watchers",
      label: isKo ? "감시 중" : "Active watchers",
      value: formatNumber(activeWatchers),
      rawValue: activeWatchers,
      severity: resolveSeverity(activeWatchers, SIGNAL_THRESHOLDS.active_watchers),
      note: isKo
        ? `프로바이더 ${formatNumber(providerCount)}개 연결`
        : `${formatNumber(providerCount)} providers in scope`,
    },
    {
      key: "recovery_seconds",
      label: isKo ? "복구 시간" : "Recovery",
      value: formatDurationCompact(recoverySeconds, isKo),
      rawValue: recoverySeconds,
      severity: resolveSeverity(recoverySeconds, SIGNAL_THRESHOLDS.recovery_seconds),
      note: isKo
        ? `가동 ${formatDurationCompact(data.uptime_secs ?? 0, true)}`
        : `uptime ${formatDurationCompact(data.uptime_secs ?? 0)}`,
    },
  ];
}

// Metric rows already cover these reasons, so they are only listed when no metric row fired.
const METRIC_REASON = /^(provider:|dispatch_outbox_)/;

export function buildBottlenecks(data: HealthResponse, isKo: boolean): BottleneckRow[] {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const rows: BottleneckRow[] = [];
  const outboxAge = data.outbox_age ?? data.dispatch_outbox?.oldest_pending_age ?? 0;
  const disconnectedProviders = (data.providers ?? []).filter((provider) => !provider.connected).length;
  const restartPendingProviders = (data.providers ?? []).filter((provider) => provider.restart_pending).length;
  const providerQueueDepth = (data.providers ?? []).reduce(
    (sum, provider) => sum + Math.max(provider.queue_depth ?? 0, 0),
    0,
  );

  const addMetricBottleneck = (
    key: SignalCard["key"],
    count: number,
    label: string,
    detail: string,
  ) => {
    const severity = resolveSeverity(count, SIGNAL_THRESHOLDS[key]);
    if (severity === "normal" || count <= 0) return;
    rows.push({ kind: key, label, count, severity, detail });
  };

  addMetricBottleneck("deferred_hooks", data.deferred_hooks ?? 0, tr("후처리 대기", "Deferred hooks"), tr("턴이 끝난 뒤 처리할 작업이 쌓였어요.", "Post-turn work is backing up."));
  addMetricBottleneck("outbox_age", outboxAge, tr("전달 지연", "Outbox age"), tr("가장 오래 기다린 전달 항목의 대기 시간(초)이에요.", "Age in seconds of the oldest undelivered item."));
  addMetricBottleneck("pending_queue", data.queue_depth ?? 0, tr("처리 대기", "Pending queue"), tr("아직 시작하지 못한 요청이에요.", "Requests not started yet."));
  addMetricBottleneck("recovery_seconds", data.recovery_duration ?? 0, tr("복구 시간", "Recovery"), tr("재시작 뒤 이전 작업을 되살리는 데 오래 걸렸어요.", "Restoring work after restart took long."));
  addMetricBottleneck("active_watchers", data.watcher_count ?? 0, tr("감시 중", "Active watchers"), tr("동시에 지켜보는 세션이 많아요.", "Many sessions are being watched at once."));

  if (disconnectedProviders > 0) {
    rows.push({
      kind: "provider_disconnects",
      label: tr("프로바이더 연결 끊김", "Provider disconnects"),
      count: disconnectedProviders,
      severity: disconnectedProviders >= 2 ? "danger" : "warning",
      detail: tr("연결이 끊긴 프로바이더 수예요.", "Providers that are disconnected."),
    });
  }

  if (restartPendingProviders > 0) {
    rows.push({
      kind: "restart_pending",
      label: tr("재시작 대기", "Restart pending"),
      count: restartPendingProviders,
      severity: restartPendingProviders >= 2 ? "danger" : "warning",
      detail: tr("재시작을 기다리는 프로바이더 수예요.", "Providers waiting for a restart."),
    });
  }

  if (providerQueueDepth > 0) {
    rows.push({
      kind: "provider_queue",
      label: tr("프로바이더 대기열", "Provider queue"),
      count: providerQueueDepth,
      severity: resolveSeverity(providerQueueDepth, SIGNAL_THRESHOLDS.pending_queue) === "danger" ? "danger" : "warning",
      detail: tr("프로바이더별 대기 건수의 합이에요.", "Sum of per-provider queue depth."),
    });
  }

  const metricRowsFired = rows.length > 0;
  for (const reason of data.degraded_reasons ?? []) {
    if (metricRowsFired && METRIC_REASON.test(reason)) continue;
    rows.push({
      kind: reason,
      label: describeDegradedReason(reason, isKo),
      count: 1,
      severity: data.status === "unhealthy" ? "danger" : "warning",
      detail: tr("서버가 보고한 주의 신호예요. 원래 키는 제목에 마우스를 올리면 보여요.", "Reported by the server. Hover the title for the raw key."),
    });
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

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
