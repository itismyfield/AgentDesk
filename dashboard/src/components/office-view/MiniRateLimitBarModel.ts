export type RateLimitLevel = "normal" | "warning" | "danger";

export interface RLBucket {
  id: string;
  label: string;
  utilization: number | null;
  projectedUtilization: number | null;
  resetAtMs: number | null;
  level: RateLimitLevel;
  projectedLevel: RateLimitLevel | null;
}

export interface RLProvider {
  provider: string;
  buckets: RLBucket[];
  stale: boolean;
  unsupported: boolean;
  reason: string | null;
}

export interface RawRLBucket {
  name: string;
  limit: number;
  used: number;
  remaining: number;
  reset: number;
  utilization?: number;
}

export interface RawRLProvider {
  provider: string;
  buckets: RawRLBucket[];
  stale: boolean;
  unsupported?: boolean;
  reason?: string | null;
}

interface TransformRLOptions {
  nowMs?: number;
}

interface ProjectRateLimitBucketInput {
  label: string;
  utilization: number | null;
  resetAtMs: number | null;
}

const RL_HIDDEN_PROVIDERS = new Set(["github"]);
const RL_HIDDEN_BUCKETS = new Set(["7d Sonnet"]);
const RATE_LIMIT_WARNING_PCT = 80;
const RATE_LIMIT_DANGER_PCT = 95;
const RATE_LIMIT_MIN_ELAPSED_MS = 60_000;

export function normalizeMiniRateLimitProviderLabel(provider: string): string {
  const normalized = provider.trim().toLowerCase();
  switch (normalized) {
    case "claude":
      return "Claude";
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "qwen":
      return "Qwen";
    default:
      return provider ? provider.charAt(0).toUpperCase() + provider.slice(1) : provider;
  }
}

function clampPct(value: number, max = 999): number {
  return Math.max(0, Math.min(value, max));
}

function classifyRateLimitLevel(utilization: number | null): RateLimitLevel {
  if (utilization !== null && utilization >= RATE_LIMIT_DANGER_PCT) return "danger";
  if (utilization !== null && utilization >= RATE_LIMIT_WARNING_PCT) return "warning";
  return "normal";
}

function normalizeBucketUtilization(bucket: RawRLBucket): number | null {
  if (
    typeof bucket.utilization === "number" &&
    Number.isFinite(bucket.utilization) &&
    bucket.utilization >= 0
  ) {
    return Math.floor(clampPct(bucket.utilization));
  }
  return bucket.limit > 0 && bucket.used >= 0 && bucket.remaining >= 0
    ? Math.round((bucket.used / bucket.limit) * 100)
    : null;
}

export function parseRateLimitWindowMs(label: string): number | null {
  const match = label.trim().toLowerCase().match(/^(\d+(?:\.\d+)?)\s*([mhd])$/);
  if (!match) return null;
  const value = Number(match[1]);
  if (!Number.isFinite(value) || value <= 0) return null;
  switch (match[2]) {
    case "m":
      return value * 60_000;
    case "h":
      return value * 3_600_000;
    case "d":
      return value * 86_400_000;
    default:
      return null;
  }
}

export function projectRateLimitBucketAtReset(
  bucket: ProjectRateLimitBucketInput,
  nowMs = Date.now(),
): number | null {
  const { label, utilization, resetAtMs } = bucket;
  if (utilization === null || resetAtMs === null) return null;
  const windowMs = parseRateLimitWindowMs(label);
  if (windowMs === null) return null;

  const remainingMs = resetAtMs - nowMs;
  if (remainingMs <= 0) return utilization;

  const elapsedMs = windowMs - remainingMs;
  if (elapsedMs < RATE_LIMIT_MIN_ELAPSED_MS) return null;

  const projected = utilization * (windowMs / elapsedMs);
  return Number.isFinite(projected) ? Math.round(clampPct(projected)) : null;
}

function startOfLocalDayMs(date: Date): number {
  return new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
}

