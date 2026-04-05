import { useEffect, useState } from "react";
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

function formatTokens(t: number): string {
  if (t >= 1e9) return `${(t / 1e9).toFixed(1)}B`;
  if (t >= 1e6) return `${(t / 1e6).toFixed(1)}M`;
  if (t >= 1e3) return `${(t / 1e3).toFixed(1)}K`;
  return String(t);
}

function formatCost(c: number): string {
  if (c >= 100) return `$${c.toFixed(0)}`;
  if (c >= 1) return `$${c.toFixed(2)}`;
  if (c >= 0.01) return `$${c.toFixed(3)}`;
  return `$${c.toFixed(4)}`;
}

interface ReceiptWidgetProps {
  t: TFunction;
}

export default function ReceiptWidget({ t }: ReceiptWidgetProps) {
  const [data, setData] = useState<ReceiptData | null>(null);
  const [period, setPeriod] = useState<Period>("month");
  const [expanded, setExpanded] = useState(false);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const res = await fetch(`/api/receipt?period=${period}`, { credentials: "include" });
        if (!res.ok) return;
        const json = await res.json();
        if (mounted) setData(json);
      } catch { /* ignore */ }
    };
    load();
    const timer = setInterval(load, 120_000);
    return () => { mounted = false; clearInterval(timer); };
  }, [period]);

  if (!data || data.models.length === 0) return null;

  const periods: { id: Period; label: string }[] = [
    { id: "today", label: t({ ko: "오늘", en: "Today", ja: "今日", zh: "今天" }) },
    { id: "week", label: t({ ko: "이번 주", en: "This Week", ja: "今週", zh: "本周" }) },
    { id: "month", label: t({ ko: "이번 달", en: "This Month", ja: "今月", zh: "本月" }) },
    { id: "all", label: t({ ko: "전체", en: "All", ja: "全期間", zh: "全部" }) },
  ];

  return (
    <div className="game-panel relative overflow-hidden px-3 py-2 sm:px-4 sm:py-3">
      {/* Header row */}
      <div className="flex items-center justify-between mb-2">
        <button
          onClick={() => setExpanded(!expanded)}
          className="flex items-center gap-1.5 hover:opacity-80 transition-opacity"
        >
          <span className="text-xs sm:text-xs font-bold uppercase tracking-wider" style={{ color: "#f59e0b" }}>
            {t({ ko: "토큰 영수증", en: "TOKEN RECEIPT", ja: "トークンレシート", zh: "代币收据" })}
          </span>
          <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {expanded ? "▲" : "▼"}
          </span>
        </button>
        {/* Period selector */}
        <div className="flex gap-0.5">
          {periods.map((p) => (
            <button
              key={p.id}
              onClick={() => setPeriod(p.id)}
              className="px-2 py-0.5 text-xs sm:text-xs font-medium rounded transition-colors"
              style={
                period === p.id
                  ? { background: "rgba(245,158,11,0.2)", color: "#f59e0b", border: "1px solid rgba(245,158,11,0.3)" }
                  : { color: "var(--th-text-muted)", border: "1px solid transparent" }
              }
            >
              {p.label}
            </button>
          ))}
        </div>
      </div>

      {/* Compact summary (always shown) */}
      <div className="flex items-center gap-3 sm:gap-4">
        <div className="flex items-baseline gap-1">
          <span className="text-lg sm:text-xl font-bold font-mono" style={{ color: "#f59e0b" }}>
            {formatCost(data.total)}
          </span>
          {data.cache_discount > 0.001 && (
            <span className="text-xs sm:text-xs font-mono" style={{ color: "#059669" }}>
              (-{formatCost(data.cache_discount)})
            </span>
          )}
        </div>
        <div className="text-xs sm:text-xs" style={{ color: "var(--th-text-muted)" }}>
          {data.stats.total_messages.toLocaleString()} {t({ ko: "메시지", en: "msgs", ja: "メッセージ", zh: "消息" })}
          {" / "}
          {data.stats.total_sessions} {t({ ko: "세션", en: "sessions", ja: "セッション", zh: "会话" })}
        </div>
        {data.providers.length > 1 && (
          <div className="hidden sm:flex gap-1.5">
            {data.providers.map((p) => (
              <span key={p.provider} className="text-xs font-mono" style={{ color: "var(--th-text-muted)" }}>
                {p.provider} {p.percentage.toFixed(0)}%
              </span>
            ))}
          </div>
        )}
      </div>

      {/* Expanded receipt */}
      {expanded && (
        <div
          className="mt-3 rounded-lg p-3 sm:p-4 font-mono text-xs sm:text-xs"
          style={{
            background: "#fefdf8",
            color: "#1a1a1a",
            maxWidth: 420,
          }}
        >
          <div className="text-center font-bold text-[13px] tracking-widest mb-0.5">AI TOKEN RECEIPT</div>
          <div className="text-center text-xs" style={{ color: "#666" }}>
            {data.period_start} ~ {data.period_end}
          </div>
          <hr style={{ border: "none", borderTop: "2px double #bbb", margin: "8px 0", opacity: 0.6 }} />

          {/* Column headers */}
          <div className="flex justify-between text-xs font-bold mb-1" style={{ color: "#888", letterSpacing: 1 }}>
            <span className="flex-1">MODEL</span>
            <span style={{ width: 60, textAlign: "right" }}>TOKENS</span>
            <span style={{ width: 70, textAlign: "right" }}>COST</span>
          </div>

          {/* Line items */}
          {data.models.map((m) => (
            <div key={m.model} className="flex items-baseline mb-0.5">
              <span className="font-semibold shrink-0">{m.display_name}</span>
              <span className="flex-1 mx-1" style={{ borderBottom: "1px dotted #ccc", height: 10 }} />
              <span className="shrink-0 text-xs" style={{ width: 60, textAlign: "right", color: "#555" }}>
                {formatTokens(m.total_tokens)}
              </span>
              <span className="shrink-0 font-semibold" style={{ width: 70, textAlign: "right" }}>
                {formatCost(m.cost)}
              </span>
            </div>
          ))}

          <hr style={{ border: "none", borderTop: "1px dashed #bbb", margin: "8px 0", opacity: 0.6 }} />

          {/* Subtotal */}
          <div className="flex justify-between font-bold">
            <span>SUBTOTAL</span>
            <span>{formatCost(data.subtotal)}</span>
          </div>
          {data.cache_discount > 0.001 && (
            <div className="flex justify-between font-semibold" style={{ color: "#059669" }}>
              <span>CACHE DISCOUNT</span>
              <span>-{formatCost(data.cache_discount)}</span>
            </div>
          )}

          <hr style={{ border: "none", borderTop: "2px double #bbb", margin: "8px 0", opacity: 0.6 }} />

          {/* Total */}
          <div className="flex justify-between font-bold text-[14px]">
            <span>TOTAL</span>
            <span>{formatCost(data.total)}</span>
          </div>

          <hr style={{ border: "none", borderTop: "2px double #bbb", margin: "8px 0", opacity: 0.6 }} />

          {/* Stats */}
          <div className="text-xs font-bold mb-1" style={{ color: "#444" }}>STATISTICS</div>
          <div className="flex justify-between text-xs" style={{ color: "#555" }}>
            <span>Messages</span>
            <span>{data.stats.total_messages.toLocaleString()}</span>
          </div>
          <div className="flex justify-between text-xs" style={{ color: "#555" }}>
            <span>Sessions</span>
            <span>{data.stats.total_sessions.toLocaleString()}</span>
          </div>

          {data.agents && data.agents.length > 0 && (
            <>
              <div className="text-xs font-bold mt-2 mb-0.5" style={{ color: "#666" }}>AGENT USAGE</div>
              {data.agents.filter((a) => a.percentage >= 0.1).map((a) => (
                <div key={a.agent} className="flex justify-between text-xs" style={{ color: "#555" }}>
                  <span>{a.agent}</span>
                  <span>{a.percentage.toFixed(0)}%</span>
                </div>
              ))}
            </>
          )}

          {data.providers.length > 1 && (
            <>
              <div className="text-xs font-bold mt-2 mb-0.5" style={{ color: "#666" }}>PROVIDER USAGE</div>
              {data.providers.map((p) => (
                <div key={p.provider} className="flex justify-between text-xs" style={{ color: "#555" }}>
                  <span>{p.provider}</span>
                  <span>{p.percentage.toFixed(0)}%</span>
                </div>
              ))}
            </>
          )}

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
