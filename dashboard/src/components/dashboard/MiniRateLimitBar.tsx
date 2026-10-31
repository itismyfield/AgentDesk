import { useEffect, useState } from "react";
import { request } from "../../api/httpClient";
import {
  getProviderLevelColors,
  getProviderMeta,
} from "../../app/providerTheme";
import {
  RATE_LIMIT_GAUGE_TRACK_STYLE,
  rateLimitFillStyle,
  rateLimitFillWidth,
  rateLimitProjectionStyle,
  rateLimitProjectionWidth,
} from "../common/rateLimitGauge";
import {
  formatRateLimitResetLabel,
  formatRateLimitWindowLabel,
  transformRLProviders,
  type RLBucket,
  type RLProvider,
  type RawRLProvider,
} from "./MiniRateLimitBarModel";

const RL_ICONS: Record<string, string> = {
  Claude: "🤖",
  Codex: "⚡",
  Gemini: "🔮",
  Qwen: "🧠",
  OpenCode: "🧩",
  Copilot: "🛩️",
  Antigravity: "🌀",
  API: "🔌",
};

function formatCurrentPct(bucket: RLBucket): string {
  return bucket.utilization === null ? "N/A" : `${bucket.utilization}%`;
}

function formatProjectedPct(bucket: RLBucket): string {
  return bucket.projectedUtilization === null ? "→ --" : `→ ${bucket.projectedUtilization}%`;
}

function shouldShowProjectedUtilization(bucket: RLBucket): boolean {
  return bucket.label.trim().toLowerCase() !== "5h";
}

function formatCompactPctPair(bucket: RLBucket, showProjectedUtilization: boolean): string {
  if (bucket.utilization === null) return "N/A";
  if (!showProjectedUtilization) return `${bucket.utilization}%`;
  if (bucket.projectedUtilization === null) return `${bucket.utilization}→--`;
  return `${bucket.utilization}→${bucket.projectedUtilization}%`;
}

function projectionChipStyle(provider: string, bucket: RLBucket) {
  const colors = getProviderLevelColors(provider, bucket.projectedLevel ?? bucket.level);
  return {
    color: bucket.projectedUtilization === null ? "var(--th-text-muted)" : colors.text,
    background:
      bucket.projectedUtilization === null
        ? "color-mix(in oklch, var(--fg-faint) 10%, var(--bg-2) 90%)"
        : `color-mix(in oklch, ${colors.bar} 14%, var(--bg-2) 86%)`,
    border:
      bucket.projectedUtilization === null
        ? "1px solid color-mix(in oklch, var(--fg-faint) 20%, var(--line) 80%)"
        : `1px solid color-mix(in oklch, ${colors.bar} 28%, var(--line) 72%)`,
  };
}

