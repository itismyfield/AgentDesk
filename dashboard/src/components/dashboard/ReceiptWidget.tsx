import { useEffect, useMemo, useState } from "react";
import { onCredentialChange } from "../../api/authState";
import { request, SLOW_AGGREGATE_GET } from "../../api/httpClient";
import { Button } from "../../kit/free/button-clean";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import { Cap, CardNote, Chip, InspSection, KeyValue, Num, Row, SectionCard } from "../common/KitPrimitives";
import type { TFunction } from "./model";

interface ModelLineItem { model: string; display_name: string; total_tokens: number; cost: number; provider: string }
interface ProviderShare { provider: string; tokens: number; percentage: number }
interface AgentShare { agent: string; tokens: number; cost: number; percentage: number }
interface ReceiptData {
  period_label: string; period_start: string; period_end: string; models: ModelLineItem[];
  subtotal: number; cache_discount: number; total: number;
  stats: { total_messages: number; total_sessions: number }; providers: ProviderShare[]; agents: AgentShare[];
}
type Period = "today" | "week" | "month" | "all";

const receiptCache = new Map<Period, { data: ReceiptData; fetchedAt: number }>();
const receiptFailedAt = new Map<Period, number>();
const RECEIPT_RETRY_AFTER_FAILURE_MS = 3 * 60_000;
onCredentialChange(() => { receiptCache.clear(); receiptFailedAt.clear(); });
const RECEIPT_CACHE_TTL: Record<Period, number> = { today: 30 * 60_000, week: 210 * 60_000, month: 15 * 60 * 60_000, all: 24 * 60 * 60_000 };

const formatTokens = (v: number) => (v >= 1e9 ? `${(v / 1e9).toFixed(1)}B` : v >= 1e6 ? `${(v / 1e6).toFixed(1)}M` : v >= 1e3 ? `${(v / 1e3).toFixed(1)}K` : String(v));
const formatCost = (v: number) => (v >= 100 ? `$${Math.round(v).toLocaleString("en-US")}` : v >= 1 ? `$${v.toFixed(2)}` : v >= 0.01 ? `$${v.toFixed(3)}` : `$${v.toFixed(4)}`);
const providerKey = (v: string) => v.trim().toLowerCase();

function buildProviderOptions(data: ReceiptData | null): string[] {
  if (!data) return [];
  const seen = new Set<string>(), out: string[] = [];
  for (const p of data.providers.map((x) => x.provider).concat(data.models.map((x) => x.provider))) {
    const k = providerKey(p);
    if (k && !seen.has(k)) { seen.add(k); out.push(p); }
  }
  return out;
}

function deriveReceiptView(data: ReceiptData | null, selectedProvider: string): ReceiptData | null {
  if (!data || selectedProvider === "all") return data;
  const models = data.models.filter((m) => providerKey(m.provider) === providerKey(selectedProvider));
  if (!models.length) return null;
  const subtotal = models.reduce((s, m) => s + m.cost, 0);
  const cache_discount = data.subtotal > 0 ? data.cache_discount * (subtotal / data.subtotal) : 0;
  const tokens = models.reduce((s, m) => s + m.total_tokens, 0);
  return { ...data, models, subtotal, cache_discount, total: Math.max(subtotal - cache_discount, 0), providers: [{ provider: models[0]?.provider ?? selectedProvider, tokens, percentage: 100 }], agents: [] };
}

