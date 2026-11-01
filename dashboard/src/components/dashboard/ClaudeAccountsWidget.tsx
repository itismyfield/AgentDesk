import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { RefreshCw } from "lucide-react";
import { ApiRequestError, request } from "../../api/httpClient";
import { SurfaceCard, SurfaceSection } from "../common/SurfacePrimitives";
import { StatusBadge } from "../common/StatusBadge";
import { WidgetState } from "../common/WidgetState";
import {
  RATE_LIMIT_GAUGE_TRACK_STYLE,
  rateLimitFillStyle,
  rateLimitFillWidth,
} from "../common/rateLimitGauge";
import type { TFunction } from "./model";
import { pollWhileVisible } from "../../lib/pollWhileVisible";

interface ClaudeUsageWindow {
  pct?: number | null;
  resetsAt?: string | null;
}

interface ClaudeAccountUsage {
  fiveHour?: ClaudeUsageWindow | null;
  sevenDay?: ClaudeUsageWindow | null;
}

interface ClaudeAccount {
  number?: number | null;
  email?: string | null;
  active?: boolean;
  usageStatus?: string | null;
  usage?: ClaudeAccountUsage | null;
  usageFetchedAt?: string | null;
  usageAgeSeconds?: number | null;
}

interface ClaudeAccountsResponse {
  schemaVersion: number;
  status: "ok" | "usage_data_stale";
  hostname: string;
  instanceId?: string | null;
  fetchedAt: string;
  servedAt: string;
  cacheTtlSeconds: number;
  usageDataStale: boolean;
  staleReason?: string | null;
  activeAccountNumber?: number | null;
  accounts: ClaudeAccount[];
}

interface ClaudeAccountsErrorResponse {
  status?: "not_installed" | "execution_failure" | "bad_request";
  code?: string;
  hostname?: string;
  instanceId?: string | null;
  error?: string;
  install?: {
    command?: string;
    binaryHint?: string;
  };
}

type LoadState = "loading" | "ready" | "not_installed" | "execution_failure";

const CLAUDE_ACCOUNTS_FETCH_TIMEOUT_MS = 15_000;
const CLAUDE_ACCOUNTS_STALE_MS = 2 * 60_000;

function createTimedController(timeoutMs: number) {
  const controller = new AbortController();
  const timeoutId = window.setTimeout(() => controller.abort(), timeoutMs);
  return {
    controller,
    cleanup: () => window.clearTimeout(timeoutId),
  };
}

// Status/install/hostname fields of a non-2xx response drive the widget state.
function errorPayload<T>(error: ApiRequestError): Partial<T> {
  return typeof error.body === "object" && error.body !== null ? (error.body as Partial<T>) : {};
}

function accountKey(account: ClaudeAccount): string {
  return account.email ?? (account.number == null ? "unknown" : String(account.number));
}

function formatSpan(totalMinutes: number, t: TFunction): string {
  const days = Math.floor(totalMinutes / 1440);
  const hours = Math.floor((totalMinutes % 1440) / 60);
  const minutes = totalMinutes % 60;
  if (days > 0) return t({ ko: `${days}일 ${hours}시간`, en: `${days}d ${hours}h`, ja: `${days}日 ${hours}時間`, zh: `${days}天 ${hours}小时` });
  if (hours > 0) return t({ ko: `${hours}시간 ${minutes}분`, en: `${hours}h ${minutes}m`, ja: `${hours}時間 ${minutes}分`, zh: `${hours}小时 ${minutes}分` });
  return t({ ko: `${minutes}분`, en: `${minutes}m`, ja: `${minutes}分`, zh: `${minutes}分` });
}

function formatResetLabel(resetsAt: string | null | undefined, t: TFunction): string {
  if (!resetsAt) return "";
  const diff = new Date(resetsAt).getTime() - Date.now();
  if (!Number.isFinite(diff)) return "";
  if (diff <= 0) return t({ ko: "곧 초기화", en: "Resets soon", ja: "まもなくリセット", zh: "即将重置" });
  const span = formatSpan(Math.floor(diff / 60000), t);
  return t({ ko: `${span} 후 초기화`, en: `Resets in ${span}`, ja: `${span}後にリセット`, zh: `${span}后重置` });
}

