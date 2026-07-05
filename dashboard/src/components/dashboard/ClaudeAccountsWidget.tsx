import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { CheckCircle2, KeyRound, Loader2, RefreshCw } from "lucide-react";
import TooltipLabel from "../common/TooltipLabel";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceSection,
} from "../common/SurfacePrimitives";
import { StatusBadge } from "../common/StatusBadge";
import { WidgetState } from "../common/WidgetState";
import {
  RATE_LIMIT_GAUGE_TRACK_STYLE,
  rateLimitFillStyle,
  rateLimitFillWidth,
} from "../common/rateLimitGauge";
import type { TFunction } from "./model";

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
  status?: "not_installed" | "execution_failure" | "switch_in_progress" | "bad_request";
  code?: string;
  hostname?: string;
  instanceId?: string | null;
  error?: string;
  install?: {
    command?: string;
    binaryHint?: string;
  };
}

interface SwitchResponse {
  status?: string;
  switched?: boolean;
  from?: unknown;
  to?: unknown;
  reason?: string | null;
  hostname?: string;
  switchedAt?: string;
  rateLimitRefresh?: {
    triggered?: boolean;
    dispatchGateRefreshed?: boolean;
    reason?: string | null;
    error?: string | null;
  };
  error?: string;
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

function accountKey(account: ClaudeAccount): string {
  return account.email ?? (account.number == null ? "unknown" : String(account.number));
}

function accountSwitchTarget(account: ClaudeAccount): string | null {
  if (account.email && account.email.trim()) return account.email;
  if (account.number != null) return String(account.number);
  return null;
}

function formatUnknown(value: unknown): string {
  if (value == null) return "-";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (typeof value === "object") {
    const maybeRecord = value as Record<string, unknown>;
    const email = maybeRecord.email;
    const number = maybeRecord.number;
    if (typeof email === "string") return email;
    if (typeof number === "number" || typeof number === "string") return String(number);
  }
  return JSON.stringify(value);
}

function switchReasonLabel(reason: string | null | undefined, t: TFunction): string | null {
  if (!reason) return null;
  switch (reason) {
    case "manual":
      return t({ ko: "수동 전환", en: "Manual switch", ja: "手動切替", zh: "手动切换" });
    case "already_active":
    case "unchanged":
    case "same_account":
      return t({ ko: "이미 활성 상태", en: "Already active", ja: "すでに有効", zh: "已是活动账号" });
    default:
      return null;
  }
}

function refreshReasonLabel(reason: string | null | undefined, t: TFunction): string | null {
  if (!reason) return null;
  switch (reason) {
    case "refresh_scheduled":
      return t({
        ko: "사용량 새로고침 예약됨",
        en: "Usage refresh queued",
        ja: "使用量更新を予約済み",
        zh: "用量刷新已排队",
      });
    case "rate_limit_sync_not_active_on_this_node":
      return t({
        ko: "사용량 새로고침은 리더 노드에서 실행됩니다",
        en: "Usage refresh runs on the leader node",
        ja: "使用量更新はリーダーノードで実行されます",
        zh: "用量刷新在主节点执行",
      });
    case "postgres_pool_unavailable":
      return t({
        ko: "사용량 새로고침을 사용할 수 없습니다",
        en: "Usage refresh unavailable",
        ja: "使用量更新を利用できません",
        zh: "用量刷新不可用",
      });
    case "serialization_failure":
      return t({
        ko: "새로고침 상태를 표시하지 못했습니다",
        en: "Unable to report refresh status",
        ja: "更新状態を表示できません",
        zh: "无法显示刷新状态",
      });
    case "sync_failed":
      return t({
        ko: "사용량 새로고침 실패",
        en: "Usage refresh failed",
        ja: "使用量更新に失敗しました",
        zh: "用量刷新失败",
      });
    default:
      return t({
        ko: "사용량 새로고침 상태를 확인할 수 없습니다",
        en: "Usage refresh status unavailable",
        ja: "使用量更新状態を確認できません",
        zh: "无法确认用量刷新状态",
      });
  }
}

function switchSuccessDescription(result: SwitchResponse, t: TFunction): string {
  const refreshReason = refreshReasonLabel(result.rateLimitRefresh?.reason, t);
  if (result.switched === false) {
    const account = formatUnknown(result.to ?? result.from);
    const alreadyActive =
      account === "-"
        ? t({ ko: "선택한 계정이 이미 활성 상태입니다", en: "Selected account is already active", ja: "選択したアカウントはすでに有効です", zh: "所选账号已是活动账号" })
        : t({ ko: `${account} 계정이 이미 활성 상태입니다`, en: `${account} is already active`, ja: `${account} はすでに有効です`, zh: `${account} 已是活动账号` });
    return [alreadyActive, refreshReason].filter(Boolean).join(" · ");
  }

  const switchReason = switchReasonLabel(result.reason, t);
  return [
    `${formatUnknown(result.from)} → ${formatUnknown(result.to)}`,
    switchReason,
    refreshReason,
  ]
    .filter(Boolean)
    .join(" · ");
}

function formatTimeRemaining(resetsAt: string | null | undefined): string {
  if (!resetsAt) return "";
  const diff = new Date(resetsAt).getTime() - Date.now();
  if (!Number.isFinite(diff) || diff <= 0) return "now";
  const days = Math.floor(diff / 86400000);
  const hours = Math.floor((diff % 86400000) / 3600000);
  const minutes = Math.floor((diff % 3600000) / 60000);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

function formatAge(seconds: number | null | undefined): string {
  if (seconds == null || seconds < 0) return "";
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
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
}

function ClaudeUsageGauge({ label, window }: ClaudeUsageGaugeProps) {
  const rawPct = typeof window?.pct === "number" ? Math.round(window.pct) : null;
  const level = usageLevel(rawPct);
  const colors = usageColors(level);
  const remaining = formatTimeRemaining(window?.resetsAt);

  return (
    <div>
      <div className="mb-1.5 flex items-center justify-between gap-2">
        <span className="text-xs font-bold" style={{ color: colors.text }}>
          {label}
        </span>
        <span
          className="text-xs font-mono font-bold"
          style={{ color: rawPct == null ? "var(--th-text-muted)" : colors.text }}
        >
          {rawPct == null ? "N/A" : `${rawPct}%`}
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
      {remaining ? (
        <span className="mt-1 inline-flex whitespace-nowrap text-[10px]" style={{ color: "var(--th-text-muted)", lineHeight: 1.2 }}>
          resets {remaining}
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
  const [switchingAccount, setSwitchingAccount] = useState<string | null>(null);
  const [switchResult, setSwitchResult] = useState<SwitchResponse | null>(null);

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
        const res = await fetch("/api/claude-accounts", {
          credentials: "include",
          signal: controller.signal,
        });
        const raw = (await res.json()) as ClaudeAccountsResponse | ClaudeAccountsErrorResponse;
        if (controller.signal.aborted) return;

        if (!res.ok) {
          const errorPayload = raw as ClaudeAccountsErrorResponse;
          setLoadState(errorPayload.status === "not_installed" ? "not_installed" : "execution_failure");
          setError(errorPayload.error ?? `HTTP ${res.status}`);
          setInstallHint(errorPayload.install?.command ?? null);
          return;
        }

        setData(raw as ClaudeAccountsResponse);
        setLoadState("ready");
        setError(null);
        setInstallHint(null);
      } catch (nextError) {
        if (controller.signal.aborted) return;
        setLoadState("execution_failure");
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
  }, [refreshNonce]);

  const handleSwitch = useCallback(
    async (target: string) => {
      const { controller, cleanup } = createTimedController(CLAUDE_ACCOUNTS_FETCH_TIMEOUT_MS);
      setSwitchingAccount(target);
      setSwitchResult(null);

      try {
        const res = await fetch("/api/claude-accounts/switch", {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ account: target }),
          signal: controller.signal,
        });
        const raw = (await res.json()) as SwitchResponse;
        if (controller.signal.aborted) return;

        if (!res.ok) {
          setSwitchResult({ ...raw, status: raw.status ?? "execution_failure", error: raw.error ?? `HTTP ${res.status}` });
          return;
        }

        setSwitchResult(raw);
        requestRefresh();
      } catch (nextError) {
        if (!controller.signal.aborted) {
          setSwitchResult({
            status: "execution_failure",
            error: nextError instanceof Error ? nextError.message : String(nextError),
          });
        }
      } finally {
        cleanup();
        if (!controller.signal.aborted) setSwitchingAccount(null);
      }
    },
    [requestRefresh],
  );

  const fetchedAgeMs = useMemo(() => {
    if (!data?.fetchedAt) return null;
    const timestamp = new Date(data.fetchedAt).getTime();
    if (!Number.isFinite(timestamp)) return null;
    return Date.now() - timestamp;
  }, [data?.fetchedAt]);
  const isUsageDataStale =
    Boolean(data?.usageDataStale) || (fetchedAgeMs != null && fetchedAgeMs > CLAUDE_ACCOUNTS_STALE_MS);
  const accounts = data?.accounts ?? [];
  const nodeLabel = data?.hostname ?? switchResult?.hostname ?? "";

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
      className="inline-flex h-7 w-7 items-center justify-center rounded-full"
      style={{
        border: "1px solid rgba(148,163,184,0.22)",
        background: "rgba(148,163,184,0.14)",
        color: "var(--th-text)",
        cursor: isRefreshing ? "wait" : "pointer",
        opacity: isRefreshing ? 0.6 : 1,
        transition: "opacity 120ms ease",
      }}
    >
      <RefreshCw size={12} className={isRefreshing ? "animate-spin" : undefined} aria-hidden />
    </button>
  );

