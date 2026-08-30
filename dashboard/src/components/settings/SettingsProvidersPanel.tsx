import { Plus, RefreshCw, Terminal, X } from "lucide-react";
import type { CSSProperties } from "react";
import { SurfaceEmptyState as SettingsEmptyState } from "../common/SurfacePrimitives";
import {
  RATE_LIMIT_GAUGE_TRACK_STYLE,
  rateLimitFillStyle,
  rateLimitFillWidth,
} from "../common/rateLimitGauge";
import { getProviderLevelColors } from "../../app/providerTheme";
import {
  accountIsDefault,
  extraAccountCount,
  usageBarPercent,
  type PendingProviderLogin,
  type ProviderAuthAccount,
  type ProviderAuthProvider,
  type ProviderAuthUsageBucket,
} from "./SettingsProvidersModel";

interface SettingsProvidersPanelProps {
  error: string | null;
  loading: boolean;
  onAddAccount: (providerId: string) => void;
  onCompleteLogin: () => void;
  onRemoveAccount: (providerId: string, profileId: string) => void;
  onReload: () => void;
  pendingLogin: PendingProviderLogin | null;
  providers: ProviderAuthProvider[];
  secondaryActionClass: string;
  secondaryActionStyle: CSSProperties;
  startingProviderId: string | null;
  removingAccountKey: string | null;
  tr: (ko: string, en: string) => string;
}