export default function ReceiptWidget({ t }: { t: TFunction }) {
  const [data, setData] = useState<ReceiptData | null>(null);
  const [period, setPeriod] = useState<Period>("month");
  const [selectedProvider, setSelectedProvider] = useState("all");
  const [expanded, setExpanded] = useState(false);
  const [loading, setLoading] = useState(false);
  const [loadingSeconds, setLoadingSeconds] = useState(0);

  useEffect(() => {
    setLoadingSeconds(0);
    if (!loading) return;
    const startedAt = Date.now();
    const timer = setInterval(() => setLoadingSeconds(Math.floor((Date.now() - startedAt) / 1000)), 1000);
    return () => clearInterval(timer);
  }, [loading, period]);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      const cached = receiptCache.get(period);
      if (cached && mounted) setData(cached.data);
      const fresh = cached ? Date.now() - cached.fetchedAt < RECEIPT_CACHE_TTL[period] : false;
      const failedAt = receiptFailedAt.get(period);
      if (fresh || (failedAt !== undefined && Date.now() - failedAt < RECEIPT_RETRY_AFTER_FAILURE_MS)) { if (mounted) setLoading(false); return; }
      if (mounted) setLoading(true);
      try {
        const json = await request<ReceiptData>(`/api/receipt?period=${period}`, { ...SLOW_AGGREGATE_GET, suppressErrorToast: true });
        receiptCache.set(period, { data: json, fetchedAt: Date.now() });
        receiptFailedAt.delete(period);
        if (mounted) setData(json);
      } catch (error) {
        if (!(error instanceof Error && error.name === "AbortError")) receiptFailedAt.set(period, Date.now());
      } finally { if (mounted) setLoading(false); }
    };
    void load();
    const stop = pollWhileVisible(() => void load(), 60_000);
    return () => { mounted = false; stop(); };
  }, [period]);

  const providerOptions = useMemo(() => buildProviderOptions(data), [data]);
  const viewData = useMemo(() => deriveReceiptView(data, selectedProvider), [data, selectedProvider]);
  const totalTokens = useMemo(() => viewData?.models.reduce((s, m) => s + m.total_tokens, 0) ?? 0, [viewData]);
  useEffect(() => {
    if (selectedProvider !== "all" && !providerOptions.some((p) => providerKey(p) === providerKey(selectedProvider))) setSelectedProvider("all");
  }, [providerOptions, selectedProvider]);

  const periods: { id: Period; label: string }[] = [
    { id: "today", label: t({ ko: "오늘", en: "Today", ja: "今日", zh: "今天" }) },
    { id: "week", label: t({ ko: "이번 주", en: "This Week", ja: "今週", zh: "本周" }) },
    { id: "month", label: t({ ko: "이번 달", en: "This Month", ja: "今月", zh: "本月" }) },
    { id: "all", label: t({ ko: "전체", en: "All", ja: "全期間", zh: "全部" }) },
  ];
  const title = t({ ko: "토큰 영수증", en: "Token Receipt", ja: "トークンレシート", zh: "代币收据" });

  if (!viewData) {
    return (
      <SectionCard title={title}>
        <CardNote>
          {loading
            ? `${t({ ko: "서버가 영수증을 집계하는 중이에요. 1분 넘게 걸릴 수 있어요", en: "The server is aggregating the receipt; this can take over a minute", ja: "サーバーがレシートを集計中です。1分以上かかる場合があります", zh: "服务器正在汇总收据，可能需要一分钟以上" })} · ${loadingSeconds}s`
            : t({ ko: "영수증 데이터를 불러올 수 없습니다.", en: "Could not load receipt data.", ja: "レシートデータを読み込めません。", zh: "无法加载收据数据。" })}
        </CardNote>
      </SectionCard>
    );
  }

  return (
    <SectionCard title={title} aside={
      <div className="ml-auto flex flex-wrap items-center gap-2">
        <Chip tone="neutral">{viewData.period_label}</Chip>
        {loading ? <Chip tone="warn">{t({ ko: "갱신 중", en: "Syncing", ja: "更新中", zh: "更新中" })} · {loadingSeconds}s</Chip> : null}
        <Button variant="secondary" size="sm" onClick={() => setExpanded((prev) => !prev)}>
          {expanded ? t({ ko: "접기", en: "Collapse", ja: "閉じる", zh: "收起" }) : t({ ko: "영수증 펼치기", en: "Expand Receipt", ja: "レシートを開く", zh: "展开收据" })}
        </Button>
      </div>
    }>
      <InspSection className="gap-3">
        <div className="flex flex-wrap gap-1.5">
          {periods.map((item) => <Button key={item.id} size="sm" variant={period === item.id ? "primary" : "secondary"} onClick={() => setPeriod(item.id)}>{item.label}</Button>)}
        </div>
        {providerOptions.length > 1 ? (
          <div className="flex flex-wrap gap-1.5">
            <Button size="sm" variant={selectedProvider === "all" ? "primary" : "ghost"} onClick={() => setSelectedProvider("all")}>{t({ ko: "전체 프로바이더", en: "All providers", ja: "全プロバイダー", zh: "全部提供方" })}</Button>
            {providerOptions.map((p) => <Button key={p} size="sm" variant={providerKey(selectedProvider) === providerKey(p) ? "primary" : "ghost"} onClick={() => setSelectedProvider(p)}>{p}</Button>)}
          </div>
        ) : null}
        <dl className="m-0 grid grid-cols-1 gap-2 sm:grid-cols-3">
          <KeyValue dl label={t({ ko: "총 비용", en: "Total Cost", ja: "総コスト", zh: "总成本" })} num>
            <strong>{formatCost(viewData.total)}</strong>
            {viewData.cache_discount > 0.001 ? <Cap className="ml-1.5">-{formatCost(viewData.cache_discount)}</Cap> : null}
          </KeyValue>
          <KeyValue dl label={t({ ko: "토큰", en: "Tokens", ja: "トークン", zh: "代币" })} num>{formatTokens(totalTokens)} · {viewData.models.length}</KeyValue>
          <KeyValue dl label={t({ ko: "활동", en: "Activity", ja: "稼働", zh: "活动" })} num>
            {selectedProvider === "all" ? `${viewData.stats.total_messages.toLocaleString()} / ${viewData.stats.total_sessions}` : (viewData.providers[0]?.provider ?? selectedProvider)}
          </KeyValue>
        </dl>
      </InspSection>
      {expanded ? (
        <div>
          <Row density="sm">
            <Cap className="flex-1">{viewData.period_start} ~ {viewData.period_end}</Cap>
            <Cap>{t({ ko: "소계", en: "Subtotal", ja: "小計", zh: "小计" })} {formatCost(viewData.subtotal)}</Cap>
          </Row>
          {viewData.models.map((m) => (
            <Row key={`${m.provider}-${m.model}`} density="sm">
              <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-[var(--fg)]">{m.display_name}</span>
              <Chip tone="neutral">{m.provider}</Chip>
              <Num className="w-16 text-right text-xs text-[var(--fg-muted)]">{formatTokens(m.total_tokens)}</Num>
              <Num className="w-20 text-right text-[13px] font-semibold text-[var(--fg)]">{formatCost(m.cost)}</Num>
            </Row>
          ))}
        </div>
      ) : null}
    </SectionCard>
  );
}
