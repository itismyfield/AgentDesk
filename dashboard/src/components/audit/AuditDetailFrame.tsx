import { useQueries, type UseQueryOptions } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { useState, type ReactNode } from "react";
import type { AuditOrigin, AuditProvider } from "../../api/promptAudit";
import { getProviderLabel } from "../../app/providerTheme";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import type { Agent } from "../../types";
import { copyText } from "../agent-manager/TurnTranscriptModel";
import { formatTranscriptTimestamp } from "../agent-manager/turn-transcript-utils";
import { SETTINGS_TRAY_CLASS } from "../settings/SettingsPrimitives";
import { auditProviders, defaultAuditProvider } from "./auditModel";
import { AUDIT_CARD_CLASS, AuditDetailHeader } from "./AuditWorkspace";

type Tr = (ko: string, en: string) => string;

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

/** Raw prompt or anchor text: line breaks kept, long lines wrapped instead of widening the card. */
export const AUDIT_TEXT_CLASS = `m-0 overflow-x-hidden p-3 font-mono text-xs leading-5 whitespace-pre-wrap text-th-text-secondary [overflow-wrap:anywhere] ${SETTINGS_TRAY_CLASS}`;

export const counter = (isKo: boolean) => (value: number) => value.toLocaleString(isKo ? "ko-KR" : "en-US");

/** List captions from reads already cached in this visit; the observers are disabled, so the list never fetches. */
export function useCachedCaptions<T, K extends readonly unknown[]>(
  list: Agent[],
  read: (agentId: string, provider: AuditProvider) => UseQueryOptions<T, Error, T, K>,
  describe: (data: T) => string,
): (agent: Agent) => string {
  const reads = useQueries({ queries: list.map((agent) => ({ ...read(agent.id, defaultAuditProvider(agent) ?? "claude"), enabled: false })) });
  return (agent) => {
    const data = reads[list.indexOf(agent)]?.data as T | undefined;
    return data ? describe(data) : auditProviders(agent).map(getProviderLabel).join(" · ");
  };
}

/** Provider, session start (KST) and where the copy came from; `saved` names the copy when no session is live. */
export function sessionMeta(provider: AuditProvider, startedAt: string | null | undefined, origin: AuditOrigin | undefined, saved: string, isKo: boolean, tr: Tr): string {
  const started = startedAt ? formatTranscriptTimestamp(startedAt, isKo) : null;
  return [
    getProviderLabel(provider),
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

export interface AuditStat {
  value: number;
  label: string;
  warn?: boolean;
}

/** The detail's head card: agent, refresh and actions, the provider choice when both are bound, and the totals. */
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
  const tone = (warn?: boolean) => (warn ? "text-[var(--warn)]" : "text-th-text-primary");
  return (
    <section aria-label={label} className={phone ? "flex min-w-0 flex-col gap-3" : `${AUDIT_CARD_CLASS} flex flex-col gap-3 p-4`}>
      <AuditDetailHeader
        agent={agent}
        isKo={isKo}
        meta={meta}
        onBack={onBack}
        tr={tr}
        actions={(
          <>
            <IconButton
              icon={<RefreshCw className={fetching ? "animate-spin motion-reduce:animate-none" : undefined} />}
              label={tr("새로 고침", "Refresh")}
              onClick={onRefresh}
              data-testid="audit-refresh"
            />
            {actions}
          </>
        )}
      />
      {providers.length > 1 ? (
        <div className={phone ? "w-full" : "w-48"} data-testid="audit-provider">
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
      ) : null}
      {stats ? (
        <div data-testid="audit-summary" className={phone ? "grid grid-cols-3 gap-2" : "flex flex-wrap gap-x-5 gap-y-1 text-sm"}>
          {stats.map((stat) => (phone ? (
            <div key={stat.label} className={`${SETTINGS_TRAY_CLASS} px-2.5 py-2`}>
              <div className={`text-base font-semibold tabular-nums ${tone(stat.warn)}`}>{count(stat.value)}</div>
              <div className="text-xs text-th-text-muted">{stat.label}</div>
            </div>
          ) : (
            <span key={stat.label}><b className={`font-semibold tabular-nums ${tone(stat.warn)}`}>{count(stat.value)}</b> <span className="text-th-text-muted">{stat.label}</span></span>
          )))}
        </div>
      ) : null}
      {children}
    </section>
  );
}
