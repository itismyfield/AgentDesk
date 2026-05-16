import { useEffect, useState } from "react";
import {
  getProviderLevelColors,
  getProviderMeta,
} from "../../app/providerTheme";

interface RLBucket {
  id: string;
  label: string;
  utilization: number | null;
  level: "normal" | "warning" | "danger";
}

interface RLProvider {
  provider: string;
  buckets: RLBucket[];
  stale: boolean;
  unsupported: boolean;
  reason: string | null;
}

interface RawRLBucket {
  name: string;
  limit: number;
  used: number;
  remaining: number;
  reset: number;
}

interface RawRLProvider {
  provider: string;
  buckets: RawRLBucket[];
  stale: boolean;
  unsupported?: boolean;
  reason?: string | null;
}

const RL_HIDDEN_PROVIDERS = new Set(["github"]);
const RL_HIDDEN_BUCKETS = new Set(["7d Sonnet"]);

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

export function transformRLProviders(raw: RawRLProvider[]): RLProvider[] {
  return raw
    .filter((rp) => !RL_HIDDEN_PROVIDERS.has(rp.provider.toLowerCase()))
    .flatMap((rp) => {
      const buckets = rp.buckets
        .filter((b) => !RL_HIDDEN_BUCKETS.has(b.name))
        .map((b) => {
          const utilization =
            b.limit > 0 && b.used >= 0 && b.remaining >= 0
              ? Math.round((b.used / b.limit) * 100)
              : null;
          return {
            id: b.name,
            label: b.name,
            utilization,
            level: (
              utilization !== null && utilization >= 95
                ? "danger"
                : utilization !== null && utilization >= 80
                  ? "warning"
                  : "normal"
            ) as "normal" | "warning" | "danger",
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
                <div className="grid grid-cols-2 gap-x-3 gap-y-1">
                  {visible.map((b) => {
                    const accentText = getProviderLevelColors(p.provider, b.level).bar;
                    return (
                      <div key={b.id} className="flex items-center gap-1.5">
                        <span
                          className="text-xs font-bold shrink-0 w-[16px]"
                          style={{ color: accentText }}
                        >
                          {b.label}
                        </span>
                        <div className="flex-1 min-w-0">
                          <div
                            className="relative h-1.5 rounded-full overflow-hidden"
                            style={{ background: "var(--line-soft)" }}
                          >
                            <div
                              className="absolute inset-y-0 left-0 rounded-full"
                              style={{
                                width:
                                  b.utilization === null
                                    ? "0%"
                                    : `${Math.min(b.utilization, 100)}%`,
                                background:
                                  b.utilization === null
                                    ? "transparent"
                                    : getProviderLevelColors(p.provider, b.level)
                                        .bar,
                              }}
                            />
                          </div>
                        </div>
                        <span
                          className="text-xs font-mono font-bold shrink-0 w-[32px] text-right"
                          style={{
                            color:
                              b.utilization === null
                                ? "var(--th-text-muted)"
                                : accentText,
                          }}
                        >
                          {b.utilization === null ? "N/A" : `${b.utilization}%`}
                        </span>
                      </div>
                    );
                  })}
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
                {visible.map((b) => {
                  const accentText = getProviderLevelColors(p.provider, b.level).bar;
                  return (
                    <div key={b.id} className="flex items-center gap-1">
                      <span
                        className="text-xs font-bold shrink-0 w-[14px]"
                        style={{ color: accentText }}
                      >
                        {b.label}
                      </span>
                      <div className="flex-1 min-w-0">
                        <div
                          className="relative h-[3px] rounded-full overflow-hidden"
                          style={{ background: "var(--line-soft)" }}
                        >
                          <div
                            className="absolute inset-y-0 left-0 rounded-full"
                            style={{
                              width: b.utilization === null ? "0%" : `${Math.min(b.utilization, 100)}%`,
                              background:
                                b.utilization === null
                                  ? "transparent"
                                  : getProviderLevelColors(p.provider, b.level).bar,
                            }}
                          />
                        </div>
                      </div>
                      <span
                        className="text-xs font-mono font-bold shrink-0 w-[28px] text-right"
                        style={{
                          color:
                            b.utilization === null ? "var(--th-text-muted)" : accentText,
                        }}
                      >
                        {b.utilization === null ? "N/A" : `${b.utilization}%`}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