function formatResetClock(resetAtMs: number, isKo: boolean): string {
  return new Intl.DateTimeFormat(isKo ? "ko-KR" : "en-US", {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(new Date(resetAtMs));
}

function formatResetDay(resetAtMs: number, nowMs: number, isKo: boolean): string | null {
  const resetDate = new Date(resetAtMs);
  const nowDate = new Date(nowMs);
  const dayDiff = Math.round(
    (startOfLocalDayMs(resetDate) - startOfLocalDayMs(nowDate)) / 86_400_000,
  );
  if (dayDiff === 0) return null;
  if (dayDiff === 1) return isKo ? "내일" : "tomorrow";
  return `${resetDate.getMonth() + 1}/${resetDate.getDate()}`;
}

function formatResetDistance(resetAtMs: number, nowMs: number): string {
  const diffMs = resetAtMs - nowMs;
  if (diffMs <= 0) return "now";
  const totalMinutes = Math.max(1, Math.round(diffMs / 60_000));
  const days = Math.floor(totalMinutes / 1_440);
  const hours = Math.floor((totalMinutes % 1_440) / 60);
  const minutes = totalMinutes % 60;
  if (days > 0) return `${days}d${hours > 0 ? ` ${hours}h` : ""}`;
  if (hours > 0) return `${hours}h${minutes > 0 ? ` ${minutes}m` : ""}`;
  return `${minutes}m`;
}

function formatResetDistanceShort(resetAtMs: number, nowMs: number): string {
  const diffMs = resetAtMs - nowMs;
  if (diffMs <= 0) return "now";
  const totalMinutes = Math.max(1, Math.round(diffMs / 60_000));
  const days = Math.floor(totalMinutes / 1_440);
  const hours = Math.floor((totalMinutes % 1_440) / 60);
  if (days > 0) return `${days}d`;
  if (hours > 0) return `${hours}h`;
  return `${totalMinutes}m`;
}

export function formatRateLimitResetLabel(
  resetAtMs: number | null,
  isKo: boolean,
  nowMs = Date.now(),
  density: "full" | "short" = "full",
): string {
  if (resetAtMs === null) {
    if (density === "short") return isKo ? "↻ 미확인" : "↻ unavailable";
    return isKo ? "초기화 정보 없음" : "Reset unavailable";
  }
  const distance =
    density === "short"
      ? formatResetDistanceShort(resetAtMs, nowMs)
      : formatResetDistance(resetAtMs, nowMs);
  if (distance === "now") {
    if (density === "short") return "↻ now";
    return isKo ? "초기화 지금" : "Reset now";
  }
  const day = formatResetDay(resetAtMs, nowMs, isKo);
  const clock = formatResetClock(resetAtMs, isKo);
  const when = day ? `${day} ${clock}` : clock;
  if (density === "short") return `↻ ${when} · ${distance}`;
  return isKo ? `초기화 ${when} · ${distance} 후` : `Reset ${when} · in ${distance}`;
}

export function transformRLProviders(
  raw: RawRLProvider[],
  options: TransformRLOptions = {},
): RLProvider[] {
  const nowMs = options.nowMs ?? Date.now();
  return raw
    .filter((rp) => !RL_HIDDEN_PROVIDERS.has(rp.provider.toLowerCase()))
    .flatMap((rp) => {
      const buckets = rp.buckets
        .filter((b) => !RL_HIDDEN_BUCKETS.has(b.name))
        .map((b) => {
          const utilization = normalizeBucketUtilization(b);
          const resetAtMs = b.reset > 0 ? b.reset * 1000 : null;
          const projectedUtilization = projectRateLimitBucketAtReset(
            { label: b.name, utilization, resetAtMs },
            nowMs,
          );
          return {
            id: b.name,
            label: b.name,
            utilization,
            projectedUtilization,
            resetAtMs,
            level: classifyRateLimitLevel(utilization),
            projectedLevel:
              projectedUtilization === null ? null : classifyRateLimitLevel(projectedUtilization),
          };
        });
      if (rp.unsupported && buckets.length === 0) {
        return [];
      }
      return [
        {
          provider: normalizeMiniRateLimitProviderLabel(rp.provider),
          stale: rp.stale,
          unsupported: Boolean(rp.unsupported),
          reason: typeof rp.reason === "string" ? rp.reason : null,
          buckets,
        },
      ];
    });
}
