import { useEffect, useState, type CSSProperties } from "react";
import { ArrowRightLeft, Eye, EyeOff, RefreshCw } from "lucide-react";
import { ApiRequestError, request } from "../../api/httpClient";
import { Button } from "../../kit/free/button-clean";
import { UsageMeter, type UsageLimit, type UsageMeterLabels } from "../../kit/pro/usage-meter";
import { pollWhileVisible } from "../../lib/pollWhileVisible";
import { Cap, CardNote, CardSkeleton, Chip, InspSection, SectionCard, Well } from "../common/KitPrimitives";
import { formatClock } from "../dashboard/routineModel";
import type { TFunction } from "../dashboard/model";
import { autoPauseRule, pauseMeterScale } from "../home/homeModel";
import { loadRuntimeConfig, usePolled } from "../home/useHomeData";
import { msg } from "./statsModel";

export interface ClaudeWindow { pct?: number | null; resetsAt?: string | null }
export interface ClaudeAccount {
  number?: number | null; email?: string | null; active?: boolean; usageStatus?: string | null;
  usage?: ({ fiveHour?: ClaudeWindow | null; sevenDay?: ClaudeWindow | null } & Record<string, unknown>) | null;
}
export interface ClaudeAccountsResponse {
  hostname?: string; fetchedAt?: string; usageDataStale?: boolean; staleReason?: string | null;
  activeAccountNumber?: number | null; accounts?: ClaudeAccount[];
}
interface Load { status: "loading" | "ready" | "notInstalled" | "failed"; data: ClaudeAccountsResponse | null; error: string | null; hint: string | null }

const TIMEOUT_MS = 15_000, SWITCH_TIMEOUT_MS = 30_000, STALE_MS = 2 * 60_000;
const ICON_BTN_STYLE = { "--aui-button-clean-h": "44px" } as CSSProperties;
const METER_STYLE = { "--aui-usage-meter-meter-fill": "var(--accent)" } as CSSProperties;

/** Masks `operator.long@example.com` as `oper***@example.com` while preserving domain context. */
export function maskAccountEmail(email: string): string {
  const s = email.trim(), at = s.indexOf("@");
  if (at <= 0) return s ? `${s.slice(0, Math.min(3, s.length))}***` : "";
  const local = s.slice(0, at);
  return `${local.slice(0, Math.min(4, Math.max(1, local.length - 1)))}***@${s.slice(at + 1)}`;
}

export function claudeWindowLabel(rawKey: string, t: TFunction): string {
  const n = rawKey.trim().toLowerCase().replace(/[\s_-]+/g, "");
  if (n === "fivehour" || n === "5h" || n === "5시간") return t(msg("5시간", "5 hours", "5時間", "5小时"));
  if (n === "sevenday" || n === "7d" || n === "7일") return t(msg("7일", "7 days", "7日", "7天"));
  if (n.includes("fable")) return t(msg("7일 상위 모델 (Fable)", "7d frontier model (Fable)", "7日 上位モデル (Fable)", "7天 高阶模型 (Fable)"));
  if (n.includes("opus")) return t(msg("7일 Opus 모델", "7d Opus model", "7日 Opus モデル", "7天 Opus 模型"));
  if (n.includes("sonnet")) return t(msg("7일 Sonnet 모델", "7d Sonnet model", "7日 Sonnet モデル", "7天 Sonnet 模型"));
  return rawKey;
}

export function unmeasuredWindowReason(account: ClaudeAccount, t: TFunction): string {
  const st = (account.usageStatus ?? "").toLowerCase();
  if (st.includes("expir") || st.includes("auth") || st.includes("unauth")) return t(msg("인증 만료 · 재로그인 필요", "Sign-in expired · re-authenticate"));
  if (st.includes("err") || st.includes("fail") || st.includes("unavail") || st.includes("rate")) return t(msg("사용량 조회 실패 · 새로고침으로 재확인", "Usage check failed · refresh to retry"));
  if (!account.active && !account.usage) return t(msg("비활성 슬롯 · 전환하거나 새로고침하면 측정돼요", "Inactive slot · measured on switch or refresh"));
  return t(msg("최근 호출 없음 · 첫 요청 후 집계돼요", "No recent window activity · measured after first call"));
}

