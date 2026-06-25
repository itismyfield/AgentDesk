import { useEffect, useState } from "react";
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

export function MiniRateLimitBar({
  isKo,
  density = "compact",
}: {
  isKo: boolean;
  density?: "compact" | "comfortable";
}) {
  const [providers, setProviders] = useState<RLProvider[]>([]);
  const isComfy = density === "comfortable";

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const res = await fetch("/api/rate-limits", { credentials: "include" });
        if (!res.ok) return;
        const json = await res.json() as { providers: RawRLProvider[] };
        if (mounted) setProviders(transformRLProviders(json.providers ?? []));
      } catch { /* ignore */ }
    };
    load();
    const timer = setInterval(load, 30_000);
    return () => { mounted = false; clearInterval(timer); };
  }, []);

  if (providers.length === 0) return null;

  if (isComfy) {
    return (
      <div className="mt-3 space-y-3">
        {providers.map((p) => {
          const providerMeta = getProviderMeta(p.provider);
          const visible = p.buckets;
          return (
            <div key={p.provider} className="space-y-1.5">
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
        })}
      </div>
    );
  }

  return (
    <div className="mt-2 space-y-1">
      {providers.map((p) => {
        const providerMeta = getProviderMeta(p.provider);
        const visible = p.buckets;
        return (
          <div key={p.provider} className="flex items-center gap-0">
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
