import { useEffect, useState } from "react";
import TooltipLabel from "../common/TooltipLabel";
import type { TFunction } from "./model";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSection,
} from "../common/SurfacePrimitives";
import {
  getProviderLevelColors,
  getProviderMeta,
} from "../../app/providerTheme";

interface RateLimitBucket {
  id: string;
  label: string;
  utilization: number | null;
  resets_at: string | null;
  level: "normal" | "warning" | "danger";
}

interface RateLimitProvider {
  provider: string;
  buckets: RateLimitBucket[];
  fetched_at: number;
  stale: boolean;
  unsupported: boolean;
  reason: string | null;
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
  unsupported?: boolean;
  reason?: string | null;
}

interface RawRateLimitData {
  providers: RawProvider[];
}

/** Providers to exclude from UI display */
const HIDDEN_PROVIDERS = new Set(["github"]);

/** Bucket IDs to exclude from UI display */
const HIDDEN_BUCKETS = new Set(["7d Sonnet"]);
const RATE_LIMIT_FETCH_TIMEOUT_MS = 15_000;

export function normalizeRateLimitProviderLabel(provider: string): string {
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

export function transformRawData(
  raw: RawRateLimitData,
  warningPct: number,
  dangerPct: number,
): RateLimitData {
  return {
    providers: raw.providers
      .filter((rp) => !HIDDEN_PROVIDERS.has(rp.provider.toLowerCase()))
      .flatMap((rp) => {
        const buckets = rp.buckets
          .filter((b) => !HIDDEN_BUCKETS.has(b.name))
          .map((b) => {
            const utilization =
              b.limit > 0 && b.used >= 0 && b.remaining >= 0
                ? Math.round((b.used / b.limit) * 100)
                : null;
            const level: "normal" | "warning" | "danger" =
              utilization !== null && utilization >= dangerPct
                ? "danger"
                : utilization !== null && utilization >= warningPct
                  ? "warning"
                  : "normal";
            return {
              id: b.name,
              label: b.name,
              utilization,
              resets_at: b.reset > 0 ? new Date(b.reset * 1000).toISOString() : null,
              level,
            };
          });
        if (rp.unsupported && buckets.length === 0) {
          return [];
        }
        return [
          {
            provider: normalizeRateLimitProviderLabel(rp.provider),
            fetched_at: rp.fetched_at,
            stale: rp.stale,
            unsupported: Boolean(rp.unsupported),
            reason: typeof rp.reason === "string" ? rp.reason : null,
            buckets,
          },
        ];
      }),
  };
}

const PROVIDER_ICONS: Record<string, string> = {
  Claude: "🤖",
  Codex: "⚡",
  Gemini: "🔮",
  Qwen: "🧠",
};

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

function createTimedController(timeoutMs: number) {
  const controller = new AbortController();
  const timeoutId = window.setTimeout(() => controller.abort(), timeoutMs);
  return {
    controller,
    cleanup: () => window.clearTimeout(timeoutId),
  };
}

interface RateLimitWidgetProps {
  t: TFunction;
  onOpenSettings?: () => void;
}

export default function RateLimitWidget({ t, onOpenSettings }: RateLimitWidgetProps) {
  const [data, setData] = useState<RateLimitData | null>(null);
  const [thresholds, setThresholds] = useState({ warning: 80, danger: 95 });
  const [error, setError] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(true);
  const title = t({
    ko: "프로바이더 상태",
    en: "Provider Status",
    ja: "プロバイダー状態",
    zh: "Provider 状态",
  });
  const tooltip = t({
    ko: "Claude/Codex/Gemini/OpenCode/Qwen provider 버킷 사용량과 stale 캐시 여부를 빠르게 보여줍니다. 지원되지 않는 provider 는 별도 상태로 표시됩니다.",
    en: "Quick view of Claude/Codex/Gemini/OpenCode/Qwen provider bucket usage and cache freshness. Unsupported providers are shown with a separate state.",
    ja: "Claude/Codex/Gemini/OpenCode/Qwen provider の bucket 使用量と cache freshness を素早く確認します。未対応 provider は別状態で表示します。",
    zh: "快速查看 Claude/Codex/Gemini/OpenCode/Qwen provider bucket 使用量与缓存新鲜度。未支持的 provider 会以单独状态显示。",
  });

  useEffect(() => {
    const { controller, cleanup } = createTimedController(RATE_LIMIT_FETCH_TIMEOUT_MS);

    (async () => {
      try {
        const res = await fetch("/api/settings/runtime-config", {
          credentials: "include",
          signal: controller.signal,
        });
        if (!res.ok) return;
        const s = await res.json();
        const current = s.current ?? s;
        setThresholds({
          warning: current.rateLimitWarningPct ?? 80,
          danger: current.rateLimitDangerPct ?? 95,
        });
      } catch {
        // Keep default thresholds when runtime config is temporarily unavailable.
      } finally {
        cleanup();
      }
    })();

    return () => {
      cleanup();
      controller.abort();
    };
  }, []);

  useEffect(() => {
    let activeController: AbortController | null = null;

    const load = async () => {
      activeController?.abort();
      const { controller, cleanup } = createTimedController(RATE_LIMIT_FETCH_TIMEOUT_MS);
      activeController = controller;
      setIsRefreshing(true);

      try {
        const res = await fetch("/api/rate-limits", {
          credentials: "include",
          signal: controller.signal,
        });
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        const raw = (await res.json()) as RawRateLimitData;
        if (controller.signal.aborted) return;
        setData(transformRawData(raw, thresholds.warning, thresholds.danger));
        setError(null);
      } catch (nextError) {
        if (controller.signal.aborted) return;
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        cleanup();
        if (!controller.signal.aborted) setIsRefreshing(false);
      }
    };

    void load();
    const timer = window.setInterval(() => void load(), 30_000);
    return () => {
      activeController?.abort();
      window.clearInterval(timer);
    };
  }, [thresholds]);

  const sectionActions = onOpenSettings ? (
    <>
      <TooltipLabel
        text={t({ ko: "설명", en: "About", ja: "説明", zh: "说明" })}
        tooltip={tooltip}
        className="max-w-fit text-sm"
      />
      <SurfaceActionButton onClick={onOpenSettings} tone="info" compact>
        {t({ ko: "임계치 설정", en: "Thresholds", ja: "閾値設定", zh: "阈值设置" })}
      </SurfaceActionButton>
    </>
  ) : (
    <TooltipLabel
      text={t({ ko: "설명", en: "About", ja: "説明", zh: "说明" })}
      tooltip={tooltip}
      className="max-w-fit text-sm"
    />
  );
  const providers = data?.providers ?? [];
  const hasProviders = providers.length > 0;

  return (
    <SurfaceSection
      eyebrow={t({ ko: "운영", en: "Operations", ja: "運用", zh: "运营" })}
      title={title}
      description={t({
        ko: "Provider 버킷 사용량과 stale 캐시 상태를 한눈에 확인합니다.",
        en: "Track provider bucket utilization, stale cache state, and unsupported telemetry at a glance.",
        ja: "Provider バケット使用量、stale キャッシュ状態、未対応テレメトリをひと目で確認します。",
        zh: "一眼查看 Provider bucket 使用率、stale cache 状态与未支持遥测。",
      })}
      actions={sectionActions}
    >
      {error && hasProviders ? (
        <SurfaceNotice className="mt-4" compact tone="warn">
          {t({
            ko: `최근 정상 데이터를 유지 중이며 새 동기화에 실패했습니다. (${error})`,
            en: `Keeping the last good snapshot because the latest refresh failed. (${error})`,
            ja: `直近の正常データを維持しつつ、最新の再同期に失敗しました。(${error})`,
            zh: `正在保留最近一次正常数据，最新刷新失败。(${error})`,
          })}
        </SurfaceNotice>
      ) : null}

      {!hasProviders ? (
        <SurfaceEmptyState className="mt-4 rounded-3xl p-4 text-sm leading-6">
          {isRefreshing
            ? t({
                ko: "프로바이더 상태를 불러오는 중입니다.",
                en: "Loading provider status.",
                ja: "プロバイダー状態を読み込み中です。",
                zh: "正在加载 provider 状态。",
              })
            : error
              ? t({
                  ko: `프로바이더 상태를 불러오지 못했습니다. ${error}`,
                  en: `Unable to load provider status. ${error}`,
                  ja: `プロバイダー状態を読み込めませんでした。${error}`,
                  zh: `无法加载 provider 状态。${error}`,
                })
              : t({
                  ko: "표시할 프로바이더 상태가 없습니다.",
                  en: "No provider status is available yet.",
                  ja: "表示できるプロバイダー状態がまだありません。",
                  zh: "暂无可显示的 provider 状态。",
                })}
        </SurfaceEmptyState>
      ) : (
        <div className="mt-4 grid gap-4 xl:grid-cols-3">
          {providers.map((provider) => {
            const providerMeta = getProviderMeta(provider.provider);
            const accent = providerMeta.color;
            const statusLabel = provider.unsupported
              ? t({ ko: "미지원", en: "N/A", ja: "未対応", zh: "未支持" })
              : provider.stale
                ? t({ ko: "지연", en: "STALE", ja: "遅延", zh: "延迟" })
                : t({ ko: "정상", en: "FRESH", ja: "正常", zh: "正常" });

            return (
              <SurfaceCard
                key={provider.provider}
                className="rounded-3xl p-5"
                style={{
                  borderColor: providerMeta.border,
                  background: providerMeta.bg,
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
                      {provider.unsupported
                        ? t({
                            ko: "한도 텔레메트리 미지원",
                            en: "Rate-limit telemetry unavailable",
                            ja: "制限テレメトリ未対応",
                            zh: "限额遥测未支持",
                          })
                        : provider.stale
                          ? t({
                              ko: "캐시 지연 상태",
                              en: "Stale cache",
                              ja: "キャッシュ遅延",
                              zh: "缓存延迟",
                            })
                          : t({
                              ko: "정상 수집 중",
                              en: "Fresh cache",
                              ja: "正常取得中",
                              zh: "缓存正常",
                            })}
                    </div>
                  </div>
                  <span
                    className="rounded-full px-2 py-1 text-[10px] font-medium"
                    style={{
                      color: provider.unsupported
                        ? "var(--fg-dim)"
                        : provider.stale
                          ? "var(--warn)"
                          : accent,
                      border: `1px solid ${
                        provider.unsupported
                          ? "color-mix(in oklch, var(--fg-faint) 24%, var(--line) 76%)"
                          : provider.stale
                            ? "color-mix(in oklch, var(--warn) 28%, var(--line) 72%)"
                            : providerMeta.border
                      }`,
                      background: provider.unsupported
                        ? "color-mix(in oklch, var(--fg-faint) 10%, var(--bg-2) 90%)"
                        : provider.stale
                          ? "color-mix(in oklch, var(--warn) 12%, var(--bg-2) 88%)"
                          : providerMeta.bg,
                    }}
                  >
                    {statusLabel}
                  </span>
                </div>

                {provider.unsupported || provider.buckets.length === 0 ? (
                  <div
                    className="mt-4 rounded-2xl border px-3 py-3"
                    style={{
                      borderColor: "color-mix(in oklch, var(--fg-faint) 20%, var(--line) 80%)",
                      background: "color-mix(in oklch, var(--fg-faint) 6%, var(--th-bg-surface) 94%)",
                    }}
                  >
                    <div className="text-xs font-semibold" style={{ color: "var(--th-text)" }}>
                      {provider.unsupported
                        ? t({
                            ko: "현재 이 provider 는 한도 버킷 집계를 제공하지 않습니다.",
                            en: "This provider does not expose rate-limit bucket telemetry yet.",
                            ja: "この provider はまだ制限バケットのテレメトリを提供していません。",
                            zh: "该 provider 暂未提供限额 bucket 遥测。",
                          })
                        : t({
                            ko: "표시할 버킷 데이터가 없습니다.",
                            en: "No bucket data is available yet.",
                            ja: "表示できるバケットデータがありません。",
                            zh: "暂时没有可显示的 bucket 数据。",
                          })}
                    </div>
                    {provider.reason ? (
                      <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                        {provider.reason}
                      </div>
                    ) : null}
                  </div>
                ) : (
                  <div className="mt-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
                    {provider.buckets.map((bucket) => {
                      const colors = getProviderLevelColors(provider.provider, bucket.level);
                      const remaining = formatTimeRemaining(bucket.resets_at);
                      return (
                        <div key={bucket.id} className="relative">
                          <div className="mb-1.5 flex items-center justify-between gap-2">
                            <span className="text-xs font-bold" style={{ color: colors.text }}>
                              {bucket.label}
                            </span>
                            <span
                              className="text-xs font-mono font-bold"
                              style={{
                                color:
                                  bucket.utilization === null
                                    ? "var(--th-text-muted)"
                                    : colors.text,
                                textShadow:
                                  bucket.utilization !== null && bucket.level === "danger"
                                    ? `0 0 6px ${colors.glow}`
                                    : "none",
                              }}
                            >
                              {bucket.utilization === null ? "N/A" : `${bucket.utilization}%`}
                            </span>
                          </div>
                          <div style={{ minWidth: 60 }}>
                            <div
                              className="relative overflow-hidden rounded-full"
                              style={{
                                height: 10,
                                background: "var(--line-soft)",
                                border: "1px solid color-mix(in oklch, var(--line) 60%, transparent)",
                              }}
                            >
                              <div
                                className="absolute inset-y-0 left-0 rounded-full transition-all duration-500"
                                style={{
                                  width:
                                    bucket.utilization === null
                                      ? "0%"
                                      : `${Math.max(Math.min(bucket.utilization, 100), 2)}%`,
                                  background:
                                    bucket.utilization === null
                                      ? "transparent"
                                      : colors.bar,
                                  boxShadow:
                                    bucket.utilization === null
                                      ? "none"
                                      : `0 0 ${bucket.level !== "normal" ? "8" : "4"}px ${colors.glow}`,
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
                )}
              </SurfaceCard>
            );
          })}
        </div>
      )}
    </SurfaceSection>
  );
}