function RateLimitBucketGauge({
  bucket,
  provider,
  isKo,
  density,
}: {
  bucket: RLBucket;
  provider: string;
  isKo: boolean;
  density: "compact" | "comfortable";
}) {
  const colors = getProviderLevelColors(provider, "normal");
  const showProjectedUtilization = shouldShowProjectedUtilization(bucket);
  const utilizationWidth = rateLimitFillWidth(bucket.utilization);
  const projectionWidth =
    !showProjectedUtilization || bucket.projectedUtilization === null
      ? "0%"
      : rateLimitProjectionWidth(bucket.projectedUtilization, bucket.utilization);
  const nowMs = Date.now();
  const resetLabel = formatRateLimitResetLabel(bucket.resetAtMs, isKo, nowMs);
  const resetShortLabel = formatRateLimitResetLabel(bucket.resetAtMs, isKo, nowMs, "short");

  if (density === "comfortable") {
    return (
      <div key={bucket.id} className="min-w-0 space-y-1">
        <div className="flex min-w-0 items-center gap-1.5">
          <span
            className="shrink-0 text-xs font-bold"
            style={{ color: colors.bar }}
          >
            {bucket.label}
          </span>
          <span
            className="ml-auto shrink-0 text-xs font-mono font-bold text-right"
            style={{
              color: bucket.utilization === null ? "var(--th-text-muted)" : colors.bar,
            }}
          >
            {formatCurrentPct(bucket)}
          </span>
          {showProjectedUtilization ? (
            <span
              className="w-[48px] rounded px-1 py-0.5 text-center text-[10px] font-mono font-bold"
              style={projectionChipStyle(provider, bucket)}
            >
              {formatProjectedPct(bucket)}
            </span>
          ) : null}
        </div>
        <div
          className="relative h-2.5 w-full rounded-full overflow-hidden"
          style={RATE_LIMIT_GAUGE_TRACK_STYLE}
        >
          <div
            className="absolute inset-y-0 left-0 rounded-full"
            style={{
              width: projectionWidth,
              ...rateLimitProjectionStyle(colors.bar, 7, 11),
            }}
          />
          <div
            className="absolute inset-y-0 left-0 rounded-full"
            style={{
              width: utilizationWidth,
              ...(bucket.utilization === null
                ? { background: "transparent", boxShadow: "none" }
                : rateLimitFillStyle(colors.bar, colors.glow, 5)),
            }}
          />
        </div>
        <div
          className="truncate text-[10px] font-medium"
          style={{ color: "var(--th-text-muted)" }}
          title={resetLabel}
        >
          {resetShortLabel}
        </div>
      </div>
    );
  }

  return (
    <div key={bucket.id} className="min-w-0 space-y-0.5">
      <div className="flex min-w-0 items-center gap-1">
        <span
          className="text-xs font-bold shrink-0 w-[20px]"
          style={{ color: colors.bar }}
        >
          {bucket.label}
        </span>
        <div className="flex-1 min-w-0">
          <div
            className="relative h-1.5 rounded-full overflow-hidden"
            style={RATE_LIMIT_GAUGE_TRACK_STYLE}
          >
            <div
              className="absolute inset-y-0 left-0 rounded-full"
              style={{
                width: projectionWidth,
                ...rateLimitProjectionStyle(colors.bar, 5, 8),
              }}
            />
            <div
              className="absolute inset-y-0 left-0 rounded-full"
              style={{
                width: utilizationWidth,
                ...(bucket.utilization === null
                  ? { background: "transparent", boxShadow: "none" }
                  : rateLimitFillStyle(colors.bar, colors.glow, 4)),
              }}
            />
          </div>
        </div>
        <span
          className="text-[10px] font-mono font-bold shrink-0 w-[54px] text-right"
          style={{
            color: bucket.utilization === null ? "var(--th-text-muted)" : colors.bar,
          }}
        >
          {formatCompactPctPair(bucket, showProjectedUtilization)}
        </span>
      </div>
      <div
        className="truncate text-[9px] font-medium"
        style={{ color: "var(--th-text-muted)" }}
        title={resetLabel}
      >
        {resetShortLabel}
      </div>
    </div>
  );
}

function HomeSummaryBucket({ bucket, isKo }: { bucket: RLBucket; isKo: boolean }) {
  const isNearLimit = bucket.utilization !== null && bucket.utilization >= 90;
  const projected = shouldShowProjectedUtilization(bucket) ? bucket.projectedUtilization : null;
  const resetLabel = formatRateLimitResetLabel(bucket.resetAtMs, isKo);

  return (
    <div className="min-w-0 space-y-1">
      <div className="flex min-w-0 items-baseline justify-between gap-2 text-xs leading-4">
        <span className="min-w-0 break-words text-th-text-secondary">{formatRateLimitWindowLabel(bucket.label, isKo)}</span>
        <span
          className="shrink-0 font-medium tabular-nums text-th-text-primary"
          style={isNearLimit ? { color: "var(--th-badge-amber-text)" } : undefined}
        >
          {bucket.utilization === null
            ? isKo ? "정보 없음" : "No data"
            : isKo ? `${bucket.utilization}% 사용` : `${bucket.utilization}% used`}
        </span>
      </div>
      <div className="h-1 overflow-hidden rounded-full bg-th-border-subtle" aria-hidden="true">
        <div
          className="h-full rounded-full bg-th-text-secondary"
          style={{
            width: rateLimitFillWidth(bucket.utilization),
            ...(isNearLimit ? { backgroundColor: "var(--th-badge-amber-text)" } : {}),
          }}
        />
      </div>
      <p className="break-words text-[11px] leading-4 text-th-text-muted">
        {resetLabel}
        {projected !== null ? (
          <span style={projected >= 100 ? { color: "var(--th-badge-amber-text)" } : undefined}>
            {isKo ? ` · 이 속도면 ${projected}%` : ` · on pace for ${projected}%`}
          </span>
        ) : null}
      </p>
    </div>
  );
}

