import { useEffect, useMemo, useState } from "react";
import type { TFunction } from "./model";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSegmentButton,
} from "../common/SurfacePrimitives";

interface ModelLineItem {
  model: string;
  display_name: string;
  total_tokens: number;
  cost: number;
  provider: string;
}

interface ProviderShare {
  provider: string;
  tokens: number;
  percentage: number;
}

interface ReceiptStats {
  total_messages: number;
  total_sessions: number;
}

interface AgentShare {
  agent: string;
  tokens: number;
  cost: number;
  percentage: number;
}

interface ReceiptData {
  period_label: string;
  period_start: string;
  period_end: string;
  models: ModelLineItem[];
  subtotal: number;
  cache_discount: number;
  total: number;
  stats: ReceiptStats;
  providers: ProviderShare[];
  agents: AgentShare[];
}

type Period = "today" | "week" | "month" | "all";

interface ReceiptWidgetProps {
  t: TFunction;
}

interface CachedReceiptEntry {
  data: ReceiptData;
  fetchedAt: number;
}

const receiptCache = new Map<Period, CachedReceiptEntry>();
const RECEIPT_CACHE_TTL: Record<Period, number> = {
  today: 30 * 60_000,
  week: 210 * 60_000,
  month: 15 * 60 * 60_000,
  all: 24 * 60 * 60_000,
};

function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return String(value);
}

function formatCost(value: number): string {
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
}

function providerKey(value: string): string {
  return value.trim().toLowerCase();
}

function buildProviderOptions(data: ReceiptData | null): string[] {
  if (!data) return [];

  const seen = new Set<string>();
  const providers: string[] = [];
  for (const provider of data.providers.map((item) => item.provider).concat(data.models.map((item) => item.provider))) {
    const key = providerKey(provider);
    if (!key || seen.has(key)) continue;
    seen.add(key);
    providers.push(provider);
  }
  return providers;
}

function deriveReceiptView(data: ReceiptData | null, selectedProvider: string): ReceiptData | null {
  if (!data) return null;
  if (selectedProvider === "all") return data;

  const filteredModels = data.models.filter((model) => providerKey(model.provider) === providerKey(selectedProvider));
  if (filteredModels.length === 0) return null;

  const filteredSubtotal = filteredModels.reduce((sum, model) => sum + model.cost, 0);
  const discountRatio = data.subtotal > 0 ? filteredSubtotal / data.subtotal : 0;
  const filteredDiscount = data.cache_discount * discountRatio;
  const filteredTokens = filteredModels.reduce((sum, model) => sum + model.total_tokens, 0);
  const providerLabel = filteredModels[0]?.provider ?? selectedProvider;

  return {
    ...data,
    models: filteredModels,
    subtotal: filteredSubtotal,
    cache_discount: filteredDiscount,
    total: Math.max(filteredSubtotal - filteredDiscount, 0),
    providers: [
      {
        provider: providerLabel,
        tokens: filteredTokens,
        percentage: 100,
      },
    ],
    agents: [],
  };
}