function formatUsageAge(seconds: number | null | undefined, t: TFunction): string {
  if (seconds == null || seconds < 0) return "";
  if (seconds < 60) return t({ ko: "방금 갱신", en: "Updated just now", ja: "たった今更新", zh: "刚刚更新" });
  const span = formatSpan(Math.floor(seconds / 60), t);
  return t({ ko: `${span} 전 갱신`, en: `Updated ${span} ago`, ja: `${span}前に更新`, zh: `${span}前更新` });
}

function usageLevel(pct: number | null | undefined): "normal" | "warning" | "danger" {
  if (pct == null) return "normal";
  if (pct >= 95) return "danger";
  if (pct >= 80) return "warning";
  return "normal";
}

function usageColors(level: "normal" | "warning" | "danger") {
  switch (level) {
    case "danger":
      return {
        text: "var(--th-accent-danger)",
        bar: "var(--th-accent-danger)",
        glow: "rgba(239, 68, 68, 0.42)",
      };
    case "warning":
      return {
        text: "var(--th-accent-warn)",
        bar: "var(--th-accent-warn)",
        glow: "rgba(245, 158, 11, 0.34)",
      };
    case "normal":
    default:
      return {
        text: "var(--th-accent-primary)",
        bar: "var(--th-accent-primary)",
        glow: "rgba(20, 184, 166, 0.28)",
      };
  }
}

interface ClaudeUsageGaugeProps {
  label: string;
  window?: ClaudeUsageWindow | null;
  t: TFunction;
}

function ClaudeUsageGauge({ label, window, t }: ClaudeUsageGaugeProps) {
  const rawPct = typeof window?.pct === "number" ? Math.round(window.pct) : null;
  const level = usageLevel(rawPct);
  const colors = usageColors(level);
  const resetLabel = formatResetLabel(window?.resetsAt, t);

  return (
    <div>
      <div className="mb-1.5 flex items-center justify-between gap-2">
        <span className="text-xs font-bold" style={{ color: colors.text }}>
          {label}
        </span>
        <span
          className="text-xs font-semibold tabular-nums"
          style={{ color: rawPct == null ? "var(--th-text-muted)" : colors.text }}
        >
          {rawPct == null ? t({ ko: "정보 없음", en: "No data", ja: "情報なし", zh: "无数据" }) : `${rawPct}%`}
        </span>
      </div>
      <div className="relative overflow-hidden rounded-full" style={{ height: 12, ...RATE_LIMIT_GAUGE_TRACK_STYLE }}>
        <div
          className="absolute inset-y-0 left-0 rounded-full transition-all duration-500"
          style={{
            width: rateLimitFillWidth(rawPct),
            ...(rawPct == null
              ? { background: "transparent", boxShadow: "none" }
              : rateLimitFillStyle(colors.bar, colors.glow, level !== "normal" ? 9 : 5)),
          }}
        />
      </div>
      {resetLabel ? (
        <span className="mt-1 inline-flex whitespace-nowrap text-[11px]" style={{ color: "var(--th-text-muted)", lineHeight: 1.2 }}>
          {resetLabel}
        </span>
      ) : null}
    </div>
  );
}

interface ClaudeAccountsWidgetProps {
  t: TFunction;
}

