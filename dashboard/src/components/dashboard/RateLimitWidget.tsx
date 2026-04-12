import { useEffect, useState } from "react";
import TooltipLabel from "../common/TooltipLabel";
import type { TFunction } from "./model";

interface RateLimitBucket {
  id: string;
  label: string;
  utilization: number;
  resets_at: string | null;
  level: "normal" | "warning" | "danger";
}

interface RateLimitProvider {
  provider: string;
  buckets: RateLimitBucket[];
  fetched_at: number;
  stale: boolean;
}

interface RateLimitData {
  providers: RateLimitProvider[];
}

/* --- Raw API types (from backend rate_limit_cache) --- */
interface RawBucket {
  name: string;
  limit: number;
  used: number;
  remaining: number;
  reset: number; // unix timestamp
}

interface RawProvider {
  provider: string;
  buckets: RawBucket[];
  fetched_at: number;
  stale: boolean;
}

interface RawRateLimitData {
  providers: RawProvider[];
}

/** Providers to exclude from UI display */
const HIDDEN_PROVIDERS = new Set(["github"]);

/** Bucket IDs to exclude from UI display */
const HIDDEN_BUCKETS = new Set(["7d Sonnet"]);

function transformRawData(
  raw: RawRateLimitData,
  warningPct: number,
  dangerPct: number,
): RateLimitData {
  return {
    providers: raw.providers
      .filter((rp) => !HIDDEN_PROVIDERS.has(rp.provider.toLowerCase()))
      .map((rp) => ({
        provider: rp.provider.charAt(0).toUpperCase() + rp.provider.slice(1),
        fetched_at: rp.fetched_at,
        stale: rp.stale,
        buckets: rp.buckets
          .filter((b) => !HIDDEN_BUCKETS.has(b.name))
          .map((b) => {
            const utilization = b.limit > 0 ? Math.round((b.used / b.limit) * 100) : 0;
            const level: "normal" | "warning" | "danger" =
              utilization >= dangerPct ? "danger" : utilization >= warningPct ? "warning" : "normal";
            return {
              id: b.name,
              label: b.name,
              utilization,
              resets_at: b.reset > 0 ? new Date(b.reset * 1000).toISOString() : null,
              level,
            };
          }),
      })),
  };
}

interface ProviderPalette {
  accent: string;
  normal: { bar: string; text: string; glow: string };
  warning: { bar: string; text: string; glow: string };
  danger: { bar: string; text: string; glow: string };
}

const PROVIDER_PALETTES: Record<string, ProviderPalette> = {
  Claude: {
    accent: "#f59e0b",
    normal: { bar: "#f59e0b", text: "#fbbf24", glow: "rgba(245,158,11,0.3)" },
    warning: { bar: "#ea580c", text: "#fb923c", glow: "rgba(234,88,12,0.4)" },
    danger: { bar: "#ef4444", text: "#fca5a5", glow: "rgba(239,68,68,0.5)" },
  },
  Codex: {
    accent: "#34d399",
    normal: { bar: "#34d399", text: "#6ee7b7", glow: "rgba(52,211,153,0.3)" },
    warning: { bar: "#fbbf24", text: "#fcd34d", glow: "rgba(251,191,36,0.4)" },
    danger: { bar: "#f87171", text: "#fca5a5", glow: "rgba(248,113,113,0.5)" },
  },
  Gemini: {
    accent: "#3b82f6",
    normal: { bar: "#3b82f6", text: "#60a5fa", glow: "rgba(59,130,246,0.3)" },
    warning: { bar: "#f59e0b", text: "#fbbf24", glow: "rgba(245,158,11,0.4)" },
    danger: { bar: "#ef4444", text: "#fca5a5", glow: "rgba(239,68,68,0.5)" },
  },
};

const DEFAULT_PALETTE: ProviderPalette = PROVIDER_PALETTES.Codex;
const PROVIDER_ICONS: Record<string, string> = {
  Claude: "🤖",
  Codex: "⚡",
  Gemini: "🔮",
};

function getColors(provider: string, level: string) {
  const palette = PROVIDER_PALETTES[provider] || DEFAULT_PALETTE;
  if (level === "danger") return palette.danger;
  if (level === "warning") return palette.warning;
  return palette.normal;
}

function getAccent(provider: string) {
  return (PROVIDER_PALETTES[provider] || DEFAULT_PALETTE).accent;
}

function formatTimeRemaining(resetsAt: string | null): string {
  if (!resetsAt) return "";
  const diff = new Date(resetsAt).getTime() - Date.now();
  if (diff <= 0) return "now";
  const days = Math.floor(diff / 86400000);
  const hours = Math.floor((diff % 86400000) / 3600000);
  const minutes = Math.floor((diff % 3600000) / 60000);
  if (days > 0) return `${days}d${hours}h`;
  if (hours > 0) return `${hours}h${minutes}m`;
  return `${minutes}m`;
}


interface RateLimitWidgetProps {
  t: TFunction;
  onOpenSettings?: () => void;
}

