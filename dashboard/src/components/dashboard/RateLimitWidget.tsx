import { useEffect, useState } from "react";
import TooltipLabel from "../common/TooltipLabel";
import type { TFunction } from "./model";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceSection,
} from "../common/SurfacePrimitives";

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
    <SurfaceSection
      eyebrow={t({ ko: "운영", en: "Operations", ja: "運用", zh: "运营" })}
      title={title}
      description={t({
        ko: "Claude/Codex/Gemini 버킷 사용량과 stale 캐시 상태를 한눈에 확인합니다.",
        en: "Track Claude/Codex/Gemini bucket utilization and stale cache state at a glance.",
        ja: "Claude/Codex/Gemini バケット使用量と stale キャッシュ状態をひと目で確認します。",
        zh: "一眼查看 Claude/Codex/Gemini bucket 使用率与 stale cache 状态。",
      })}
      actions={(
        <>
          <TooltipLabel
            text={t({ ko: "설명", en: "About", ja: "説明", zh: "说明" })}
            tooltip={tooltip}
            className="max-w-fit text-sm"
          />
          {onOpenSettings ? (
            <SurfaceActionButton onClick={onOpenSettings} tone="info" compact>
              {t({ ko: "임계치 설정", en: "Thresholds", ja: "閾値設定", zh: "阈值设置" })}
            </SurfaceActionButton>
          ) : undefined}
        </>
      )}
    >
      <div className="mt-4 grid gap-3 xl:grid-cols-3">
        {data.providers.map((provider) => {
          const accent = getAccent(provider.provider);
          return (
            <SurfaceCard
              key={provider.provider}
              className="rounded-3xl p-4"
              style={{
                borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
                background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
              }}
            >
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0">
                  <div
                    className="text-xs font-bold uppercase tracking-wider"
                    style={{ color: accent }}
                  >
                    {(PROVIDER_ICONS[provider.provider] ?? "•")} {provider.provider}
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {provider.stale
                      ? t({ ko: "캐시 지연 상태", en: "Stale cache", ja: "キャッシュ遅延", zh: "缓存延迟" })
                      : t({ ko: "정상 수집 중", en: "Fresh cache", ja: "正常取得中", zh: "缓存正常" })}
                  </div>
                </div>
                <span
                  className="rounded-full px-2 py-1 text-[10px] font-medium"
                  style={{
                    color: provider.stale ? "#fbbf24" : accent,
                    border: `1px solid ${provider.stale ? "rgba(251,191,36,0.3)" : `color-mix(in srgb, ${accent} 24%, var(--th-border) 76%)`}`,
                    background: provider.stale
                      ? "rgba(251,191,36,0.1)"
                      : `color-mix(in srgb, ${accent} 10%, var(--th-bg-surface) 90%)`,
                  }}
                >
                  {provider.stale
                    ? t({ ko: "지연", en: "STALE", ja: "遅延", zh: "延迟" })
                    : t({ ko: "정상", en: "FRESH", ja: "正常", zh: "正常" })}
                </span>
              </div>

              <div className="mt-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
                {provider.buckets.map((bucket) => {
                  const colors = getColors(provider.provider, bucket.level);
                  const remaining = formatTimeRemaining(bucket.resets_at);
                  return (
                    <div key={bucket.id} className="relative">
                      <div className="mb-1.5 flex items-center justify-between gap-2">
                        <span
                          className="text-xs font-bold"
                          style={{ color: colors.text }}
                        >
                          {bucket.label}
                        </span>
                        <span
                          className="text-xs font-mono font-bold"
                          style={{
                            color: colors.text,
                            textShadow: bucket.level === "danger" ? `0 0 6px ${colors.glow}` : "none",
                          }}
                        >
                          {bucket.utilization}%
                        </span>
                      </div>
                      <div style={{ minWidth: 60 }}>
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
                      {remaining && (
                        <span
                          className="mt-1 inline-flex whitespace-nowrap text-[10px]"
                          style={{ color: "var(--th-text-muted)", lineHeight: 1.2 }}
                        >
                          ↻ {remaining}
                        </span>
                      )}
                    </div>
                  );
                })}
              </div>
            </SurfaceCard>
          );
        })}
      </div>
    </SurfaceSection>
  );
}