function ClaudeAccountsWidgetImpl({ t }: ClaudeAccountsWidgetProps) {
  const [data, setData] = useState<ClaudeAccountsResponse | null>(null);
  const [loadState, setLoadState] = useState<LoadState>("loading");
  const [error, setError] = useState<string | null>(null);
  const [installHint, setInstallHint] = useState<string | null>(null);
  const [isRefreshing, setIsRefreshing] = useState(true);
  const [refreshNonce, setRefreshNonce] = useState(0);

  const requestRefresh = useCallback(() => {
    setRefreshNonce((current) => current + 1);
  }, []);

  useEffect(() => {
    let activeController: AbortController | null = null;

    const load = async () => {
      activeController?.abort();
      const { controller, cleanup } = createTimedController(CLAUDE_ACCOUNTS_FETCH_TIMEOUT_MS);
      activeController = controller;
      setIsRefreshing(true);

      try {
        const raw = await request<ClaudeAccountsResponse>("/api/claude-accounts", {
          signal: controller.signal,
          timeoutMs: CLAUDE_ACCOUNTS_FETCH_TIMEOUT_MS,
          maxRetries: 0,
          suppressErrorToast: true,
        });
        if (controller.signal.aborted) return;

        setData(raw);
        setLoadState("ready");
        setError(null);
        setInstallHint(null);
      } catch (nextError) {
        if (controller.signal.aborted) return;
        if (nextError instanceof ApiRequestError) {
          const failure = errorPayload<ClaudeAccountsErrorResponse>(nextError);
          setLoadState(failure.status === "not_installed" ? "not_installed" : "execution_failure");
          setError(failure.error ?? `HTTP ${nextError.status}`);
          setInstallHint(failure.install?.command ?? null);
          return;
        }
        setLoadState("execution_failure");
        setError(nextError instanceof Error ? nextError.message : String(nextError));
      } finally {
        cleanup();
        if (!controller.signal.aborted) setIsRefreshing(false);
      }
    };

    void load();
    const stopPolling = pollWhileVisible(() => void load(), 30_000);
    return () => {
      activeController?.abort();
      stopPolling();
    };
  }, [refreshNonce]);

  const fetchedAgeMs = useMemo(() => {
    if (!data?.fetchedAt) return null;
    const timestamp = new Date(data.fetchedAt).getTime();
    if (!Number.isFinite(timestamp)) return null;
    return Date.now() - timestamp;
  }, [data?.fetchedAt]);
  const isUsageDataStale =
    Boolean(data?.usageDataStale) || (fetchedAgeMs != null && fetchedAgeMs > CLAUDE_ACCOUNTS_STALE_MS);
  const accounts = data?.accounts ?? [];
  const nodeLabel = data?.hostname ?? "";

  const refreshButton = (
    <button
      type="button"
      onClick={requestRefresh}
      disabled={isRefreshing}
      aria-label={t({
        ko: "Claude 계정 사용량 새로고침",
        en: "Refresh Claude accounts",
        ja: "Claude アカウント使用量を更新",
        zh: "刷新 Claude 账号用量",
      })}
      title={t({ ko: "지금 새로고침", en: "Refresh now", ja: "今すぐ更新", zh: "立即刷新" })}
      className="inline-flex h-11 w-11 items-center justify-center rounded-md"
      style={{
        border: "1px solid var(--th-border-subtle)",
        background: "var(--th-overlay-subtle)",
        color: "var(--th-text)",
        cursor: isRefreshing ? "wait" : "pointer",
        opacity: isRefreshing ? 0.6 : 1,
        transition: "opacity 120ms ease",
      }}
    >
      <RefreshCw size={12} className={isRefreshing ? "animate-spin" : undefined} aria-hidden />
    </button>
  );

  const sectionActions = (
    <>
      {nodeLabel ? (
        <StatusBadge tone="info" size="xs" title={nodeLabel}>
          {nodeLabel}
        </StatusBadge>
      ) : null}
      {refreshButton}
    </>
  );

  return (
    <SurfaceSection
      title={t({ ko: "Claude 계정 사용량", en: "Claude account usage", ja: "Claude アカウント使用量", zh: "Claude 账号用量" })}
      description={t({
        ko: "계정별로 사용 한도에 얼마나 가까운지 봐요.",
        en: "How close each account is to its usage limit.",
        ja: "アカウントごとの使用上限までの余裕を確認します。",
        zh: "查看各账号距离用量上限还有多少。",
      })}
      actions={sectionActions}
    >
      {loadState === "not_installed" ? (
        <div className="mt-4">
          <WidgetState
            kind="error"
            tone="warning"
            title={t({
              ko: "cswap 이 설치되지 않았습니다",
              en: "cswap is not installed",
              ja: "cswap がインストールされていません",
              zh: "尚未安装 cswap",
            })}
            description={installHint ? `${installHint} · ${error ?? ""}` : error ?? undefined}
          />
        </div>
      ) : loadState === "execution_failure" && accounts.length === 0 ? (
        <div className="mt-4">
          <WidgetState
            kind="error"
            title={t({
              ko: "Claude 계정 정보를 불러오지 못했습니다",
              en: "Unable to load Claude accounts",
              ja: "Claude アカウント情報を読み込めませんでした",
              zh: "无法加载 Claude 账号信息",
            })}
            description={error ?? undefined}
          />
        </div>
      ) : loadState === "loading" && accounts.length === 0 ? (
        <div className="mt-4">
          <WidgetState
            kind="loading"
            title={t({
              ko: "Claude 계정 사용량 동기화 중",
              en: "Loading Claude account usage",
              ja: "Claude アカウント使用量を読み込み中",
              zh: "正在加载 Claude 账号用量",
            })}
          />
        </div>
      ) : (
        <>
          {isUsageDataStale ? (
            <div className="mt-4">
              <WidgetState
                kind="stale"
                compact
                title={t({
                  ko: "최신이 아닌 사용량을 보여주고 있어요",
                  en: "Showing stale usage data",
                  ja: "古い使用量データを表示中",
                  zh: "正在显示过期用量数据",
                })}
                description={data?.staleReason ?? error ?? undefined}
              />
            </div>
          ) : null}

          {accounts.length === 0 ? (
            <div className="mt-4">
              <WidgetState
                kind="empty"
                title={t({
                  ko: "등록된 Claude 계정이 없습니다",
                  en: "No Claude accounts registered",
                  ja: "登録された Claude アカウントがありません",
                  zh: "没有已注册的 Claude 账号",
                })}
              />
            </div>
          ) : (
            <div className="mt-4 grid gap-4 xl:grid-cols-2">
              {accounts.map((account) => {
                const key = accountKey(account);
                const usageAge = formatUsageAge(account.usageAgeSeconds, t);
                return (
                  <SurfaceCard key={key} className="rounded-lg p-4">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <div className="min-w-0 text-sm font-medium [overflow-wrap:anywhere]" style={{ color: "var(--th-text)" }}>
                            {account.email ?? t({ ko: `계정 ${account.number ?? "-"}`, en: `Account ${account.number ?? "-"}`, ja: `アカウント ${account.number ?? "-"}`, zh: `账号 ${account.number ?? "-"}` })}
                          </div>
                          {account.active ? (
                            <StatusBadge tone="healthy" size="xs">
                              {t({ ko: "사용 중", en: "In use", ja: "使用中", zh: "使用中" })}
                            </StatusBadge>
                          ) : null}
                        </div>
                        <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {[account.number != null ? `#${account.number}` : null, usageAge || null]
                            .filter(Boolean)
                            .join(" · ")}
                        </div>
                      </div>
                    </div>

                    <div className="mt-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
                      <ClaudeUsageGauge label={t({ ko: "5시간", en: "5 hours", ja: "5時間", zh: "5小时" })} window={account.usage?.fiveHour} t={t} />
                      <ClaudeUsageGauge label={t({ ko: "7일", en: "7 days", ja: "7日", zh: "7天" })} window={account.usage?.sevenDay} t={t} />
                    </div>
                  </SurfaceCard>
                );
              })}
            </div>
          )}
        </>
      )}
    </SurfaceSection>
  );
}

const ClaudeAccountsWidget = memo(ClaudeAccountsWidgetImpl);
ClaudeAccountsWidget.displayName = "ClaudeAccountsWidget";

export default ClaudeAccountsWidget;
