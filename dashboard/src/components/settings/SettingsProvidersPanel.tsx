import { Check, Plus, RefreshCw, Terminal } from "lucide-react";
import {
  RATE_LIMIT_GAUGE_TRACK_STYLE,
  rateLimitFillStyle,
  rateLimitFillWidth,
} from "../common/rateLimitGauge";
import { getProviderLevelColors } from "../../app/providerTheme";
import { providerDisplayName, usageBucketLabel } from "./SettingsCopy";
import { ActionMenu } from "../common/ActionMenu";
import { SETTINGS_SECONDARY_BUTTON, SettingsStatusMessage } from "./SettingsPanels";
import {
  accountIsDefault,
  extraAccountCount,
  usageBarPercent,
  type ProviderAuthAccount,
  type ProviderAuthProvider,
  type ProviderAuthUsageBucket,
} from "./SettingsProvidersModel";
import type { useProviderAccounts } from "./useSettingsRemoteData";

type Tr = (ko: string, en: string) => string;
type ProviderAccounts = ReturnType<typeof useProviderAccounts>;

interface SettingsProvidersPanelProps {
  accounts: ProviderAccounts;
  warningPct: number;
  dangerPct: number;
  tr: Tr;
}

export function SettingsProvidersPanel({ accounts, warningPct, dangerPct, tr }: SettingsProvidersPanelProps) {
  const { providers, loading, error, pendingLogin } = accounts;
  const extras = extraAccountCount(providers);

  return (
    <div className="space-y-4" data-testid="settings-providers-panel">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-[14rem] flex-1 space-y-1 text-xs leading-5 break-keep text-th-text-muted">
          <p>
            {tr(
              `추가 계정 ${extras}개가 연결돼 있어요. 추가 계정 로그인을 지원하는 서비스만 보여요.`,
              `${extras} additional ${extras === 1 ? "account is" : "accounts are"} connected. Only services that support extra sign-ins are listed.`,
            )}
          </p>
          {providers.some((provider) => provider.id === "codex") ? <p>
            {tr(
              "같은 Codex 계정을 기본 계정과 추가 계정에 함께 로그인하면 서로 로그인이 풀릴 수 있어요. 추가 계정에는 다른 계정을 쓰세요.",
              "Signing the same Codex account into both the default and an extra slot can sign both out. Use a different account for extras.",
            )}
          </p> : null}
        </div>
        <button type="button" onClick={accounts.reload} disabled={loading} className={SETTINGS_SECONDARY_BUTTON}>
          <RefreshCw size={13} aria-hidden className={loading ? "animate-spin" : undefined} />
          {loading ? tr("불러오는 중…", "Loading…") : tr("다시 불러오기", "Reload")}
        </button>
      </div>

      {error ? (
        <p role="alert" className="rounded-md border px-4 py-3 text-sm break-keep" style={{ borderColor: "color-mix(in srgb, var(--th-accent-danger) 40%, transparent)", color: "var(--th-text)" }}>
          {error}
        </p>
      ) : null}

      {pendingLogin ? (
        <div
          className="rounded-lg border px-4 py-3 text-sm"
          data-testid="settings-providers-login-pending"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 35%, transparent)",
            background: "color-mix(in srgb, var(--th-accent-primary) 8%, transparent)",
          }}
        >
          <div className="flex items-center gap-2 font-medium text-th-text-primary">
            <Terminal size={14} aria-hidden />
            {tr(
              `${providerDisplayName(pendingLogin.providerId)} 추가 계정 로그인 중`,
              `Signing in an additional ${providerDisplayName(pendingLogin.providerId)} account`,
            )}
          </div>
          <p className="mt-2 text-xs leading-5 break-keep text-th-text-muted">
            {tr(
              "대시보드에서는 로그인 창을 열 수 없어요. 터미널에서 아래 명령으로 접속해 로그인을 마친 뒤 '로그인 완료 확인'을 눌러 주세요.",
              "Sign-in can't run inside the dashboard. Attach with the command below in a terminal, finish signing in, then press “Check sign-in”.",
            )}
          </p>
          <pre className="mt-2 overflow-x-auto rounded-md px-3 py-2 text-[11px]" style={{ background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)" }}>
            {pendingLogin.attach}
          </pre>
          <button type="button" onClick={() => void accounts.completeLogin()} className={`${SETTINGS_SECONDARY_BUTTON} mt-3`} data-testid="settings-providers-login-complete">
            {tr("로그인 완료 확인", "Check sign-in")}
          </button>
        </div>
      ) : null}

      {loading && providers.length === 0 ? (
        <SettingsStatusMessage>{tr("AI 계정을 불러오는 중…", "Loading AI accounts…")}</SettingsStatusMessage>
      ) : providers.length === 0 && !error ? (
        <SettingsStatusMessage>{tr("표시할 계정이 없어요. 터미널에서 각 AI 도구에 로그인했는지 확인한 뒤 다시 불러와 주세요.", "No accounts to show. Check that each AI tool is signed in, then reload.")}</SettingsStatusMessage>
      ) : (
        <div className="grid gap-3">
          {providers.map((provider) => (
            <ProviderCard
              key={provider.id}
              provider={provider}
              accounts={accounts}
              warningPct={warningPct}
              dangerPct={dangerPct}
              tr={tr}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function ProviderCard({ provider, accounts, warningPct, dangerPct, tr }: {
  provider: ProviderAuthProvider;
  accounts: ProviderAccounts;
  warningPct: number;
  dangerPct: number;
  tr: Tr;
}) {
  const name = providerDisplayName(provider.id);
  const starting = accounts.startingProviderId === provider.id;
  return (
    <section
      className="rounded-lg border px-4 py-4"
      data-testid={`settings-provider-${provider.id}`}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-overlay-medium) 60%, transparent)",
      }}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h4 className="text-sm font-semibold text-th-text-heading">{name}</h4>
          <p className="mt-1 text-[11px] leading-5 text-th-text-muted [overflow-wrap:anywhere]">
            {tr("기본 로그인 위치", "Default sign-in folder")}: {provider.default_home}
          </p>
        </div>
        <button
          type="button"
          onClick={() => void accounts.addAccount(provider.id)}
          disabled={starting}
          className={SETTINGS_SECONDARY_BUTTON}
          data-testid={`settings-provider-add-${provider.id}`}
        >
          <Plus size={13} aria-hidden />
          {starting ? tr("로그인 준비 중…", "Starting…") : tr("계정 추가", "Add account")}
        </button>
      </div>
      <div className="mt-3 space-y-2">
        {(provider.accounts ?? []).map((account) => (
          <AccountRow
            key={`${provider.id}:${account.id}`}
            providerName={name}
            account={account}
            isPrimary={(provider.primary_profile_id ?? "default") === account.id}
            removing={accounts.removingAccountKey === `${provider.id}:${account.id}`}
            onRemove={() => accounts.removeAccount(provider.id, account.id)}
            onSetPrimary={() => void accounts.setPrimary(provider.id, account.id)}
            warningPct={warningPct}
            dangerPct={dangerPct}
            tr={tr}
          />
        ))}
      </div>
    </section>
  );
}

function AccountRow({ providerName, account, isPrimary, removing, onRemove, onSetPrimary, warningPct, dangerPct, tr }: {
  providerName: string;
  account: ProviderAuthAccount;
  isPrimary: boolean;
  removing: boolean;
  onRemove: () => Promise<void>;
  onSetPrimary: () => void;
  warningPct: number;
  dangerPct: number;
  tr: Tr;
}) {
  const isDefault = accountIsDefault(account);
  const buckets = account.usage?.buckets ?? [];
  return (
    <div
      className="rounded-md border px-3 py-3"
      data-testid={`settings-provider-account-${account.id}`}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
        background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="min-w-0 text-sm font-medium text-th-text-primary [overflow-wrap:anywhere]">
          {isDefault ? tr("기본 로그인", "Default sign-in") : account.id}
        </div>
        <div className="ml-auto flex shrink-0 items-center gap-1">
          <button
            type="button"
            onClick={onSetPrimary}
            disabled={isPrimary}
            aria-pressed={isPrimary}
            className="inline-flex min-h-11 items-center justify-center gap-1 rounded-md border border-th-border px-2.5 text-xs text-th-text-secondary disabled:opacity-80"
            title={tr("이 서비스에서 먼저 쓸 계정으로 정해요", "Use this account first for this service")}
          >
            {isPrimary ? <Check size={12} aria-hidden /> : null}
            {isPrimary ? tr("기본 계정", "Primary") : tr("기본으로 쓰기", "Make primary")}
          </button>
          {isDefault ? null : (
            <ActionMenu
              label={tr(`${account.id} 계정 메뉴`, `${account.id} account menu`)}
              items={[{
                key: "unlink",
                label: removing ? tr("해제 중…", "Unlinking…") : tr("연결 해제", "Unlink"),
                danger: true,
                disabled: removing,
                onSelect: onRemove,
                confirm: {
                  title: tr(`${providerName} “${account.id}” 계정 연결을 해제할까요?`, `Unlink ${providerName} “${account.id}”?`),
                  description: tr("저장된 로그인 정보 폴더는 그대로 남아요.", "Its saved sign-in folder is kept."),
                  confirmLabel: tr("연결 해제", "Unlink"),
                  cancelLabel: tr("취소", "Cancel"),
                },
              }]}
            />
          )}
        </div>
      </div>
      <p className="mt-1 text-[11px] leading-5 text-th-text-muted [overflow-wrap:anywhere]">{account.home}</p>
      {(account.bound_agents ?? []).length > 0 ? (
        <p className="mt-1 text-[11px] leading-5 text-th-text-muted [overflow-wrap:anywhere]">
          {tr("이 계정을 쓰는 에이전트", "Agents using this account")}: {(account.bound_agents ?? []).join(", ")}
        </p>
      ) : null}
      {(account.bound_channels ?? []).length > 0 ? (
        <p className="mt-1 text-[11px] leading-5 text-th-text-muted [overflow-wrap:anywhere]">
          {tr("이 계정을 쓰는 채널", "Channels using this account")}: {(account.bound_channels ?? []).join(", ")}
        </p>
      ) : null}
      {account.usage?.unsupported ? (
        <p className="mt-2 text-xs leading-5 break-keep text-th-text-muted">
          {tr(`${providerName} 사용량은 아직 볼 수 없어요.`, `${providerName} usage is not available yet.`)}
        </p>
      ) : buckets.length > 0 ? (
        <div className="mt-2 space-y-2">
          {account.usage?.stale ? (
            <p className="text-[11px] leading-5 break-keep text-th-text-muted">
              {tr("사용량 정보가 오래됐어요. 곧 다시 받아요.", "Usage data is out of date and will refresh soon.")}
            </p>
          ) : null}
          {buckets.map((bucket, index) => (
            <UsageBar
              key={`${account.id}:${bucket.name ?? bucket.label ?? index}`}
              bucket={bucket}
              warningPct={warningPct}
              dangerPct={dangerPct}
              tr={tr}
            />
          ))}
        </div>
      ) : null}
    </div>
  );
}

function formatReset(reset: number | undefined, tr: Tr): string | null {
  if (!reset || reset <= 0) return null;
  const date = new Date(reset < 1e12 ? reset * 1000 : reset);
  if (!Number.isFinite(date.getTime())) return null;
  const text = date.toLocaleString(tr("ko-KR", "en-US"), { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
  return tr(`${text}에 초기화`, `Resets ${text}`);
}

function UsageBar({ bucket, warningPct, dangerPct, tr }: {
  bucket: ProviderAuthUsageBucket;
  warningPct: number;
  dangerPct: number;
  tr: Tr;
}) {
  const percent = usageBarPercent(bucket);
  const label = usageBucketLabel(bucket.label ?? bucket.name ?? "", tr) || tr("사용량", "Usage");
  const level = percent !== null && percent >= dangerPct ? "danger" : percent !== null && percent >= warningPct ? "warning" : "normal";
  const colors = getProviderLevelColors("codex", level);
  const reset = formatReset(bucket.reset, tr);
  return (
    <div className="min-w-0" data-testid="settings-provider-usage-bar" data-level={level}>
      <div className="mb-1 flex flex-wrap items-baseline justify-between gap-x-2 text-[11px]">
        <span className="break-keep text-th-text-secondary">{label}</span>
        <span className="tabular-nums text-th-text-primary">
          {percent === null ? tr("확인 안 됨", "Unknown") : tr(`${percent}% 사용`, `${percent}% used`)}
          {reset ? <span className="ml-2 text-th-text-muted">{reset}</span> : null}
        </span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full" style={RATE_LIMIT_GAUGE_TRACK_STYLE}>
        <div className="h-full rounded-full" style={{ width: rateLimitFillWidth(percent), ...rateLimitFillStyle(colors.bar, colors.glow, 6) }} />
      </div>
    </div>
  );
}