export function MiniRateLimitBar({
  isKo,
  density = "compact",
  layout = "default",
}: {
  isKo: boolean;
  density?: "compact" | "comfortable";
  layout?: "default" | "homeWide" | "homeSummary";
}) {
  const [providers, setProviders] = useState<RLProvider[]>([]);
  const [loadState, setLoadState] = useState<"loading" | "ready" | "error">("loading");
  const isComfy = density === "comfortable";

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const json = await request<{ providers?: RawRLProvider[] }>("/api/rate-limits", { suppressErrorToast: true });
        if (mounted) {
          setProviders(transformRLProviders(json.providers ?? []));
          setLoadState("ready");
        }
      } catch {
        if (mounted) setLoadState("error");
      }
    };
    load();
    // Hidden tabs skip the poll and refresh once they are visible again.
    const timer = setInterval(() => { if (!document.hidden) load(); }, 30_000);
    const onVisible = () => { if (!document.hidden) load(); };
    document.addEventListener("visibilitychange", onVisible);
    return () => { mounted = false; clearInterval(timer); document.removeEventListener("visibilitychange", onVisible); };
  }, []);

  if (layout === "homeSummary") {
    return (
      <div className="mt-3 min-w-0">
        {loadState === "loading" || loadState === "error" || providers.length === 0 ? (
          <p role="status" className="text-xs leading-5 text-th-text-muted">
            {loadState === "loading"
              ? isKo ? "한도 불러오는 중…" : "Loading rate limits…"
              : loadState === "error"
                ? isKo ? "한도를 불러오지 못했습니다. 잠시 후 다시 확인합니다." : "Could not load rate limits. Retrying shortly."
                : isKo ? "표시할 한도 데이터가 없습니다." : "No rate-limit data available."}
          </p>
        ) : null}
        {providers.length > 0 ? (
          <div className={`grid min-w-0 grid-cols-2 gap-x-4 gap-y-5${loadState === "error" ? " mt-3" : ""}`}>
            {providers.map((p) => (
              <div key={`${p.provider}:${p.profileId}`} className="min-w-0">
                <div className="mb-2 flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
                  <span className="min-w-0 break-words text-xs font-medium leading-4 text-th-text-primary">{p.provider}</span>
                  {p.stale ? <span className="text-[11px] leading-4 text-th-text-muted">{isKo ? "지연" : "Stale"}</span> : null}
                </div>
                {p.unsupported || p.buckets.length === 0 ? (
                  <p className="break-words text-[11px] leading-4 text-th-text-muted">
                    {p.unsupported
                      ? p.reason ?? (isKo ? "한도 측정을 지원하지 않습니다." : "Rate-limit telemetry unavailable.")
                      : isKo ? "버킷 데이터가 없습니다." : "No bucket data."}
                  </p>
                ) : (
                  <div className="space-y-3">
                    {p.buckets.map((bucket) => <HomeSummaryBucket key={bucket.id} bucket={bucket} isKo={isKo} />)}
                  </div>
                )}
              </div>
            ))}
          </div>
        ) : null}
      </div>
    );
  }

  if (providers.length === 0) return null;

  const renderComfortableProvider = (p: RLProvider) => {
    const providerMeta = getProviderMeta(p.provider);
    const visible = p.buckets;
    return (
      <div key={`${p.provider}:${p.profileId}`} className="space-y-1.5">
        <div className="flex items-center gap-1">
          <span
            className="text-sm font-bold uppercase truncate"
            style={{ color: providerMeta.color }}
          >
            {(RL_ICONS[p.provider] ?? "•")} {p.provider}
          </span>
          {p.stale ? (
            <span
              className="rounded px-1 text-[8px] font-medium shrink-0"
              style={{
                color: "var(--warn)",
                background:
                  "color-mix(in oklch, var(--warn) 14%, var(--bg-2) 86%)",
                border:
                  "1px solid color-mix(in oklch, var(--warn) 28%, var(--line) 72%)",
              }}
            >
              {isKo ? "지연" : "STALE"}
            </span>
          ) : null}
        </div>
        {p.unsupported || visible.length === 0 ? (
          <div className="flex items-center gap-2 overflow-hidden">
            <span
              className="rounded px-1.5 py-0.5 text-[10px] font-semibold shrink-0"
              style={{
                color: "var(--fg-dim)",
                background:
                  "color-mix(in oklch, var(--fg-faint) 10%, var(--bg-2) 90%)",
                border:
                  "1px solid color-mix(in oklch, var(--fg-faint) 20%, var(--line) 80%)",
              }}
            >
              {p.unsupported ? "N/A" : (isKo ? "비어있음" : "EMPTY")}
            </span>
            <span
              className="truncate text-[11px]"
              style={{ color: "var(--th-text-muted)" }}
            >
              {p.unsupported
                ? p.reason ??
                  (isKo
                    ? "한도 텔레메트리 미지원"
                    : "Rate-limit telemetry unavailable")
                : isKo
                  ? "표시할 버킷 데이터 없음"
                  : "No bucket data"}
            </span>
          </div>
        ) : (
          <div className="grid grid-cols-1 gap-y-1.5">
            {visible.map((b) => (
              <RateLimitBucketGauge
                key={b.id}
                bucket={b}
                provider={p.provider}
                isKo={isKo}
                density="comfortable"
              />
            ))}
          </div>
        )}
      </div>
    );
  };

  const renderHomeWideProvider = (p: RLProvider) => {
    const providerMeta = getProviderMeta(p.provider);
    const visible = p.buckets;
    return (
      <div key={`${p.provider}:${p.profileId}`} className="min-w-0 space-y-1.5">
        <div className="flex h-5 min-w-0 items-center gap-1">
          <span
            className="truncate text-sm font-bold uppercase"
            style={{ color: providerMeta.color }}
          >
            {(RL_ICONS[p.provider] ?? "•")} {p.provider}
          </span>
          {p.stale ? (
            <span
              className="rounded px-1 text-[8px] font-medium shrink-0"
              style={{
                color: "var(--warn)",
                background:
                  "color-mix(in oklch, var(--warn) 14%, var(--bg-2) 86%)",
                border:
                  "1px solid color-mix(in oklch, var(--warn) 28%, var(--line) 72%)",
              }}
            >
              {isKo ? "지연" : "STALE"}
            </span>
          ) : null}
        </div>
        {p.unsupported || visible.length === 0 ? (
          <div className="flex items-center gap-2 overflow-hidden">
            <span
              className="rounded px-1.5 py-0.5 text-[9px] font-semibold shrink-0"
              style={{
                color: "var(--fg-dim)",
                background:
                  "color-mix(in oklch, var(--fg-faint) 10%, var(--bg-2) 90%)",
                border:
                  "1px solid color-mix(in oklch, var(--fg-faint) 20%, var(--line) 80%)",
              }}
            >
              {p.unsupported ? "N/A" : (isKo ? "비어있음" : "EMPTY")}
            </span>
            <span
              className="truncate text-[11px]"
              style={{ color: "var(--th-text-muted)" }}
            >
              {p.unsupported
                ? p.reason ??
                  (isKo
                    ? "한도 텔레메트리 미지원"
                    : "Rate-limit telemetry unavailable")
                : isKo
                  ? "표시할 버킷 데이터 없음"
                  : "No bucket data"}
            </span>
          </div>
        ) : (
          <div className="grid grid-cols-1 gap-y-1">
            {visible.map((b) => (
              <RateLimitBucketGauge
                key={b.id}
                bucket={b}
                provider={p.provider}
                isKo={isKo}
                density="compact"
              />
            ))}
          </div>
        )}
      </div>
    );
  };

  if (layout === "homeWide") {
    return (
      <div className="mt-3">
        <div className="hidden min-w-0 gap-4 lg:grid lg:grid-cols-2">
          {providers.map(renderHomeWideProvider)}
        </div>
        <div className="space-y-3 lg:hidden">
          {providers.map(renderComfortableProvider)}
        </div>
      </div>
    );
  }

  if (isComfy) {
    return (
      <div className="mt-3 space-y-3">
        {providers.map(renderComfortableProvider)}
      </div>
    );
  }

  return (
    <div className="mt-2 space-y-1">
      {providers.map((p) => {
        const providerMeta = getProviderMeta(p.provider);
        const visible = p.buckets;
        return (
          <div key={`${p.provider}:${p.profileId}`} className="flex items-center gap-0">
            <div className="flex items-center gap-1 shrink-0" style={{ width: 96 }}>
              <span
                className="text-xs font-bold uppercase truncate"
                style={{ color: providerMeta.color }}
              >
                {(RL_ICONS[p.provider] ?? "•")} {p.provider}
              </span>
              {p.stale ? (
                <span
                  className="rounded px-0.5 text-[7px] font-medium shrink-0"
                  style={{
                    color: "var(--warn)",
                    background:
                      "color-mix(in oklch, var(--warn) 14%, var(--bg-2) 86%)",
                    border:
                      "1px solid color-mix(in oklch, var(--warn) 28%, var(--line) 72%)",
                  }}
                >
                  {isKo ? "지연" : "STALE"}
                </span>
              ) : null}
            </div>
            {p.unsupported || visible.length === 0 ? (
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2 overflow-hidden">
                  <span
                    className="rounded px-1.5 py-0.5 text-[9px] font-semibold shrink-0"
                    style={{
                      color: "var(--fg-dim)",
                      background:
                        "color-mix(in oklch, var(--fg-faint) 10%, var(--bg-2) 90%)",
                      border:
                        "1px solid color-mix(in oklch, var(--fg-faint) 20%, var(--line) 80%)",
                    }}
                  >
                    {p.unsupported ? "N/A" : (isKo ? "비어있음" : "EMPTY")}
                  </span>
                  <span className="truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {p.unsupported
                      ? (p.reason ?? (isKo ? "한도 텔레메트리 미지원" : "Rate-limit telemetry unavailable"))
                      : (isKo ? "표시할 버킷 데이터 없음" : "No bucket data")}
                  </span>
                </div>
              </div>
            ) : (
              <div className="flex-1 grid grid-cols-2 gap-x-2">
                {visible.map((b) => (
                  <RateLimitBucketGauge
                    key={b.id}
                    bucket={b}
                    provider={p.provider}
                    isKo={isKo}
                    density="compact"
                  />
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
