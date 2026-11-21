import { useQueries, type UseQueryOptions } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { useRef, useState, type CSSProperties, type KeyboardEvent, type ReactNode } from "react";
import type { AuditOrigin, AuditProvider } from "../../api/promptAudit";
import { getProviderLabel } from "../../app/providerTheme";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import type { Agent } from "../../types";
import { copyText } from "../agent-manager/TurnTranscriptModel";
import { formatTranscriptTimestamp } from "../agent-manager/turn-transcript-utils";
import { Cap, KitCard, Num, Well } from "../common/KitPrimitives";
import { auditProviders, defaultAuditProvider } from "./auditModel";
import { AuditDetailHeader } from "./AuditWorkspace";

type Tr = (ko: string, en: string) => string;

/** Full-width 44px touch button style used for phone fold footers inside cards. */
export const FULL_TOUCH_BUTTON_STYLE = {
  "--aui-button-clean-h": "44px",
  "--aui-button-clean-muted": "var(--fg-dim)",
  width: "100%",
} as CSSProperties;

/** Raw prompt or anchor text inside a `Well`: line breaks kept, long lines wrapped cleanly. */
export const AUDIT_PRE_CLASS = "m-0 overflow-x-hidden font-mono text-xs leading-5 whitespace-pre-wrap text-[var(--fg-dim)] [overflow-wrap:anywhere]";

/** What every tab's detail gets from the page: the open agent, its provider choice and the layout. */
export interface AuditDetailProps {
  agent: Agent;
  provider: AuditProvider;
  providers: AuditProvider[];
  onProvider: (provider: AuditProvider) => void;
  onBack?: () => void;
  phone: boolean;
  isKo: boolean;
  tr: Tr;
}

export const counter = (isKo: boolean) => (value: number) => value.toLocaleString(isKo ? "ko-KR" : "en-US");

/** Reads already cached in this visit; observers stay disabled so the agent list never triggers extra requests. */
export function useCachedReads<T, K extends readonly unknown[]>(
  list: Agent[],
  read: (agentId: string, provider: AuditProvider) => UseQueryOptions<T, Error, T, K>,
): (agent: Agent) => T | undefined {
  const reads = useQueries({ queries: list.map((agent) => ({ ...read(agent.id, defaultAuditProvider(agent) ?? "claude"), enabled: false })) });
  return (agent) => reads[list.indexOf(agent)]?.data as T | undefined;
}

/** List captions from reads already cached in this visit, falling back to bound provider labels. */
export function useCachedCaptions<T, K extends readonly unknown[]>(
  list: Agent[],
  read: (agentId: string, provider: AuditProvider) => UseQueryOptions<T, Error, T, K>,
  describe: (data: T) => string,
): (agent: Agent) => string {
  const cached = useCachedReads(list, read);
  return (agent) => {
    const data = cached(agent);
    return data ? describe(data) : auditProviders(agent).map(getProviderLabel).join(" · ");
  };
}

/** Provider, session start (KST) and where the copy came from; `saved` names the copy when no session is live. */
export function sessionMeta({ provider, providers, phone, isKo, tr }: AuditDetailProps, startedAt: string | null | undefined, origin: AuditOrigin | undefined, saved: string): string {
  const started = startedAt ? formatTranscriptTimestamp(startedAt, isKo) : null;
  return [
    phone && providers.length > 1 ? null : getProviderLabel(provider),
    started ? tr(`${started} 시작`, `started ${started}`) : null,
    origin === "current_session" ? tr("현재 세션", "Current session") : origin === "latest_saved" ? `${tr("세션 없음", "No session")} · ${saved}` : null,
  ].filter(Boolean).join(" · ");
}

/** Copies text and reports the outcome for two seconds, per key when several buttons copy. */
export function useCopyFeedback<K>(tr: Tr) {
  const [copied, setCopied] = useState<{ key: K; ok: boolean } | null>(null);
  const copy = (key: K, text: string) => {
    void copyText(text).then(() => true, () => false).then((ok) => {
      setCopied({ key, ok });
      window.setTimeout(() => setCopied((current) => (current?.key === key ? null : current)), 2000);
    });
  };
  const label = (key: K, idle: string) => (copied?.key !== key ? idle : copied.ok ? tr("복사했어요", "Copied") : tr("복사하지 못했어요", "Could not copy"));
  return { copy, label, done: (key: K) => copied?.key === key && copied.ok };
}

export interface ShareSegment {
  key: string;
  value: number;
  color: string;
  label?: string;
}

/** Multi-segment proportional bar (`10px` on desktop, `8px` on phone) with optional swatch legend; candidate for `KitPrimitives`. */
export function ShareBar({
  segments,
  ariaLabel,
  compact = false,
  testId,
}: {
  segments: ShareSegment[];
  ariaLabel: string;
  compact?: boolean;
  testId?: string;
}) {
  const labelled = segments.filter((segment) => segment.label);
  return (
    <>
      <div
        role="img"
        aria-label={ariaLabel}
        data-testid={testId}
        className={`flex gap-0.5 overflow-hidden rounded-full bg-[var(--bg-3)] ${compact ? "h-2" : "h-2.5"}`}
      >
        {segments.map((segment) => (
          <i key={segment.key} className="block h-full" style={{ flex: `${segment.value} 1 0`, background: segment.color }} />
        ))}
      </div>
      {labelled.length > 0 ? (
        <ul className="m-0 flex list-none flex-wrap gap-x-3.5 gap-y-1 p-0">
          {labelled.map((segment) => (
            <li key={segment.key} className="flex items-center gap-1.5">
              <i aria-hidden className="block size-2 shrink-0 rounded-[2px]" style={{ background: segment.color }} />
              <Cap>{segment.label}</Cap>
            </li>
          ))}
        </ul>
      ) : null}
    </>
  );
}