export default function ReceiptWidget({ t }: ReceiptWidgetProps) {
  const [data, setData] = useState<ReceiptData | null>(null);
  const [period, setPeriod] = useState<Period>("month");
  const [selectedProvider, setSelectedProvider] = useState("all");
  const [expanded, setExpanded] = useState(false);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    let mounted = true;

    const load = async () => {
      const cached = receiptCache.get(period);
      const ttl = RECEIPT_CACHE_TTL[period];
      const fresh = cached ? Date.now() - cached.fetchedAt < ttl : false;

      if (cached && mounted) {
        setData(cached.data);
      }
      if (fresh) {
        if (mounted) setLoading(false);
        return;
      }

      if (mounted) setLoading(true);
      try {
        const res = await fetch(`/api/receipt?period=${period}`, { credentials: "include" });
        if (!res.ok) return;
        const json = (await res.json()) as ReceiptData;
        receiptCache.set(period, { data: json, fetchedAt: Date.now() });
        if (mounted) setData(json);
      } catch {
        // ignore network/auth errors and keep cached data if present
      } finally {
        if (mounted) setLoading(false);
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, [period]);

  const providerOptions = useMemo(() => buildProviderOptions(data), [data]);
  const viewData = useMemo(() => deriveReceiptView(data, selectedProvider), [data, selectedProvider]);
  const totalTokens = useMemo(
    () => viewData?.models.reduce((sum, model) => sum + model.total_tokens, 0) ?? 0,
    [viewData],
  );
  const selectedProviderLabel = selectedProvider === "all"
    ? null
    : (viewData?.providers[0]?.provider ?? selectedProvider);

  useEffect(() => {
    if (selectedProvider === "all") return;
    const exists = providerOptions.some((provider) => providerKey(provider) === providerKey(selectedProvider));
    if (!exists) setSelectedProvider("all");
  }, [providerOptions, selectedProvider]);

  if (!viewData || viewData.models.length === 0) return null;

  const periods: { id: Period; label: string }[] = [
    { id: "today", label: t({ ko: "오늘", en: "Today", ja: "今日", zh: "今天" }) },
    { id: "week", label: t({ ko: "이번 주", en: "This Week", ja: "今週", zh: "本周" }) },
    { id: "month", label: t({ ko: "이번 달", en: "This Month", ja: "今月", zh: "本月" }) },
    { id: "all", label: t({ ko: "전체", en: "All", ja: "全期間", zh: "全部" }) },
  ];

  return (
    <SurfaceSection
      eyebrow={t({ ko: "Receipt", en: "Receipt", ja: "Receipt", zh: "Receipt" })}
      title={t({ ko: "토큰 영수증", en: "Token Receipt", ja: "トークンレシート", zh: "代币收据" })}
      description={t({
        ko: `${viewData.period_start} ~ ${viewData.period_end} 사용량과 비용 흐름을 빠르게 훑어봅니다.`,
        en: `Quick read of usage and cost from ${viewData.period_start} to ${viewData.period_end}.`,
        ja: `${viewData.period_start} から ${viewData.period_end} までの使用量とコストを確認します。`,
        zh: `快速查看 ${viewData.period_start} 到 ${viewData.period_end} 的用量与成本。`,
      })}
      badge={viewData.period_label}
      actions={(
        <div className="flex items-center gap-2">
          {loading && (
            <span
              className="rounded-full border px-2 py-1 text-[10px] font-medium"
              style={{
                color: "var(--th-accent-warn)",
                background: "color-mix(in srgb, var(--th-badge-amber-bg) 82%, transparent)",
                borderColor: "color-mix(in srgb, var(--th-accent-warn) 28%, var(--th-border) 72%)",
              }}
            >
              {t({ ko: "갱신 중", en: "SYNCING", ja: "更新中", zh: "更新中" })}
            </span>
          )}
          <SurfaceActionButton
            tone={expanded ? "neutral" : "warn"}
            compact
            onClick={() => setExpanded((prev) => !prev)}
          >
            {expanded
              ? t({ ko: "접기", en: "Collapse", ja: "閉じる", zh: "收起" })
              : t({ ko: "영수증 펼치기", en: "Expand Receipt", ja: "レシートを開く", zh: "展开收据" })}
          </SurfaceActionButton>
        </div>
      )}
      className="relative overflow-hidden"
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-badge-amber-bg) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)",
      }}
    >
      <div className="mt-4 flex flex-wrap gap-2">
        {periods.map((item) => (
          <SurfaceSegmentButton
            key={item.id}
            tone="warn"
            active={period === item.id}
            onClick={() => setPeriod(item.id)}
          >
            {item.label}
          </SurfaceSegmentButton>
        ))}
      </div>

      {providerOptions.length > 1 && (
        <div className="mt-3 flex gap-2 overflow-x-auto pb-1">
          <SurfaceSegmentButton
            tone="warn"
            active={selectedProvider === "all"}
            onClick={() => setSelectedProvider("all")}
          >
            {t({ ko: "전체 Provider", en: "All Providers", ja: "全 Provider", zh: "全部 Provider" })}
          </SurfaceSegmentButton>
          {providerOptions.map((provider) => {
            const active = providerKey(selectedProvider) === providerKey(provider);
            return (
              <SurfaceSegmentButton
                key={provider}
                tone="warn"
                active={active}
                onClick={() => setSelectedProvider(provider)}
              >
                {provider}
              </SurfaceSegmentButton>
            );
          })}
        </div>
      )}

      <div className="mt-4 flex flex-wrap gap-3">
        <SurfaceMetricPill
          label={t({ ko: "총 비용", en: "Total Cost", ja: "総コスト", zh: "总成本" })}
          value={(
            <span className="inline-flex items-center gap-2">
              <span style={{ color: "var(--th-accent-warn)" }}>{formatCost(viewData.total)}</span>
              {viewData.cache_discount > 0.001 && (
                <span className="text-[11px]" style={{ color: "var(--th-accent-primary)" }}>
                  -{formatCost(viewData.cache_discount)}
                </span>
              )}
            </span>
          )}
          tone="warn"
        />
        <SurfaceMetricPill
          label={t({ ko: "토큰 사용량", en: "Token Usage", ja: "トークン使用量", zh: "代币用量" })}
          value={t({
            ko: `${formatTokens(totalTokens)} tokens / ${viewData.models.length} lines`,
            en: `${formatTokens(totalTokens)} tokens / ${viewData.models.length} lines`,
            ja: `${formatTokens(totalTokens)} tokens / ${viewData.models.length} lines`,
            zh: `${formatTokens(totalTokens)} tokens / ${viewData.models.length} lines`,
          })}
          tone="info"
        />
        {selectedProvider === "all" ? (
          <SurfaceMetricPill
            label={t({ ko: "활동량", en: "Activity", ja: "アクティビティ", zh: "活动量" })}
            value={t({
              ko: `${viewData.stats.total_messages.toLocaleString()} msgs / ${viewData.stats.total_sessions} sessions`,
              en: `${viewData.stats.total_messages.toLocaleString()} msgs / ${viewData.stats.total_sessions} sessions`,
              ja: `${viewData.stats.total_messages.toLocaleString()} msgs / ${viewData.stats.total_sessions} sessions`,
              zh: `${viewData.stats.total_messages.toLocaleString()} msgs / ${viewData.stats.total_sessions} sessions`,
            })}
            tone="neutral"
          />
        ) : (
          <SurfaceMetricPill
            label={t({ ko: "Provider Slice", en: "Provider Slice", ja: "Provider Slice", zh: "Provider Slice" })}
            value={selectedProviderLabel ?? selectedProvider}
            tone="accent"
          />
        )}
      </div>

      {selectedProviderLabel && (
        <SurfaceNotice className="mt-4" tone="info" compact>
          {t({
            ko: `${selectedProviderLabel} provider만 필터링한 비용/토큰 slice입니다.`,
            en: `This view is filtered to the ${selectedProviderLabel} provider slice.`,
            ja: `${selectedProviderLabel} provider に絞った slice です。`,
            zh: `当前视图是 ${selectedProviderLabel} provider 的 slice。`,
          })}
        </SurfaceNotice>
      )}

      {expanded && (
        <SurfaceCard
          className="mt-4 rounded-3xl p-2 sm:p-3"
          style={{
            background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
            borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
          }}
        >
          <div
            className="mx-auto max-w-[440px] rounded-2xl p-3 sm:p-4 font-mono text-xs sm:text-xs"
            style={{
              background: "#fefdf8",
              color: "#1a1a1a",
            }}
          >
            <div className="text-center font-bold text-[13px] tracking-widest mb-0.5">AI TOKEN RECEIPT</div>
            <div className="text-center text-[9px]" style={{ color: "#666" }}>
              {viewData.period_start} ~ {viewData.period_end}
            </div>
            {selectedProvider !== "all" && (
              <div className="text-center text-[9px] mt-0.5" style={{ color: "#a16207" }}>
                {viewData.providers[0]?.provider ?? selectedProvider}
              </div>
            )}
            <hr style={{ border: "none", borderTop: "2px double #bbb", margin: "8px 0", opacity: 0.6 }} />

            <div className="flex justify-between text-[9px] font-bold mb-1" style={{ color: "#888", letterSpacing: 1 }}>
              <span className="flex-1">MODEL</span>
              <span style={{ width: 60, textAlign: "right" }}>TOKENS</span>
              <span style={{ width: 70, textAlign: "right" }}>COST</span>
            </div>

            {viewData.models.map((model) => (
              <div key={`${model.provider}-${model.model}`} className="flex items-baseline mb-0.5">
                <span className="font-semibold shrink-0">{model.display_name}</span>
                <span className="flex-1 mx-1" style={{ borderBottom: "1px dotted #ccc", height: 10 }} />
                <span className="shrink-0 text-[10px]" style={{ width: 60, textAlign: "right", color: "#555" }}>
                  {formatTokens(model.total_tokens)}
                </span>
                <span className="shrink-0 font-semibold" style={{ width: 70, textAlign: "right" }}>
                  {formatCost(model.cost)}
                </span>
              </div>
            ))}

            <hr style={{ border: "none", borderTop: "1px dashed #bbb", margin: "8px 0", opacity: 0.6 }} />

            <div className="flex justify-between font-bold">
              <span>SUBTOTAL</span>
              <span>{formatCost(viewData.subtotal)}</span>
            </div>
            {viewData.cache_discount > 0.001 && (
              <div className="flex justify-between font-semibold" style={{ color: "#059669" }}>
                <span>CACHE DISCOUNT</span>
                <span>-{formatCost(viewData.cache_discount)}</span>
              </div>
            )}

            <hr style={{ border: "none", borderTop: "2px double #bbb", margin: "8px 0", opacity: 0.6 }} />

            <div className="flex justify-between font-bold text-[14px]">
              <span>TOTAL</span>
              <span>{formatCost(viewData.total)}</span>
            </div>

            <hr style={{ border: "none", borderTop: "2px double #bbb", margin: "8px 0", opacity: 0.6 }} />

            <div className="text-[10px] font-bold mb-1" style={{ color: "#444" }}>STATISTICS</div>
            <div className="flex justify-between text-[10px]" style={{ color: "#555" }}>
              <span>Tokens</span>
              <span>{formatTokens(totalTokens)}</span>
            </div>
            {selectedProvider === "all" && (
              <>
                <div className="flex justify-between text-[10px]" style={{ color: "#555" }}>
                  <span>Messages</span>
                  <span>{viewData.stats.total_messages.toLocaleString()}</span>
                </div>
                <div className="flex justify-between text-[10px]" style={{ color: "#555" }}>
                  <span>Sessions</span>
                  <span>{viewData.stats.total_sessions.toLocaleString()}</span>
                </div>
              </>
            )}

            {selectedProvider === "all" && viewData.agents.length > 0 && (
              <>
                <div className="text-[9px] font-bold mt-2 mb-0.5" style={{ color: "#666" }}>AGENT USAGE</div>
                {viewData.agents.filter((agent) => agent.percentage >= 0.1).map((agent) => (
                  <div key={agent.agent} className="flex justify-between text-[10px]" style={{ color: "#555" }}>
                    <span>{agent.agent}</span>
                    <span>{agent.percentage.toFixed(0)}%</span>
                  </div>
                ))}
              </>
            )}

            <div className="text-[9px] font-bold mt-2 mb-0.5" style={{ color: "#666" }}>PROVIDER USAGE</div>
            {viewData.providers.map((provider) => (
              <div key={provider.provider} className="flex justify-between text-[10px]" style={{ color: "#555" }}>
                <span>{provider.provider}</span>
                <span>{provider.percentage.toFixed(0)}%</span>
              </div>
            ))}

            <hr style={{ border: "none", borderTop: "1px dashed #bbb", margin: "8px 0", opacity: 0.6 }} />
            <div className="text-center text-xs" style={{ color: "#888" }}>Thank you for using AgentDesk!</div>
            <div className="text-center text-[12px] mt-1" style={{ color: "#1a1a1a", opacity: 0.2, letterSpacing: 1 }}>
              ||||| || ||| || |||| || ||| | |||| ||| ||
            </div>
          </div>
        </SurfaceCard>
      )}
    </SurfaceSection>
  );
}
