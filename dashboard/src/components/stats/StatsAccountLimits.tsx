import { useEffect, useState, type CSSProperties } from "react";
import { RefreshCw } from "lucide-react";
import { ApiRequestError, request } from "../../api/httpClient";
import { Button } from "../../kit/free/button-clean";
import { UsageMeter, type UsageLimit, type UsageMeterLabels } from "../../kit/pro/usage-meter";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import { Cap, CardNote, CardSkeleton, InspSection, SectionCard } from "../common/KitPrimitives";
import { formatClock } from "../dashboard/routineModel";
import type { TFunction } from "../dashboard/model";
import { autoPauseRule, pauseMeterScale } from "../home/homeModel";
import { loadRuntimeConfig, usePolled } from "../home/useHomeData";
import { msg } from "./statsModel";

interface ClaudeWindow { pct?: number | null; resetsAt?: string | null }
interface ClaudeAccount {
  number?: number | null;
  email?: string | null;
  active?: boolean;
  usage?: { fiveHour?: ClaudeWindow | null; sevenDay?: ClaudeWindow | null } | null;
}
interface ClaudeAccountsResponse {
  hostname?: string;
  fetchedAt?: string;
  usageDataStale?: boolean;
  staleReason?: string | null;
  accounts?: ClaudeAccount[];
}
interface Load {
  status: "loading" | "ready" | "notInstalled" | "failed";
  data: ClaudeAccountsResponse | null;
  error: string | null;
  hint: string | null;
}

const TIMEOUT_MS = 15_000;
const STALE_MS = 2 * 60_000;
const REFRESH_STYLE = { "--aui-button-clean-h": "44px" } as CSSProperties;
// Accent bars below the warning step, as on home's usage card.
const METER_STYLE = { "--aui-usage-meter-meter-fill": "var(--accent)" } as CSSProperties;

function span(minutes: number, t: TFunction): string {
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return t(msg(`${days}일 ${hours}시간`, `${days}d ${hours}h`, `${days}日 ${hours}時間`, `${days}天 ${hours}小时`));
  if (hours > 0) return t(msg(`${hours}시간 ${minutes % 60}분`, `${hours}h ${minutes % 60}m`, `${hours}時間 ${minutes % 60}分`, `${hours}小时 ${minutes % 60}分`));
  return t(msg(`${minutes}분`, `${minutes}m`, `${minutes}分`, `${minutes}分`));
}

/** One kit limit row per account window; the line under a row says when that window resets. */
export function accountLimits(data: ClaudeAccountsResponse | null, t: TFunction, now: number): UsageLimit[] {
  return (data?.accounts ?? []).flatMap((account) => {
    // cswap can list one email under two numbers, and the kit keys rows by group and label, so the number stays.
    const name = account.email ? `${account.email}${account.number != null ? ` · #${account.number}` : ""}` : t(msg(`계정 ${account.number ?? "-"}`, `Account ${account.number ?? "-"}`));
    const group = account.active ? `${name} · ${t(msg("사용 중", "In use", "使用中", "使用中"))}` : name;
    const windows: Array<[string, ClaudeWindow | null | undefined]> = [
      [t(msg("5시간", "5 hours", "5時間", "5小时")), account.usage?.fiveHour],
      [t(msg("7일", "7 days", "7日", "7天")), account.usage?.sevenDay],
    ];
    return windows.map(([label, window]): UsageLimit => {
      const left = window?.resetsAt ? new Date(window.resetsAt).getTime() - now : Number.NaN;
      const detail = !Number.isFinite(left) ? undefined
        : left < 60_000 ? t(msg("곧 초기화", "Resets soon", "まもなくリセット", "即将重置"))
          : t(msg(`${span(Math.floor(left / 60_000), t)} 후 초기화`, `Resets in ${span(Math.floor(left / 60_000), t)}`));
      return { group, label, used: typeof window?.pct === "number" ? Math.round(window.pct) : null, total: 100, unit: "%", detail };
    });
  });
}

/** Kit texts for the account list. The pause line names only its value: the gate reads provider usage, not an account. */
function limitLabels(t: TFunction, pausePct: number | null): Partial<UsageMeterLabels> {
  return {
    limits: t(msg("계정별 한도", "Limits by account", "アカウント別の上限", "按账号划分的上限")),
    limitReached: t(msg("한도 도달", "Limit reached")),
    nearLimit: t(msg("한도에 가까움", "Close to the limit")),
    notMeasured: t(msg("측정 안 됨", "Not measured")),
    unlimited: t(msg("무제한", "Unlimited")),
    announce: (state, title, actual, percent) => state === "limit"
      ? t(msg(`${title}, 한도 도달. ${actual}.`, `${title}, limit reached. ${actual}.`))
      : t(msg(`${title}, ${percent}% 썼어요.`, `${title}, ${percent}% used.`)),
    ...(pausePct === null ? {} : {
      danger: t(msg(`멈춤 기준 ${pausePct}% 이상`, `At or past the ${pausePct}% pause line`)),
      announceDanger: (title: string, _actual: string, percent: number) => t(msg(`${title}, 멈춤 기준 ${pausePct}% 이상이에요. ${percent}%.`, `${title}, at or past the ${pausePct}% pause line. ${percent}%.`)),
    }),
  };
}

/** Claude account limits from cswap as the kit limit list home uses for provider limits, with home's auto-run pause line.
 * Polls every 30 seconds while visible; the button rereads at once. */