export default function RateLimitWidget({ t, onOpenSettings }: RateLimitWidgetProps) {
  const [data, setData] = useState<RateLimitData | null>(null);
  const [thresholds, setThresholds] = useState({ warning: 80, danger: 95 });
  const title = t({ ko: "프로바이더 상태", en: "Provider Status", ja: "プロバイダー状態", zh: "Provider 状态" });
  const tooltip = t({
    ko: "Claude/Codex/Gemini provider 버킷 사용량과 stale 캐시 여부를 빠르게 보여줍니다. STALE은 최근 동기화 결과가 늦어졌다는 뜻입니다.",
    en: "Quick view of Claude/Codex/Gemini provider bucket usage and cache freshness. STALE means the latest sync result is lagging.",
    ja: "Claude/Codex/Gemini provider の bucket 使用量と cache freshness を素早く確認します。STALE は直近の同期結果が遅れている状態です。",
    zh: "快速查看 Claude/Codex/Gemini provider bucket 使用量与缓存新鲜度。STALE 表示最近一次同步结果已落后。",
  });

  useEffect(() => {
    (async () => {
      try {
        const res = await fetch("/api/settings/runtime-config", { credentials: "include" });
        if (!res.ok) return;
        const s = await res.json();
        const current = s.current ?? s;
        setThresholds({
          warning: current.rateLimitWarningPct ?? 80,
          danger: current.rateLimitDangerPct ?? 95,
        });
      } catch { /* ignore */ }
    })();
  }, []);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const res = await fetch("/api/rate-limits", { credentials: "include" });
        if (!res.ok) return;
        const raw = (await res.json()) as RawRateLimitData;
        if (mounted) setData(transformRawData(raw, thresholds.warning, thresholds.danger));
      } catch { /* ignore */ }
    };
    load();
    const timer = setInterval(load, 30_000);
    return () => { mounted = false; clearInterval(timer); };
  }, [thresholds]);

  if (!data || !data.providers || data.providers.length === 0) return null;

  return (
    <div className="game-panel relative overflow-hidden px-3 py-3 sm:px-4 sm:py-3.5">
      <div className="mb-3 flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div style={{ color: "#60a5fa" }}>
            <TooltipLabel
              text={title}
              tooltip={tooltip}
              className="text-[10px] sm:text-xs font-bold uppercase tracking-wider"
            />
          </div>
          <div className="text-[10px]" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "Provider 버킷 사용량과 stale 캐시 상태",
              en: "Provider bucket utilization and stale cache status",
              ja: "Provider バケット使用量と stale cache 状態",
              zh: "Provider bucket 使用量与 stale cache 状态",
            })}
          </div>
        </div>
        {onOpenSettings && (
          <button
            type="button"
            onClick={onOpenSettings}
            className="shrink-0 rounded-lg px-3 py-2 text-[11px] font-medium"
            style={{
              color: "#93c5fd",
              border: "1px solid rgba(96,165,250,0.35)",
              background: "rgba(59,130,246,0.12)",
            }}
          >
            {t({ ko: "임계치 설정", en: "Thresholds", ja: "閾値設定", zh: "阈值设置" })}
          </button>
        )}
      </div>

      <div className="flex flex-col gap-1.5 sm:flex-row sm:items-center sm:gap-x-6">
        {data.providers.map((provider) => {
          const accent = getAccent(provider.provider);
          return (
            <div key={provider.provider} className="flex items-center gap-0 min-w-0">
              {/* Fixed-width left: provider + stale */}
              <div className="flex items-center gap-1 shrink-0" style={{ width: 120 }}>
                <span
                  className="text-xs font-bold uppercase tracking-wider whitespace-nowrap"
                  style={{ color: accent }}
                >
                  {(PROVIDER_ICONS[provider.provider] ?? "•")}{" "}
                  {provider.provider}
                </span>
                {provider.stale ? (
                  <span
                    className="rounded px-1 py-px text-[10px] leading-tight font-medium shrink-0"
                    style={{ color: "#fbbf24", background: "rgba(251,191,36,0.1)", border: "1px solid rgba(251,191,36,0.2)" }}
                  >
                    {t({ ko: "지연", en: "STALE", ja: "遅延", zh: "延迟" })}
                  </span>
                ) : null}
              </div>
              {/* Buckets — flat row, refresh absolute below */}
              <div className="flex-1 grid grid-cols-2 gap-x-4 sm:gap-x-5">
                {provider.buckets.map((bucket) => {
                  const colors = getColors(provider.provider, bucket.level);
                  const remaining = formatTimeRemaining(bucket.resets_at);
                  return (
                    <div key={bucket.id} className="relative flex items-center gap-1.5 sm:gap-2">
                      <span
                        className="text-xs font-bold shrink-0"
                        style={{ color: colors.text, minWidth: 18 }}
                      >
                        {bucket.label}
                      </span>
                      <div className="flex-1" style={{ minWidth: 60 }}>
                        <div
                          className="relative rounded-full overflow-hidden"
                          style={{
                            height: 10,
                            background: "rgba(255,255,255,0.12)",
                            border: "1px solid rgba(255,255,255,0.08)",
                          }}
                        >
                          <div
                            className="absolute inset-y-0 left-0 rounded-full transition-all duration-500"
                            style={{
                              width: `${Math.max(Math.min(bucket.utilization, 100), 2)}%`,
                              background: colors.bar,
                              boxShadow: `0 0 ${bucket.level !== "normal" ? "8" : "4"}px ${colors.glow}`,
                            }}
                          />
                        </div>
                      </div>
                      <span
                        className="text-xs font-mono font-bold shrink-0"
                        style={{
                          color: colors.text,
                          textShadow: bucket.level === "danger" ? `0 0 6px ${colors.glow}` : "none",
                        }}
                      >
                        {bucket.utilization}%
                      </span>
                      {remaining && (
                        <span
                          className="absolute whitespace-nowrap text-[8px]"
                          style={{ color: "var(--th-text-muted)", top: "calc(100% + 1px)", left: 0, lineHeight: 1 }}
                        >
                          ↻ {remaining}
                        </span>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
