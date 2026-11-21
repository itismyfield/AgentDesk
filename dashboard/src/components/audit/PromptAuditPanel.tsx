import { queryOptions, useQuery } from "@tanstack/react-query";
import { Check, ChevronRight, Copy } from "lucide-react";
import { useId, useMemo, useState } from "react";
import { getPromptAudit, type AuditOrigin, type AuditProvider, type PromptSection, type PromptSource } from "../../api/promptAudit";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import type { Agent } from "../../types";
import { Cap, Chip, Desc, Eyebrow, InspSection, KeyValue, KitCard, Row, RowButton, RuleItem, Well } from "../common/KitPrimitives";
import { auditState, fullPromptText, sectionLabels, sourceLabel, sourceShares } from "./auditModel";
import {
  AUDIT_PRE_CLASS, AuditSummaryCard, counter, FULL_TOUCH_BUTTON_STYLE, sessionMeta, ShareBar, useCachedCaptions, useCopyFeedback,
  type AuditDetailProps,
} from "./AuditDetailFrame";
import { AuditRequestState } from "./AuditWorkspace";

type Tr = (ko: string, en: string) => string;

const PHONE_SECTION_LIMIT = 5;
const SOURCE_CSS_COLOR: Record<PromptSource, string> = {
  canon_file: "var(--accent)",
  memento: "var(--info)",
  runtime: "var(--fg-faint)",
  conditional: "var(--warn)",
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

/** Right-hand desktop inspector summarizing source shares, canon logical paths and origin semantics. */
function PromptSourceInspector({ sections, labels, origin, maskedCount, isKo, tr }: {
  sections: PromptSection[];
  labels: string[];
  origin: AuditOrigin;
  maskedCount: number;
  isKo: boolean;
  tr: Tr;
}) {
  const count = counter(isKo);
  const chars = (value: number) => tr(`${count(value)}자`, `${count(value)} chars`);
  const shares = sourceShares(sections);
  const canonRows = sections.map((section, index) => ({ section, label: labels[index] })).filter(({ section }) => section.source === "canon_file");

  return (
    <aside aria-label={tr("출처 요약", "Source summary")} data-testid="audit-source-summary" className="w-full xl:w-[288px] xl:shrink-0">
      <KitCard>
        <div className="px-4 py-3.5 text-sm leading-5 font-semibold text-[var(--fg)]">{tr("출처 요약", "Source summary")}</div>
        <InspSection>
          {shares.map((share) => (
            <KeyValue
              key={share.source}
              leading={<i aria-hidden className="block size-2 shrink-0 rounded-[2px]" style={{ background: SOURCE_CSS_COLOR[share.source] }} />}
              label={sourceLabel(share.source, tr)}
              num
            >
              {chars(share.chars)} · {share.percent}%
            </KeyValue>
          ))}
        </InspSection>
        {canonRows.length > 0 ? (
          <InspSection>
            <Eyebrow>{tr("정본 파일", "Canon files")}</Eyebrow>
            {canonRows.map(({ section, label }, index) => (
              <RuleItem key={index} tone="info">
                <div className="font-medium text-[var(--fg)]">{label}</div>
                {section.path ? <Cap className="block font-mono [overflow-wrap:anywhere]">{section.path}</Cap> : null}
              </RuleItem>
            ))}
          </InspSection>
        ) : null}
        <InspSection>
          <Eyebrow>{tr("이 화면이 쓰는 값", "What this screen shows")}</Eyebrow>
          <RuleItem tone={origin === "current_session" ? "ok" : "neutral"}>
            <Desc className="text-[var(--fg-dim)]">
              {origin === "current_session"
                ? tr("지금 열려 있는 세션의 시스템 프롬프트예요.", "System prompt from the live session.")
                : tr("열린 세션이 없어 최근 저장본을 보여 줘요.", "No live session; showing the latest saved copy.")}
            </Desc>
          </RuleItem>
          {maskedCount > 0 ? (
            <RuleItem tone="warn">
              <Desc className="text-[var(--fg-dim)]">{tr(`민감한 값 ${count(maskedCount)}곳을 *** 로 가렸어요.`, `Masked ${count(maskedCount)} sensitive values with ***.`)}</Desc>
            </RuleItem>
          ) : null}
          <RuleItem tone="neutral">
            <Desc>{tr("세션과 저장본이 모두 없으면 재구성하지 않고 비워 둬요.", "Shown empty without rebuilding when no session or saved copy exists.")}</Desc>
          </RuleItem>
        </InspSection>
      </KitCard>
    </aside>
  );
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

  const mainContent = (
    <>
      <AuditSummaryCard
        detail={detail}
        label={tr("프롬프트 요약", "Prompt summary")}
        meta={sessionMeta(detail, data?.session_started_at, data?.origin, tr("최근 저장본", "latest saved copy"))}
        fetching={query.isFetching}
        onRefresh={retry}
        actions={phone ? (
          <IconButton className="min-h-11 min-w-11" icon={copyIcon} label={copyLabel} disabled={!sections.length} onClick={copyAll} data-testid="audit-copy" />
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
          <ShareBar
            compact={phone}
            testId="audit-share-bar"
            ariaLabel={`${tr("출처별 비율", "Share by source")}: ${shares.map((share) => `${sourceLabel(share.source, tr)} ${share.percent}%`).join(", ")}`}
            segments={shares.map((share) => ({
              key: share.source,
              value: share.chars,
              color: SOURCE_CSS_COLOR[share.source],
              label: `${sourceLabel(share.source, tr)} ${chars(share.chars)}`,
            }))}
          />
        ) : null}
      </AuditSummaryCard>

      {state !== "ready" ? (
        <AuditRequestState state={state} retrying={query.isFetching} onRetry={retry} tr={tr} />
      ) : (
        <KitCard as="section" aria-label={tr("섹션", "Sections")} data-testid="audit-sections">
          {visible.map((section, index) => {
            const expanded = open.has(index);
            return (
              <div key={index} data-testid="audit-section" className="border-t border-[var(--line-soft)] first:border-t-0">
                <div className="flex min-w-0 items-center">
                  <RowButton
                    aria-expanded={expanded}
                    aria-controls={expanded ? `${listId}-${index}` : undefined}
                    selected={expanded}
                    aria-current={undefined}
                    onClick={() => toggle(index)}
                    className="min-h-14 flex-1 py-2.5"
                  >
                    <i
                      aria-hidden
                      className={`block shrink-0 ${phone ? "size-2 rounded-full" : "h-7 w-1 rounded-[2px]"}`}
                      style={{ background: SOURCE_CSS_COLOR[section.source] }}
                    />
                    <span className="min-w-0 flex-1">
                      <span className="flex flex-wrap items-center gap-2 text-sm leading-5 font-medium break-keep text-[var(--fg)]">
                        {labels[index]}
                        {section.masked_count ? <Chip tone="neutral">{tr(`가림 ${section.masked_count}`, `${section.masked_count} masked`)}</Chip> : null}
                      </span>
                      <Cap className="mt-0.5 block leading-5 [overflow-wrap:anywhere]">
                        {sourceLabel(section.source, tr)}
                        {section.path ? <> · <span className="font-mono">{section.path}</span></> : null}
                      </Cap>
                    </span>
                    <Cap className="shrink-0 font-display tabular-nums">{chars(section.chars)}</Cap>
                    <ChevronRight
                      size={16}
                      aria-hidden
                      className={`shrink-0 text-[var(--fg-muted)] transition-transform motion-reduce:transition-none ${expanded ? "rotate-90" : ""}`}
                    />
                  </RowButton>
                  {/* A sibling of the row button: the same agent and provider open on the anchor tab. */}
                  {section.source === "memento" ? (
                    <button
                      type="button"
                      onClick={onOpenAnchors}
                      data-testid="audit-open-anchors"
                      className="mr-2 inline-flex min-h-11 min-w-11 shrink-0 cursor-pointer items-center justify-center rounded-[var(--r-sm)] px-2.5 text-xs leading-4 font-medium text-[var(--accent-fg)] hover:underline focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-[var(--accent)]"
                    >
                      {tr("앵커 감사", "Audit anchors")}
                    </button>
                  ) : null}
                </div>
                {expanded ? (
                  <Well className="mx-3 mb-3 p-3 sm:mx-4">
                    <pre id={`${listId}-${index}`} className={AUDIT_PRE_CLASS}>{section.text.replace(/^\n+|\n+$/g, "")}</pre>
                  </Well>
                ) : null}
              </div>
            );
          })}
          {visible.length < sections.length ? (
            <Row className="justify-center py-1.5">
              <Button variant="ghost" style={FULL_TOUCH_BUTTON_STYLE} onClick={() => setShowAll(true)}>
                {tr(`섹션 ${sections.length - visible.length}개 더 보기`, `Show ${sections.length - visible.length} more sections`)}
              </Button>
            </Row>
          ) : null}
        </KitCard>
      )}
    </>
  );

  if (phone) {
    return (
      <div className="flex min-w-0 flex-col gap-3" data-testid="audit-prompt-detail" data-state={state}>
        {mainContent}
      </div>
    );
  }

  return (
    <div className="flex min-w-0 flex-col gap-5 xl:flex-row xl:items-start" data-testid="audit-prompt-detail" data-state={state}>
      <div className="flex min-w-0 flex-1 flex-col gap-3">
        {mainContent}
      </div>
      {data ? (
        <PromptSourceInspector
          sections={sections}
          labels={labels}
          origin={data.origin}
          maskedCount={data.masked_count}
          isKo={isKo}
          tr={tr}
        />
      ) : null}
    </div>
  );
}