function span(minutes: number, t: TFunction): string {
  const days = Math.floor(minutes / 1440), hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return t(msg(`${days}일 ${hours}시간`, `${days}d ${hours}h`, `${days}日 ${hours}時間`, `${days}天 ${hours}小时`));
  if (hours > 0) return t(msg(`${hours}시간 ${minutes % 60}분`, `${hours}h ${minutes % 60}m`, `${hours}時間 ${minutes % 60}分`, `${hours}小时 ${minutes % 60}分`));
  return t(msg(`${minutes}분`, `${minutes}m`, `${minutes}分`, `${minutes}分`));
}

const isWin = (v: unknown): v is ClaudeWindow => Boolean(v && typeof v === "object" && ("pct" in v || "resetsAt" in v));

export function accountLimits(data: ClaudeAccountsResponse | null, t: TFunction, now: number, options?: { maskEmail?: boolean }): UsageLimit[] {
  const mask = Boolean(options?.maskEmail);
  return (data?.accounts ?? []).flatMap((account) => {
    const shownEmail = account.email ? (mask ? maskAccountEmail(account.email) : account.email) : null;
    const name = shownEmail ? `${shownEmail}${account.number != null ? ` · #${account.number}` : ""}` : t(msg(`계정 ${account.number ?? "-"}`, `Account ${account.number ?? "-"}`));
    const group = account.active ? `${name} · ${t(msg("사용 중", "In use", "使用中", "使用中"))}` : name;
    const extra = Object.entries(account.usage ?? {}).filter(([k, v]) => k !== "fiveHour" && k !== "sevenDay" && isWin(v)).map(([k, v]) => [claudeWindowLabel(k, t), v as ClaudeWindow] as const);
    const windows: ReadonlyArray<readonly [string, ClaudeWindow | null | undefined]> = [[claudeWindowLabel("fiveHour", t), account.usage?.fiveHour], [claudeWindowLabel("sevenDay", t), account.usage?.sevenDay], ...extra];
    return windows.map(([label, window]): UsageLimit => {
      const hasPct = typeof window?.pct === "number" && Number.isFinite(window.pct);
      const left = window?.resetsAt ? new Date(window.resetsAt).getTime() - now : Number.NaN;
      const detail = Number.isFinite(left)
        ? (left < 60_000 ? t(msg("곧 초기화", "Resets soon", "まもなくリセット", "即将重置")) : t(msg(`${span(Math.floor(left / 60_000), t)} 후 초기화`, `Resets in ${span(Math.floor(left / 60_000), t)}`)))
        : (hasPct ? undefined : unmeasuredWindowReason(account, t));
      return { group, label, used: hasPct ? Math.round(window!.pct!) : null, total: 100, unit: "%", detail };
    });
  });
}

function limitLabels(t: TFunction, pausePct: number | null): Partial<UsageMeterLabels> {
  return {
    limits: t(msg("계정별 한도", "Limits by account", "アカウント別の上限", "按账号划分的上限")),
    limitReached: t(msg("한도 도달", "Limit reached")), nearLimit: t(msg("한도에 가까움", "Close to the limit")),
    notMeasured: t(msg("측정 안 됨", "Not measured")), unlimited: t(msg("무제한", "Unlimited")),
    announce: (state, title, actual, percent) => state === "limit" ? t(msg(`${title}, 한도 도달. ${actual}.`, `${title}, limit reached. ${actual}.`)) : t(msg(`${title}, ${percent}% 썼어요.`, `${title}, ${percent}% used.`)),
    ...(pausePct === null ? {} : {
      danger: t(msg(`멈춤 기준 ${pausePct}% 이상`, `At or past the ${pausePct}% pause line`)),
      announceDanger: (title: string, _actual: string, percent: number) => t(msg(`${title}, 멈춤 기준 ${pausePct}% 이상이에요. ${percent}%.`, `${title}, at or past the ${pausePct}% pause line. ${percent}%.`)),
    }),
  };
}