export interface FilterChipOption<T extends string> {
  value: T;
  label: string;
  count?: number;
  disabled?: boolean;
}

/** Pill radiogroup filter bar (`--accent-soft` active, `--bg-3` idle, 44px touch targets); candidate for `KitPrimitives`. */
export function FilterChipGroup<T extends string>({
  value,
  onChange,
  options,
  ariaLabel,
}: {
  value: T;
  onChange: (next: T) => void;
  options: FilterChipOption<T>[];
  ariaLabel: string;
}) {
  const refs = useRef<Array<HTMLButtonElement | null>>([]);
  const enabled = options.map((opt, index) => ({ opt, index })).filter(({ opt }) => !opt.disabled);
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!["ArrowRight", "ArrowDown", "ArrowLeft", "ArrowUp", "Home", "End"].includes(event.key) || !enabled.length) return;
    event.preventDefault();
    const pos = Math.max(0, enabled.findIndex(({ opt }) => opt.value === value));
    const next = event.key === "Home"
      ? enabled[0]
      : event.key === "End"
        ? enabled[enabled.length - 1]
        : enabled[(pos + (event.key === "ArrowRight" || event.key === "ArrowDown" ? 1 : enabled.length - 1)) % enabled.length];
    refs.current[next.index]?.focus();
    onChange(next.opt.value);
  };
  return (
    <div role="radiogroup" aria-label={ariaLabel} onKeyDown={onKeyDown} className="flex flex-wrap gap-2">
      {options.map((option, index) => {
        const active = option.value === value;
        return (
          <button
            key={option.value}
            ref={(el) => { refs.current[index] = el; }}
            type="button"
            role="radio"
            aria-checked={active}
            disabled={option.disabled}
            tabIndex={active ? 0 : -1}
            onClick={() => onChange(option.value)}
            className="inline-flex min-h-11 min-w-11 cursor-pointer items-center justify-center gap-1.5 rounded-full px-3 text-xs leading-none font-medium whitespace-nowrap transition-colors sm:min-h-[30px] pointer-coarse:min-h-11 disabled:cursor-not-allowed disabled:opacity-40"
            style={
              active
                ? { background: "var(--accent-soft)", color: "var(--accent-fg)" }
                : { background: "var(--bg-3)", color: "var(--fg-dim)" }
            }
          >
            <span>{option.label}</span>
            {option.count != null ? <Num>{option.count}</Num> : null}
          </button>
        );
      })}
    </div>
  );
}

export interface AuditStat {
  value: number;
  label: string;
  warn?: boolean;
}

/** Detail head card: sticky top bar + summary section on phones, unified `KitCard` on desktop. */
export function AuditSummaryCard({ detail, label, meta, fetching, onRefresh, actions, stats, children }: {
  detail: AuditDetailProps;
  label: string;
  meta: string;
  fetching: boolean;
  onRefresh: () => void;
  actions?: ReactNode;
  stats: AuditStat[] | null;
  children?: ReactNode;
}) {
  const { agent, provider, providers, onProvider, onBack, phone, isKo, tr } = detail;
  const count = counter(isKo);
  const tone = (warn?: boolean) => (warn ? "text-[var(--warn)]" : "text-[var(--fg)]");
  const header = (
    <AuditDetailHeader
      agent={agent}
      isKo={isKo}
      meta={meta}
      onBack={onBack}
      phone={phone}
      tr={tr}
      actions={(
        <>
          <IconButton
            className={phone ? "min-h-11 min-w-11" : undefined}
            icon={<RefreshCw className={fetching ? "animate-spin motion-reduce:animate-none" : undefined} />}
            label={tr("새로 고침", "Refresh")}
            onClick={onRefresh}
            data-testid="audit-refresh"
          />
          {actions}
        </>
      )}
    />
  );
  const providerSwitch = providers.length > 1 ? (
    <div className="w-48" data-testid="audit-provider">
      <SegmentedControl<AuditProvider>
        variant="tray"
        size="sm"
        fullWidth
        value={provider}
        onChange={onProvider}
        aria-label={tr("provider 고르기", "Choose a provider")}
        options={providers.map((value) => ({ value, label: getProviderLabel(value) }))}
      />
    </div>
  ) : null;

  if (phone) {
    const hasBody = Boolean(providerSwitch || stats || children);
    return (
      <>
        {header}
        {hasBody ? (
          <section aria-label={label} className="flex min-w-0 flex-col gap-3">
            {providerSwitch}
            {stats ? (
              <div data-testid="audit-summary" className="grid grid-cols-3 gap-2">
                {stats.map((stat) => (
                  <Well key={stat.label} className="px-2.5 py-2">
                    <Num className={`block text-sm leading-5 font-semibold ${tone(stat.warn)}`}>{count(stat.value)}</Num>
                    <Cap className="block">{stat.label}</Cap>
                  </Well>
                ))}
              </div>
            ) : null}
            {children}
          </section>
        ) : null}
      </>
    );
  }

  return (
    <KitCard as="section" aria-label={label} className="px-4 py-3.5" style={{ gap: 10 }}>
      {header}
      {providerSwitch}
      {stats ? (
        <div data-testid="audit-summary" className="flex flex-wrap gap-x-5 gap-y-1 text-sm">
          {stats.map((stat) => (
            <span key={stat.label}>
              <Num className={`font-semibold ${tone(stat.warn)}`}>{count(stat.value)}</Num>{" "}
              <span className="text-[var(--fg-muted)]">{stat.label}</span>
            </span>
          ))}
        </div>
      ) : null}
      {children}
    </KitCard>
  );
}
