import { useEffect, useMemo, useState } from "react";
import type { TFunction } from "./model";

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
    <div className="game-panel relative overflow-hidden px-3 py-3 sm:px-4 sm:py-3.5">
      <div className="flex items-start justify-between gap-3 flex-wrap">
        <button
          type="button"
          onClick={() => setExpanded((prev) => !prev)}
          className="flex items-center gap-1.5 hover:opacity-80 transition-opacity"
        >
          <span className="text-xs sm:text-xs font-bold uppercase tracking-wider" style={{ color: "#f59e0b" }}>
            {t({ ko: "토큰 영수증", en: "TOKEN RECEIPT", ja: "トークンレシート", zh: "代币收据" })}
          </span>
          <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {expanded ? "▲" : "▼"}
          </span>
          {loading && (
            <span
              className="rounded px-1.5 py-0.5 text-[8px] font-medium"
              style={{ color: "#fbbf24", background: "rgba(245,158,11,0.14)", border: "1px solid rgba(245,158,11,0.24)" }}
            >
              {t({ ko: "갱신 중", en: "SYNCING", ja: "更新中", zh: "更新中" })}
            </span>
          )}
        </button>

        <div className="flex gap-0.5 flex-wrap justify-end">
          {periods.map((item) => (
            <button
              key={item.id}
              type="button"
              onClick={() => setPeriod(item.id)}
              className="px-2 py-0.5 text-[9px] sm:text-[10px] font-medium rounded transition-colors"
              style={
                period === item.id
                  ? { background: "rgba(245,158,11,0.2)", color: "#f59e0b", border: "1px solid rgba(245,158,11,0.3)" }
                  : { color: "var(--th-text-muted)", border: "1px solid transparent" }
              }
            >
              {item.label}
            </button>
          ))}
        </div>
      </div>

      {providerOptions.length > 1 && (
        <div className="mt-2 flex gap-1 overflow-x-auto">
          <button
            type="button"
            onClick={() => setSelectedProvider("all")}
            className="shrink-0 rounded-full px-2.5 py-1 text-[10px] font-medium"
            style={
              selectedProvider === "all"
                ? { color: "#f59e0b", border: "1px solid rgba(245,158,11,0.28)", background: "rgba(245,158,11,0.12)" }
                : { color: "var(--th-text-muted)", border: "1px solid rgba(255,255,255,0.06)" }
            }
          >
            {t({ ko: "전체 Provider", en: "All Providers", ja: "全 Provider", zh: "全部 Provider" })}
          </button>
          {providerOptions.map((provider) => {
            const active = providerKey(selectedProvider) === providerKey(provider);
            return (
              <button
                key={provider}
                type="button"
                onClick={() => setSelectedProvider(provider)}
                className="shrink-0 rounded-full px-2.5 py-1 text-[10px] font-medium"
                style={
                  active
                    ? { color: "#f59e0b", border: "1px solid rgba(245,158,11,0.28)", background: "rgba(245,158,11,0.12)" }
                    : { color: "var(--th-text-muted)", border: "1px solid rgba(255,255,255,0.06)" }
                }
              >
                {provider}
              </button>
            );
          })}
        </div>
      )}

      <div className="mt-3 flex items-center gap-3 sm:gap-4 flex-wrap">
        <div className="flex items-baseline gap-1">
          <span className="text-lg sm:text-xl font-bold font-mono" style={{ color: "#f59e0b" }}>
            {formatCost(viewData.total)}
          </span>
          {viewData.cache_discount > 0.001 && (
            <span className="text-[9px] sm:text-[10px] font-mono" style={{ color: "#059669" }}>
              (-{formatCost(viewData.cache_discount)})
            </span>
          )}
        </div>
        <div className="text-[9px] sm:text-[10px]" style={{ color: "var(--th-text-muted)" }}>
          {formatTokens(totalTokens)} {t({ ko: "토큰", en: "tokens", ja: "トークン", zh: "代币" })}
          {" / "}
          {viewData.models.length} {t({ ko: "모델 라인", en: "model lines", ja: "モデル行", zh: "模型行" })}
        </div>
        {selectedProvider === "all" ? (
          <div className="text-[9px] sm:text-[10px]" style={{ color: "var(--th-text-muted)" }}>
            {viewData.stats.total_messages.toLocaleString()} {t({ ko: "메시지", en: "msgs", ja: "メッセージ", zh: "消息" })}
            {" / "}
            {viewData.stats.total_sessions} {t({ ko: "세션", en: "sessions", ja: "セッション", zh: "会话" })}
          </div>
        ) : (
          <div className="text-[9px] sm:text-[10px]" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: `${viewData.providers[0]?.provider ?? selectedProvider} slice`,
              en: `${viewData.providers[0]?.provider ?? selectedProvider} slice`,
              ja: `${viewData.providers[0]?.provider ?? selectedProvider} slice`,
              zh: `${viewData.providers[0]?.provider ?? selectedProvider} slice`,
            })}
          </div>
        )}
      </div>

      {expanded && (
        <div
          className="mt-3 rounded-lg p-3 sm:p-4 font-mono text-xs sm:text-xs"
          style={{
            background: "#fefdf8",
            color: "#1a1a1a",
            maxWidth: 440,
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
      )}
    </div>
  );
}