export function SettingsProvidersPanel({
  error,
  loading,
  onAddAccount,
  onCompleteLogin,
  onRemoveAccount,
  onReload,
  pendingLogin,
  providers,
  secondaryActionClass,
  secondaryActionStyle,
  startingProviderId,
  removingAccountKey,
  tr,
}: SettingsProvidersPanelProps) {
  const extras = extraAccountCount(providers);

  return (
    <div className="space-y-4" data-testid="settings-providers-panel">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            `시스템 기본 홈은 그대로 두고 extra 계정만 누적합니다. extra ${extras}개.`,
            `System default homes stay untouched; extra accounts accumulate. ${extras} extra.`,
          )}
        </div>
        <div className="basis-full text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
          {tr(
            "같은 Codex 계정을 기본 홈과 extra 홈에 동시에 로그인하면 공급자 토큰이 서로 무효화될 수 있습니다. extra에는 별도 계정을 사용하세요.",
            "Logging the same Codex account into both the system home and an extra home can invalidate tokens. Use a distinct account for extras.",
          )}
        </div>
        <button
          type="button"
          onClick={onReload}
          disabled={loading}
          className={secondaryActionClass}
          style={secondaryActionStyle}
        >
          <RefreshCw size={13} />
          {loading ? tr("불러오는 중...", "Loading...") : tr("다시 불러오기", "Reload")}
        </button>
      </div>

      {error ? (
        <div
          className="rounded-2xl border px-4 py-3 text-sm"
          style={{
            borderColor: "rgba(248, 113, 113, 0.38)",
            background: "rgba(248, 113, 113, 0.10)",
            color: "var(--th-text)",
          }}
        >
          {error}
        </div>
      ) : null}

      {pendingLogin ? (
        <div
          className="rounded-2xl border px-4 py-3 text-sm"
          data-testid="settings-providers-login-pending"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 35%, transparent)",
            background: "color-mix(in srgb, var(--th-accent-primary) 8%, transparent)",
            color: "var(--th-text)",
          }}
        >
          <div className="flex items-center gap-2 font-medium">
            <Terminal size={14} />
            {tr(
              `${pendingLogin.providerId} extra 로그인 진행 중`,
              `${pendingLogin.providerId} extra login in progress`,
            )}
          </div>
          <div className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "대시보드에는 PTY가 없습니다. 터미널에서 아래 명령으로 붙인 뒤 vendor 로그인을 마치고 완료를 누르세요.",
              "This dashboard has no PTY. Attach in a terminal, finish vendor login, then complete.",
            )}
          </div>
          <pre
            className="mt-2 overflow-x-auto rounded-xl px-3 py-2 text-[11px]"
            style={{
              background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
            }}
          >
            {pendingLogin.attach}
          </pre>
          <div className="mt-3">
            <button
              type="button"
              onClick={onCompleteLogin}
              className={secondaryActionClass}
              style={secondaryActionStyle}
              data-testid="settings-providers-login-complete"
            >
              {tr("로그인 완료 확인", "Complete login")}
            </button>
          </div>
        </div>
      ) : null}

      {loading && providers.length === 0 ? (
        <SettingsEmptyState className="text-sm">
          {tr("프로바이더 계정을 불러오는 중입니다.", "Loading provider accounts.")}
        </SettingsEmptyState>
      ) : (
        <div className="grid gap-3">
          {providers.map((provider) => (
            <ProviderCard
              key={provider.id}
              onAddAccount={() => onAddAccount(provider.id)}
              onRemoveAccount={(profileId) => onRemoveAccount(provider.id, profileId)}
              provider={provider}
              secondaryActionClass={secondaryActionClass}
              secondaryActionStyle={secondaryActionStyle}
              starting={startingProviderId === provider.id}
              removingAccountKey={removingAccountKey}
              tr={tr}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function ProviderCard({
  onAddAccount,
  onRemoveAccount,
  provider,
  secondaryActionClass,
  secondaryActionStyle,
  starting,
  removingAccountKey,
  tr,
}: {
  onAddAccount: () => void;
  onRemoveAccount: (profileId: string) => void;
  provider: ProviderAuthProvider;
  secondaryActionClass: string;
  secondaryActionStyle: CSSProperties;
  starting: boolean;
  removingAccountKey: string | null;
  tr: (ko: string, en: string) => string;
}) {
  const accounts = provider.accounts ?? [];
  return (
    <section
      className="rounded-2xl border px-4 py-4"
      data-testid={`settings-provider-${provider.id}`}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-overlay-medium) 70%, transparent)",
      }}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {provider.id}
          </div>
          <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
            {tr("시스템 기본", "System default")}: {provider.default_home}
          </div>
        </div>
        <button
          type="button"
          onClick={onAddAccount}
          disabled={starting}
          className={secondaryActionClass}
          style={secondaryActionStyle}
          data-testid={`settings-provider-add-${provider.id}`}
        >
          <Plus size={13} />
          {starting ? tr("로그인 준비 중...", "Starting...") : tr("계정 추가", "Add account")}
        </button>
      </div>
      <div className="mt-3 space-y-2">
        {accounts.map((account) => (
          <AccountRow
            key={`${provider.id}:${account.id}`}
            account={account}
            onRemove={() => onRemoveAccount(account.id)}
            removing={removingAccountKey === `${provider.id}:${account.id}`}
            tr={tr}
          />
        ))}
      </div>
    </section>
  );
}

function AccountRow({
  account,
  onRemove,
  removing,
  tr,
}: {
  account: ProviderAuthAccount;
  onRemove: () => void;
  removing: boolean;
  tr: (ko: string, en: string) => string;
}) {
  const isDefault = accountIsDefault(account);
  const buckets = account.usage?.buckets ?? [];
  return (
    <div
      className="rounded-xl border px-3 py-3"
      data-testid={`settings-provider-account-${account.id}`}
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
        background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
          {isDefault ? tr("시스템 기본", "System default") : account.id}
        </div>
        {isDefault ? null : (
          <button
            type="button"
            onClick={onRemove}
            disabled={removing}
            className="rounded-lg p-1 disabled:opacity-50"
            style={{ color: "var(--th-text-muted)" }}
            title={tr("연결 해제 (자격 증명은 유지)", "Unlink (keep credentials)")}
            aria-label={tr("계정 연결 해제", "Unlink account")}
          >
            <X size={14} />
          </button>
        )}
        {account.usage?.unsupported ? (
          <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
            {account.usage.reason ?? tr("사용량 미지원", "Usage unsupported")}
          </span>
        ) : null}
      </div>
      <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
        {account.home}
      </div>
      {(account.bound_agents ?? []).length > 0 ? (
        <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
          {tr("연결된 에이전트", "Bound agents")}: {(account.bound_agents ?? []).join(", ")}
        </div>
      ) : null}
      {buckets.length > 0 ? (
        <div className="mt-2 space-y-1.5">
          {buckets.map((bucket, index) => (
            <UsageBar key={`${account.id}:${bucket.name ?? bucket.label ?? index}`} bucket={bucket} />
          ))}
        </div>
      ) : null}
    </div>
  );
}

function UsageBar({ bucket }: { bucket: ProviderAuthUsageBucket }) {
  const percent = usageBarPercent(bucket);
  const label = bucket.label ?? bucket.name ?? "usage";
  const colors = getProviderLevelColors("codex", percent !== null && percent >= 95 ? "danger" : percent !== null && percent >= 80 ? "warning" : "normal");
  return (
    <div className="min-w-0" data-testid="settings-provider-usage-bar">
      <div className="mb-1 flex items-center justify-between gap-2 text-[11px]">
        <span style={{ color: "var(--th-text-muted)" }}>{label}</span>
        <span style={{ color: "var(--th-text)" }}>{percent === null ? "—" : `${percent}%`}</span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full" style={RATE_LIMIT_GAUGE_TRACK_STYLE}>
        <div
          className="h-full rounded-full"
          style={{
            width: rateLimitFillWidth(percent),
            ...rateLimitFillStyle(colors.bar, colors.glow, 6),
          }}
        />
      </div>
    </div>
  );
}