export function StatsAccountLimits({ t, locale, className }: { t: TFunction; locale: string; className?: string }) {
  const [load, setLoad] = useState<Load>({ status: "loading", data: null, error: null, hint: null });
  const [busy, setBusy] = useState(true);
  const [nonce, setNonce] = useState(0);
  const pauseScale = pauseMeterScale(autoPauseRule(usePolled(loadRuntimeConfig, 300_000).data));
  const pausePct = pauseScale.dangerAt === undefined ? null : Math.round(pauseScale.dangerAt * 100);

  useEffect(() => {
    let controller: AbortController | null = null;
    const read = async () => {
      controller?.abort();
      const current = new AbortController();
      controller = current;
      setBusy(true);
      try {
        const data = await request<ClaudeAccountsResponse>("/api/claude-accounts", { signal: current.signal, timeoutMs: TIMEOUT_MS, maxRetries: 0, suppressErrorToast: true });
        if (!current.signal.aborted) setLoad({ status: "ready", data, error: null, hint: null });
      } catch (error) {
        if (current.signal.aborted) return;
        // A non-2xx answer carries the cswap state and its install command; the last accounts stay on screen.
        const body = (error instanceof ApiRequestError && typeof error.body === "object" && error.body ? error.body : {}) as { status?: string; error?: string; install?: { command?: string } };
        const reason = body.error ?? (error instanceof ApiRequestError ? `HTTP ${error.status}` : String(error));
        setLoad((previous) => ({ ...previous, status: body.status === "not_installed" ? "notInstalled" : "failed", error: reason, hint: body.install?.command ?? null }));
      } finally {
        if (!current.signal.aborted) setBusy(false);
      }
    };
    void read();
    const stop = pollWhileVisible(() => void read(), 30_000);
    return () => {
      controller?.abort();
      stop();
    };
  }, [nonce]);

  const { status, data, error, hint } = load;
  const now = Date.now();
  const limits = accountLimits(data, t, now);
  const fetchedAt = data?.fetchedAt ? new Date(data.fetchedAt).getTime() : Number.NaN;
  const stale = Boolean(data?.usageDataStale) || (Number.isFinite(fetchedAt) && now - fetchedAt > STALE_MS);
  const refresh = (
    <span className="-my-3 -mr-1.5 inline-flex">
      <Button
        variant="ghost"
        icon={<RefreshCw className={busy ? "animate-spin motion-reduce:animate-none" : undefined} />}
        aria-label={t(msg("Claude 계정 사용량 새로고침", "Refresh Claude accounts", "Claude アカウント使用量を更新", "刷新 Claude 账号用量"))}
        disabled={busy}
        style={REFRESH_STYLE}
        onClick={() => setNonce((value) => value + 1)}
      />
    </span>
  );

  return (
    <SectionCard
      title={t(msg("Claude 계정 한도", "Claude account limits", "Claude アカウントの上限", "Claude 账号上限"))}
      testId="stats-claude-accounts"
      className={className}
      aside={
        <span className="ml-auto flex min-w-0 items-center gap-2">
          {data?.hostname || Number.isFinite(fetchedAt) ? (
            <Cap className="min-w-0 truncate font-display tabular-nums" title={data?.hostname}>
              {[data?.hostname, Number.isFinite(fetchedAt) ? formatClock(fetchedAt) : null].filter(Boolean).join(" · ")}
            </Cap>
          ) : null}
          {refresh}
        </span>
      }
    >
      {status === "notInstalled" ? (
        <CardNote>
          {t(msg("cswap이 설치되지 않았어요", "cswap is not installed", "cswap がインストールされていません", "尚未安装 cswap"))}
          {hint || error ? <Cap className="mt-1 block [overflow-wrap:anywhere]">{hint ? `${hint} · ${error ?? ""}` : error}</Cap> : null}
        </CardNote>
      ) : status === "failed" && limits.length === 0 ? (
        <CardNote>
          {t(msg("Claude 계정 정보를 불러오지 못했어요", "Unable to load Claude accounts", "Claude アカウント情報を読み込めませんでした", "无法加载 Claude 账号信息"))}
          {error ? <Cap className="mt-1 block [overflow-wrap:anywhere]">{error}</Cap> : null}
        </CardNote>
      ) : status === "loading" && limits.length === 0 ? (
        <CardSkeleton label={t(msg("Claude 계정 사용량 불러오는 중", "Loading Claude account usage", "Claude アカウント使用量を読み込み中", "正在加载 Claude 账号用量"))} />
      ) : limits.length === 0 ? (
        <CardNote>{t(msg("등록된 Claude 계정이 없어요", "No Claude accounts registered", "登録された Claude アカウントがありません", "没有已注册的 Claude 账号"))}</CardNote>
      ) : (
        // The kit caps its card at 460px; the list fills the column like home's usage card.
        <InspSection className="gap-3 [&_.aui-usage-meter]:max-w-none!" style={METER_STYLE}>
          {stale || status === "failed" ? (
            <Cap className="[overflow-wrap:anywhere]" style={{ color: "var(--warn)" }}>
              {t(msg("최신이 아닌 사용량이에요", "Showing stale usage data", "古い使用量データを表示中", "正在显示过期用量数据"))}
              {data?.staleReason ?? error ? ` · ${data?.staleReason ?? error}` : ""}
            </Cap>
          ) : null}
          <UsageMeter
            layout="limits"
            title={t(msg("계정별 한도", "Limits by account", "アカウント別の上限", "按账号划分的上限"))}
            locale={locale}
            limits={limits}
            {...pauseScale}
            labels={limitLabels(t, pausePct)}
          />
          {pausePct !== null ? (
            <Cap>{t(msg("자동 진행 멈춤은 공급자 사용량으로 판정해요", "Auto-run pauses are judged by provider usage"))}</Cap>
          ) : null}
        </InspSection>
      )}
    </SectionCard>
  );
}