  const tooltip = t({
    ko: "이 노드의 cswap 계정 목록과 5h/7d 사용량을 표시하고, 선택한 계정으로 macOS 전역 Claude 인증을 전환합니다.",
    en: "Shows this node's cswap accounts, 5h/7d usage, and switches the machine-wide Claude auth to the selected account.",
    ja: "このノードの cswap アカウント、5h/7d 使用量を表示し、選択したアカウントへマシン全体の Claude 認証を切り替えます。",
    zh: "显示此节点的 cswap 账号、5h/7d 用量，并将整机 Claude 认证切换到所选账号。",
  });

  const sectionActions = (
    <>
      {nodeLabel ? (
        <StatusBadge tone="info" size="xs" title={nodeLabel}>
          {nodeLabel}
        </StatusBadge>
      ) : null}
      <TooltipLabel
        text={t({ ko: "설명", en: "About", ja: "説明", zh: "说明" })}
        tooltip={tooltip}
        className="max-w-fit text-sm"
      />
      {refreshButton}
    </>
  );

  return (
    <SurfaceSection
      eyebrow={t({ ko: "Claude", en: "Claude", ja: "Claude", zh: "Claude" })}
      title={t({ ko: "Claude 계정", en: "Claude Accounts", ja: "Claude アカウント", zh: "Claude 账号" })}
      description={t({
        ko: "현재 노드의 전역 Claude 인증 계정과 계정별 사용량을 확인합니다.",
        en: "Inspect this node's global Claude auth account and per-account usage.",
        ja: "このノードのグローバル Claude 認証アカウントとアカウント別使用量を確認します。",
        zh: "查看此节点的全局 Claude 认证账号和各账号用量。",
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
                  ko: "오래된 사용량 데이터를 표시 중",
                  en: "Showing stale usage data",
                  ja: "古い使用量データを表示中",
                  zh: "正在显示过期用量数据",
                })}
                description={data?.staleReason ?? error ?? undefined}
              />
            </div>
          ) : null}

          {switchResult ? (
            <div className="mt-4">
              <WidgetState
                kind={switchResult.status === "ok" ? (switchResult.switched === false ? "empty" : "stale") : "error"}
                tone={switchResult.status === "ok" ? (switchResult.switched === false ? "idle" : "healthy") : undefined}
                compact
                icon={switchResult.status === "ok" ? (switchResult.switched === false ? <KeyRound size={18} aria-hidden /> : <CheckCircle2 size={18} aria-hidden />) : undefined}
                title={
                  switchResult.status === "ok"
                    ? switchResult.switched === false
                      ? t({
                          ko: `${switchResult.hostname ?? nodeLabel} 노드에서 이미 활성 상태`,
                          en: `Already active on ${switchResult.hostname ?? nodeLabel}`,
                          ja: `${switchResult.hostname ?? nodeLabel} ノードですでに有効`,
                          zh: `${switchResult.hostname ?? nodeLabel} 节点已是活动账号`,
                        })
                      : t({
                          ko: `${switchResult.hostname ?? nodeLabel} 노드에 적용됨`,
                          en: `Applied on ${switchResult.hostname ?? nodeLabel}`,
                          ja: `${switchResult.hostname ?? nodeLabel} ノードに適用済み`,
                          zh: `已应用到 ${switchResult.hostname ?? nodeLabel}`,
                        })
                    : t({
                        ko: "계정 전환 실패",
                        en: "Account switch failed",
                        ja: "アカウント切替に失敗しました",
                        zh: "账号切换失败",
                      })
                }
                description={
                  switchResult.status === "ok"
                    ? switchSuccessDescription(switchResult, t)
                    : switchResult.error
                }
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
                const target = accountSwitchTarget(account);
                const key = accountKey(account);
                const usageAge = formatAge(account.usageAgeSeconds);
                const isSwitching = switchingAccount === target;
                return (
                  <SurfaceCard key={key} className="rounded-3xl p-5">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="flex min-w-0 flex-wrap items-center gap-2">
                          <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                            {account.email ?? t({ ko: `계정 ${account.number ?? "-"}`, en: `Account ${account.number ?? "-"}`, ja: `アカウント ${account.number ?? "-"}`, zh: `账号 ${account.number ?? "-"}` })}
                          </div>
                          {account.active ? (
                            <StatusBadge tone="healthy" size="xs" pulse title="active">
                              active
                            </StatusBadge>
                          ) : null}
                        </div>
                        <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {account.number != null ? `#${account.number}` : "no slot"}
                          {account.usageStatus ? ` · ${account.usageStatus}` : ""}
                          {usageAge ? ` · ${usageAge} old` : ""}
                        </div>
                      </div>

                      <SurfaceActionButton
                        tone={account.active ? "neutral" : "info"}
                        compact
                        disabled={account.active || !target || Boolean(switchingAccount)}
                        onClick={target ? () => void handleSwitch(target) : undefined}
                        className="gap-1.5"
                      >
                        {isSwitching ? (
                          <Loader2 size={12} className="motion-safe:animate-spin" aria-hidden />
                        ) : (
                          <KeyRound size={12} aria-hidden />
                        )}
                        <span>{account.active ? "Active" : "Switch"}</span>
                      </SurfaceActionButton>
                    </div>

                    <div className="mt-4 grid grid-cols-1 gap-4 sm:grid-cols-2">
                      <ClaudeUsageGauge label="5h" window={account.usage?.fiveHour} />
                      <ClaudeUsageGauge label="7d" window={account.usage?.sevenDay} />
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
