import { queryOptions, useQueries, useQuery } from "@tanstack/react-query";
import { Check, ChevronDown, Copy, RefreshCw } from "lucide-react";
import { useId, useMemo, useState } from "react";
import { getPromptAudit, type AuditProvider, type PromptSource } from "../../api/promptAudit";
import { getProviderLabel } from "../../app/providerTheme";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import type { Agent } from "../../types";
import { copyText } from "../agent-manager/TurnTranscriptModel";
import { formatTranscriptTimestamp } from "../agent-manager/turn-transcript-utils";
import { SETTINGS_TRAY_CLASS } from "../settings/SettingsPrimitives";
import { auditProviders, auditState, defaultAuditProvider, fullPromptText, sectionLabels, sourceLabel, sourceShares } from "./auditModel";
import { AUDIT_CARD_CLASS, AuditDetailHeader, AuditRequestState } from "./AuditWorkspace";

type Tr = (ko: string, en: string) => string;

const PHONE_SECTION_LIMIT = 5;
const SOURCE_COLOR: Record<PromptSource, string> = {
  canon_file: "bg-[var(--accent)]",
  memento: "bg-[var(--info)]",
  runtime: "bg-[var(--fg-faint)]",
  conditional: "bg-[var(--warn)]",
};

// Keyed by agent and provider, so a late answer lands in its own slot and never replaces the open selection.
const promptAuditQuery = (agentId: string, provider: AuditProvider) => queryOptions({
  queryKey: ["audit", "prompt", agentId, provider],
  queryFn: ({ signal }) => getPromptAudit(agentId, provider, signal),
  retry: false,
});

const counter = (isKo: boolean) => (value: number) => value.toLocaleString(isKo ? "ko-KR" : "en-US");

/** List captions from prompts already read in this visit; the observers are disabled, so the list never fetches. */
export function usePromptCaptions(list: Agent[], isKo: boolean, tr: Tr): (agent: Agent) => string {
  const reads = useQueries({
    queries: list.map((agent) => ({ ...promptAuditQuery(agent.id, defaultAuditProvider(agent) ?? "claude"), enabled: false })),
  });
  const count = counter(isKo);
  return (agent) => {
    const data = reads[list.indexOf(agent)]?.data;
    if (!data) return auditProviders(agent).map(getProviderLabel).join(" · ");
    if (data.origin === "unavailable") return tr("세션 없음", "No session");
    const total = data.sections.reduce((sum, section) => sum + section.chars, 0);
    const chars = tr(`${count(total)}자`, `${count(total)} chars`);
    return data.origin === "latest_saved" ? `${chars} · ${tr("최근 저장본", "latest saved")}` : chars;
  };
}

interface PromptAuditDetailProps {
  agent: Agent;
  provider: AuditProvider;
  providers: AuditProvider[];
  onProvider: (provider: AuditProvider) => void;
  onBack?: () => void;
  phone: boolean;
  isKo: boolean;
  tr: Tr;
}

