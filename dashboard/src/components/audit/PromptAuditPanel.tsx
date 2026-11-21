import { queryOptions, useQuery } from "@tanstack/react-query";
import { Check, ChevronDown, Copy } from "lucide-react";
import { useId, useMemo, useState } from "react";
import { getPromptAudit, type AuditProvider, type PromptSource } from "../../api/promptAudit";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import type { Agent } from "../../types";
import { auditState, fullPromptText, sectionLabels, sourceLabel, sourceShares } from "./auditModel";
import { AUDIT_TEXT_CLASS, AuditSummaryCard, counter, sessionMeta, useCachedCaptions, useCopyFeedback, type AuditDetailProps } from "./AuditDetailFrame";
import { AUDIT_CARD_CLASS, AuditRequestState } from "./AuditWorkspace";

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

export function usePromptCaptions(list: Agent[], isKo: boolean, tr: Tr): (agent: Agent) => string {
  const count = counter(isKo);
  return useCachedCaptions(list, promptAuditQuery, (data) => {
    if (data.origin === "unavailable") return tr("세션 없음", "No session");
    const total = data.sections.reduce((sum, section) => sum + section.chars, 0);
    const chars = tr(`${count(total)}자`, `${count(total)} chars`);
    return data.origin === "latest_saved" ? `${chars} · ${tr("최근 저장본", "latest saved")}` : chars;
  });
}

/** One agent's system prompt: head, totals, share by source and the sections in prompt order. */
export function PromptAuditDetail({ onOpenAnchors, ...detail }: AuditDetailProps & { onOpenAnchors: () => void }) {
  const { agent, provider, phone, isKo, tr } = detail;
  const query = useQuery(promptAuditQuery(agent.id, provider));
  const state = auditState(query.error, query.data?.origin);
  const data = state === "ready" ? query.data : undefined;
  const sections = useMemo(() => data?.sections ?? [], [data]);
  const labels = useMemo(() => sectionLabels(sections, tr), [sections, tr]);
  const [open, setOpen] = useState<ReadonlySet<number>>(() => new Set());
  const [showAll, setShowAll] = useState(false);
  const copied = useCopyFeedback<"all">(tr);
  const listId = useId();
  const count = counter(isKo);
  const chars = (value: number) => tr(`${count(value)}자`, `${count(value)} chars`);

  const copyLabel = copied.label("all", tr("전문 복사", "Copy all"));
  const copyIcon = copied.done("all") ? <Check aria-hidden /> : <Copy aria-hidden />;
  const copyAll = () => copied.copy("all", fullPromptText(sections));
  const retry = () => void query.refetch();

  const total = sections.reduce((sum, section) => sum + section.chars, 0);
  const shares = sourceShares(sections).filter((share) => share.chars > 0);
  const visible = phone && !showAll ? sections.slice(0, PHONE_SECTION_LIMIT) : sections;
  const toggle = (index: number) => setOpen((current) => {
    const next = new Set(current);
    if (!next.delete(index)) next.add(index);
    return next;
  });

  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid="audit-prompt-detail" data-state={state}>
      <AuditSummaryCard
        detail={detail}
        label={tr("프롬프트 요약", "Prompt summary")}
        meta={sessionMeta(detail, data?.session_started_at, data?.origin, tr("최근 저장본", "latest saved copy"))}
        fetching={query.isFetching}
        onRefresh={retry}
        actions={phone ? (
          <IconButton icon={copyIcon} label={copyLabel} disabled={!sections.length} onClick={copyAll} data-testid="audit-copy" />
        ) : (
          <Button variant="secondary" size="sm" icon={copyIcon} disabled={!sections.length} onClick={copyAll} data-testid="audit-copy">{copyLabel}</Button>
        )}
        stats={data ? [
          { value: total, label: tr("자", "chars") },
          { value: sections.length, label: tr("섹션", "sections") },
          { value: data.masked_count, label: tr("가림", "masked") },
        ] : null}
      >
        {data ? (
          <>
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
      </AuditSummaryCard>

      {state !== "ready" ? (
        <AuditRequestState state={state} retrying={query.isFetching} onRetry={retry} tr={tr} />
      ) : (
        <section aria-label={tr("섹션", "Sections")} className={`${AUDIT_CARD_CLASS} overflow-hidden [&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]`} data-testid="audit-sections">
          {visible.map((section, index) => {
            const expanded = open.has(index);
            return (
              <div key={index} data-testid="audit-section">
                <div className="flex min-w-0 items-center">
                  <button
                    type="button"
                    aria-expanded={expanded}
                    aria-controls={expanded ? `${listId}-${index}` : undefined}
                    onClick={() => toggle(index)}
                    className="flex min-h-14 min-w-0 flex-1 items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-subtle"
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
                  {/* A sibling of the row button: the same agent and provider open on the anchor tab. */}
                  {section.source === "memento" ? (
                    <Button variant="ghost" size="sm" className="mr-2 shrink-0" onClick={onOpenAnchors} data-testid="audit-open-anchors">{tr("앵커 감사", "Audit anchors")}</Button>
                  ) : null}
                </div>
                {expanded ? (
                  <pre id={`${listId}-${index}`} className={`mx-4 mb-3 ${AUDIT_TEXT_CLASS}`}>{section.text.replace(/^\n+|\n+$/g, "")}</pre>
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