export function StatsAccountLimits({ t, locale, allowSwitch = false, defaultMasked = false, className }: { t: TFunction; locale: string; allowSwitch?: boolean; defaultMasked?: boolean; className?: string }) {
  const [load, setLoad] = useState<Load>({ status: "loading", data: null, error: null, hint: null });
  const [busy, setBusy] = useState(true);
  const [nonce, setNonce] = useState(0);
  const [masked, setMasked] = useState(defaultMasked);
  const [confirmAccount, setConfirmAccount] = useState<ClaudeAccount | null>(null);
  const [switching, setSwitching] = useState(false);
  const [switchFeedback, setSwitchFeedback] = useState<{ tone: "ok" | "err"; message: string } | null>(null);
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
        const body = (error instanceof ApiRequestError && typeof error.body === "object" && error.body ? error.body : {}) as { status?: string; error?: string; install?: { command?: string } };
        const reason = body.error ?? (error instanceof ApiRequestError ? `HTTP ${error.status}` : String(error));
        setLoad((prev) => ({ ...prev, status: body.status === "not_installed" ? "notInstalled" : "failed", error: reason, hint: body.install?.command ?? null }));
      } finally { if (!current.signal.aborted) setBusy(false); }
    };
    void read();
    const stop = pollWhileVisible(() => void read(), 30_000);
    return () => { controller?.abort(); stop(); };
  }, [nonce]);

  const { status, data, error, hint } = load;
  const now = Date.now(), accounts = data?.accounts ?? [];
  const limits = accountLimits(data, t, now, { maskEmail: masked });
  const spareScore = (a: ClaudeAccount) => Math.max(a.usage?.fiveHour?.pct ?? Infinity, a.usage?.sevenDay?.pct ?? Infinity);
  const spareAccounts = allowSwitch
    ? accounts.filter((a): a is ClaudeAccount & { number: number } => !a.active && typeof a.number === "number")
        .sort((a, b) => spareScore(a) - spareScore(b))
    : [];
  const fetchedAt = data?.fetchedAt ? new Date(data.fetchedAt).getTime() : Number.NaN;
  const stale = Boolean(data?.usageDataStale) || (Number.isFinite(fetchedAt) && now - fetchedAt > STALE_MS);

  const labelFor = (a: ClaudeAccount) => a.email ? `#${a.number} (${masked ? maskAccountEmail(a.email) : a.email})` : `#${a.number}`;

  const runSwitch = async (target: ClaudeAccount) => {
    if (target.number == null || switching) return;
    setSwitching(true); setSwitchFeedback(null);
    try {
      await request("/api/claude-accounts/switch", { method: "POST", body: JSON.stringify({ account: String(target.number) }), timeoutMs: SWITCH_TIMEOUT_MS, maxRetries: 0, suppressErrorToast: true });
      setConfirmAccount(null);
      setSwitchFeedback({ tone: "ok", message: t(msg(`계정 #${target.number}로 전환했어요.`, `Switched to account #${target.number}.`)) });
      setNonce((v) => v + 1);
    } catch (err) {
      const body = (err instanceof ApiRequestError && typeof err.body === "object" && err.body ? err.body : {}) as { error?: string };
      setSwitchFeedback({ tone: "err", message: t(msg(`계정 전환 실패: ${body.error ?? (err instanceof Error ? err.message : String(err))}`, `Could not change account: ${body.error ?? String(err)}`)) });
    } finally { setSwitching(false); }
  };

  return (
    <SectionCard title={t(msg("Claude 계정 한도", "Claude account limits", "Claude アカウントの上限", "Claude 账号上限"))} testId="stats-claude-accounts" className={className} aside={
      <span className="ml-auto flex min-w-0 items-center gap-1">
        {data?.hostname || Number.isFinite(fetchedAt) ? <Cap className="min-w-0 truncate font-display tabular-nums" title={data?.hostname}>{[data?.hostname, Number.isFinite(fetchedAt) ? formatClock(fetchedAt) : null].filter(Boolean).join(" · ")}</Cap> : null}
        {defaultMasked && accounts.some((a) => a.email) ? (
          <span className="-my-3 inline-flex">
            <Button variant="ghost" data-testid="stats-claude-mask-toggle" icon={masked ? <Eye /> : <EyeOff />} aria-pressed={!masked} aria-label={masked ? t(msg("계정 이메일 원문 보기", "Reveal account emails")) : t(msg("계정 이메일 마스킹", "Mask account emails"))} style={ICON_BTN_STYLE} onClick={() => setMasked((p) => !p)} />
          </span>
        ) : null}
        <span className="-my-3 -mr-1.5 inline-flex">
          <Button variant="ghost" icon={<RefreshCw className={busy ? "animate-spin motion-reduce:animate-none" : undefined} />} aria-label={t(msg("Claude 계정 사용량 새로고침", "Refresh Claude accounts", "Claude アカウント使用量を更新", "刷新 Claude 账号用量"))} disabled={busy} style={ICON_BTN_STYLE} onClick={() => setNonce((v) => v + 1)} />
        </span>
      </span>
    }>
      {status === "notInstalled" ? (
        <CardNote>{t(msg("cswap이 설치되지 않았어요", "cswap is not installed", "cswap がインストールされていません", "尚未安装 cswap"))}{hint || error ? <Cap className="mt-1 block [overflow-wrap:anywhere]">{hint ? `${hint} · ${error ?? ""}` : error}</Cap> : null}</CardNote>
      ) : status === "failed" && limits.length === 0 ? (
        <CardNote>{t(msg("Claude 계정 정보를 불러오지 못했어요", "Unable to load Claude accounts", "Claude アカウント情報を読み込めませんでした", "无法加载 Claude 账号信息"))}{error ? <Cap className="mt-1 block [overflow-wrap:anywhere]">{error}</Cap> : null}</CardNote>
      ) : status === "loading" && limits.length === 0 ? (
        <CardSkeleton label={t(msg("Claude 계정 사용량 불러오는 중", "Loading Claude account usage", "Claude アカウント使用量を読み込み中", "正在加载 Claude 账号用量"))} />
      ) : limits.length === 0 ? (
        <CardNote>{t(msg("등록된 Claude 계정이 없어요", "No Claude accounts registered", "登録された Claude アカウントがありません", "没有已注册的 Claude 账号"))}</CardNote>
      ) : (
        <InspSection className="gap-3 [&_.aui-usage-meter]:max-w-none!" style={METER_STYLE}>
          {stale || status === "failed" ? <Cap className="[overflow-wrap:anywhere]" style={{ color: "var(--warn)" }}>{t(msg("최신이 아닌 사용량이에요", "Showing stale usage data", "古い使用量データを表示中", "正在显示过期用量数据"))}{data?.staleReason ?? error ? ` · ${data?.staleReason ?? error}` : ""}</Cap> : null}
          <UsageMeter layout="limits" title={t(msg("계정별 한도", "Limits by account", "アカウント別の上限", "按账号划分的上限"))} locale={locale} limits={limits} {...pauseScale} labels={limitLabels(t, pausePct)} upgradeLabel={t(msg("여유 계정 전환", "Use spare account"))} topUpLabel={t(msg("여유 계정 전환", "Use spare account"))} onUpgrade={spareAccounts.length > 0 ? () => setConfirmAccount(spareAccounts[0]) : undefined} />
          {spareAccounts.length > 0 ? (
            <div className="flex flex-col gap-2" data-testid="stats-claude-switch-bar">
              <div className="flex flex-wrap items-center gap-1.5">
                <Cap className="mr-1">{t(msg("빠른 계정 전환", "Quick account change"))}</Cap>
                {spareAccounts.map((a) => (
                  <Button key={a.number} size="sm" variant="secondary" icon={<ArrowRightLeft size={13} />} disabled={switching} data-testid={`stats-claude-switch-${a.number}`} onClick={() => { setSwitchFeedback(null); setConfirmAccount(a); }}>
                    {t(msg(`#${a.number} 전환`, `Use #${a.number}`))}{typeof a.usage?.fiveHour?.pct === "number" ? ` · ${Math.round(a.usage.fiveHour.pct)}%` : ""}
                  </Button>
                ))}
              </div>
              {confirmAccount ? (
                <Well className="flex flex-wrap items-center justify-between gap-2 px-3 py-2.5 text-xs" data-testid="stats-claude-switch-confirm">
                  <span className="min-w-0 flex-1 text-[var(--fg)] [word-break:keep-all]">{t(msg(`활성 Claude 계정을 ${labelFor(confirmAccount)}로 바로 전환할까요?`, `Change active Claude account to ${labelFor(confirmAccount)}?`))}</span>
                  <span className="flex shrink-0 items-center gap-1.5">
                    <Button size="sm" variant="ghost" disabled={switching} data-testid="stats-claude-switch-cancel" onClick={() => setConfirmAccount(null)}>{t(msg("취소", "Cancel"))}</Button>
                    <Button size="sm" variant="primary" loading={switching} disabled={switching} data-testid="stats-claude-switch-submit" onClick={() => void runSwitch(confirmAccount)}>{t(msg("전환 확인", "Confirm"))}</Button>
                  </span>
                </Well>
              ) : null}
            </div>
          ) : null}
          {switchFeedback ? <div role={switchFeedback.tone === "err" ? "alert" : "status"} data-testid="stats-claude-switch-feedback"><Chip tone={switchFeedback.tone === "err" ? "err" : "ok"} className="h-auto py-1 whitespace-normal [word-break:keep-all]">{switchFeedback.message}</Chip></div> : null}
          {pausePct !== null ? <Cap>{t(msg("자동 진행 멈춤은 공급자 사용량으로 판정해요", "Auto-run pauses are judged by provider usage"))}</Cap> : null}
        </InspSection>
      )}
    </SectionCard>
  );
}