/** One agent's system prompt: head, totals, share by source and the sections in prompt order. */
export function PromptAuditDetail({ agent, provider, providers, onProvider, onBack, phone, isKo, tr }: PromptAuditDetailProps) {
  const query = useQuery(promptAuditQuery(agent.id, provider));
  const state = auditState(query.error, query.data?.origin);
  const data = state === "ready" ? query.data : undefined;
  const sections = useMemo(() => data?.sections ?? [], [data]);
  const labels = useMemo(() => sectionLabels(sections, tr), [sections, tr]);
  const [open, setOpen] = useState<ReadonlySet<number>>(() => new Set());
  const [showAll, setShowAll] = useState(false);
  const [copied, setCopied] = useState<"idle" | "done" | "failed">("idle");
  const listId = useId();
  const count = counter(isKo);
  const chars = (value: number) => tr(`${count(value)}자`, `${count(value)} chars`);

  const started = data?.session_started_at ? formatTranscriptTimestamp(data.session_started_at, isKo) : null;
  const meta = [
    getProviderLabel(provider),
    started ? tr(`${started} 시작`, `started ${started}`) : null,
    data ? (data.origin === "current_session" ? tr("현재 세션", "Current session") : tr("세션 없음 · 최근 저장본", "No session · latest saved copy")) : null,
  ].filter(Boolean).join(" · ");

  const copyAll = () => {
    void copyText(fullPromptText(sections)).then(() => "done" as const, () => "failed" as const).then((result) => {
      setCopied(result);
      window.setTimeout(() => setCopied("idle"), 2000);
    });
  };
  const copyLabel = { idle: tr("전문 복사", "Copy all"), done: tr("복사했어요", "Copied"), failed: tr("복사하지 못했어요", "Could not copy") }[copied];
  const copyIcon = copied === "done" ? <Check aria-hidden /> : <Copy aria-hidden />;
  const retry = () => void query.refetch();

  const total = sections.reduce((sum, section) => sum + section.chars, 0);
  const shares = sourceShares(sections).filter((share) => share.chars > 0);
  const stats: Array<[number, string]> = data ? [[total, tr("자", "chars")], [sections.length, tr("섹션", "sections")], [data.masked_count, tr("가림", "masked")]] : [];
  const visible = phone && !showAll ? sections.slice(0, PHONE_SECTION_LIMIT) : sections;
  const toggle = (index: number) => setOpen((current) => {
    const next = new Set(current);
    if (!next.delete(index)) next.add(index);
    return next;
  });

  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid="audit-prompt-detail" data-state={state}>
      <section aria-label={tr("프롬프트 요약", "Prompt summary")} className={phone ? "flex min-w-0 flex-col gap-3" : `${AUDIT_CARD_CLASS} flex flex-col gap-3 p-4`}>
        <AuditDetailHeader
          agent={agent}
          isKo={isKo}
          meta={meta}
          onBack={onBack}
          tr={tr}
          actions={(
            <>
              <IconButton
                icon={<RefreshCw className={query.isFetching ? "animate-spin motion-reduce:animate-none" : undefined} />}
                label={tr("새로 고침", "Refresh")}
                onClick={retry}
                data-testid="audit-refresh"
              />
              {phone ? (
                <IconButton icon={copyIcon} label={copyLabel} disabled={!sections.length} onClick={copyAll} data-testid="audit-copy" />
              ) : (
                <Button variant="secondary" size="sm" icon={copyIcon} disabled={!sections.length} onClick={copyAll} data-testid="audit-copy">{copyLabel}</Button>
              )}
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
        {data ? (
          <>
            <div data-testid="audit-summary" className={phone ? "grid grid-cols-3 gap-2" : "flex flex-wrap gap-x-5 gap-y-1 text-sm"}>
              {stats.map(([value, label]) => (phone ? (
                <div key={label} className={`${SETTINGS_TRAY_CLASS} px-2.5 py-2`}>
                  <div className="text-base font-semibold tabular-nums text-th-text-primary">{count(value)}</div>
                  <div className="text-xs text-th-text-muted">{label}</div>
                </div>
              ) : (
                <span key={label}><b className="font-semibold tabular-nums text-th-text-primary">{count(value)}</b> <span className="text-th-text-muted">{label}</span></span>
              )))}
            </div>
            <div
              role="img"
              aria-label={`${tr("출처별 비율", "Share by source")}: ${shares.map((share) => `${sourceLabel(share.source, tr)} ${share.percent}%`).join(", ")}`}
              className="flex h-2.5 gap-0.5 overflow-hidden rounded-full"
              data-testid="audit-share-bar"
            >
              {shares.map((share) => <span key={share.source} className={SOURCE_COLOR[share.source]} style={{ flex: `${share.chars} 1 0` }} />)}
            </div>
            <ul className="m-0 flex list-none flex-wrap gap-x-3.5 gap-y-1 p-0 text-xs text-th-text-muted">
              {shares.map((share) => (
                <li key={share.source} className="flex items-center gap-1.5">
                  <span aria-hidden className={`size-2 rounded-[2px] ${SOURCE_COLOR[share.source]}`} />
                  {sourceLabel(share.source, tr)} {chars(share.chars)}
                </li>
              ))}
            </ul>
          </>
        ) : null}
      </section>

      {state !== "ready" ? (
        <AuditRequestState state={state} retrying={query.isFetching} onRetry={retry} tr={tr} />
      ) : (
        <section aria-label={tr("섹션", "Sections")} className={`${AUDIT_CARD_CLASS} overflow-hidden [&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]`} data-testid="audit-sections">
          {visible.map((section, index) => {
            const expanded = open.has(index);
            return (
              <div key={index} data-testid="audit-section">
                <button
                  type="button"
                  aria-expanded={expanded}
                  aria-controls={expanded ? `${listId}-${index}` : undefined}
                  onClick={() => toggle(index)}
                  className="flex min-h-14 w-full min-w-0 items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-subtle"
                >
                  <span aria-hidden className={`shrink-0 ${phone ? "size-2 rounded-full" : "h-7 w-1 rounded-sm"} ${SOURCE_COLOR[section.source]}`} />
                  <span className="min-w-0 flex-1">
                    <span className="flex flex-wrap items-center gap-2 text-sm font-medium break-keep text-th-text-primary">
                      {labels[index]}
                      {section.masked_count ? <Badge tone="neutral">{tr(`가림 ${section.masked_count}`, `${section.masked_count} masked`)}</Badge> : null}
                    </span>
                    <span className="block text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">
                      {sourceLabel(section.source, tr)}
                      {section.path ? <> · <span className="font-mono">{section.path}</span></> : null}
                    </span>
                  </span>
                  <span className="shrink-0 text-xs tabular-nums text-th-text-muted">{chars(section.chars)}</span>
                  <ChevronDown size={16} aria-hidden className={`shrink-0 text-th-text-muted transition-transform motion-reduce:transition-none ${expanded ? "rotate-180" : ""}`} />
                </button>
                {expanded ? (
                  <pre id={`${listId}-${index}`} className={`m-0 mx-4 mb-3 overflow-x-hidden p-3 font-mono text-xs leading-5 whitespace-pre-wrap text-th-text-secondary [overflow-wrap:anywhere] ${SETTINGS_TRAY_CLASS}`}>
                    {section.text.replace(/^\n+|\n+$/g, "")}
                  </pre>
                ) : null}
              </div>
            );
          })}
          {visible.length < sections.length ? (
            <div className="flex justify-center p-2">
              <Button variant="ghost" onClick={() => setShowAll(true)}>{tr(`섹션 ${sections.length - visible.length}개 더 보기`, `Show ${sections.length - visible.length} more sections`)}</Button>
            </div>
          ) : null}
        </section>
      )}
    </div>
  );
}
