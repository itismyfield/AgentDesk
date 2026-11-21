import { queryOptions, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, ChevronDown, Copy } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { getAgentAnchors, type AgentAnchor, type AgentAnchors, type AuditProvider } from "../../api/promptAudit";
import { localeName } from "../../i18n";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import type { Agent } from "../../types";
import AgentPersona from "../AgentPersona";
import { formatTranscriptTimestamp } from "../agent-manager/turn-transcript-utils";
import { SettingsNotice } from "../settings/SettingsPrimitives";
import {
  ANCHOR_FILTERS, anchorFilterCounts, anchorFilterLabel, anchorGroups, anchorLimitState, anchorMatches, anchorsToCheck, anchorTitle,
  overlapsAnchor, overlapsCanon, type AnchorFilter,
} from "./anchorModel";
import { auditState } from "./auditModel";
import { AUDIT_TEXT_CLASS, AuditSummaryCard, counter, sessionMeta, useCachedCaptions, useCopyFeedback, type AuditDetailProps } from "./AuditDetailFrame";
import { AUDIT_CARD_CLASS, AuditRequestState } from "./AuditWorkspace";

type Tr = (ko: string, en: string) => string;

const PHONE_ANCHOR_LIMIT = 5;

const anchorsQuery = (agentId: string, provider: AuditProvider) => queryOptions({
  queryKey: ["audit", "anchors", agentId, provider],
  queryFn: ({ signal }) => getAgentAnchors(agentId, provider, signal),
  retry: false,
});

export function useAnchorCaptions(list: Agent[], tr: Tr): (agent: Agent) => string {
  return useCachedCaptions(list, anchorsQuery, (data) => {
    if (data.source === "unavailable") return tr("세션 없음", "No session");
    const check = anchorsToCheck(data.anchors);
    const anchors = tr(`앵커 ${data.anchors.length}개`, `${data.anchors.length} anchors`);
    return check ? `${anchors} · ${tr(`확인할 것 ${check}`, `${check} to check`)}` : anchors;
  });
}

function MementoNotice({ memento, copied, isKo, tr }: { memento: AgentAnchors["memento"]; copied: boolean; isKo: boolean; tr: Tr }) {
  if (memento.status === "connected") return null;
  const title = {
    disconnected: tr("Memento에 연결할 수 없어요", "Memento is unreachable"),
    unknown: tr("Memento 연결을 아직 확인하지 못했어요", "Memento has not been checked yet"),
    unconfigured: tr("Memento가 설정되지 않았어요", "Memento is not configured"),
  }[memento.status];
  const checked = memento.checked_at ? formatTranscriptTimestamp(memento.checked_at, isKo) : null;
  return (
    <SettingsNotice tone="warn" role="status" icon={<AlertTriangle size={15} />} testId="audit-memento-notice">
      <b className="font-semibold text-th-text-primary">{title}</b>
      <div>
        {checked ? tr(`${checked}에 마지막으로 확인했어요. `, `Last checked ${checked}. `) : null}
        {copied ? tr("아래 앵커는 세션을 시작할 때 저장한 사본이에요.", "The anchors below are the copy saved when the session started.") : null}
      </div>
    </SettingsNotice>
  );
}

/** One agent's Memento anchors: totals, filters, rows grouped by scope, and each anchor's text, size, overlaps and readers. */
export function AnchorAuditDetail({ agents, ...detail }: AuditDetailProps & { agents: Agent[] }) {
  const { agent, provider, phone, isKo, tr } = detail;
  const query = useQuery(anchorsQuery(agent.id, provider));
  const state = auditState(query.error, query.data?.source);
  const read = query.error ? undefined : query.data;
  const anchors = useMemo(() => (state === "ready" ? read?.anchors ?? [] : []), [read, state]);
  const [filter, setFilter] = useState<AnchorFilter>("all");
  const [open, setOpen] = useState<ReadonlySet<number>>(() => new Set());
  const [showAll, setShowAll] = useState(false);
  const [jump, setJump] = useState<{ index: number } | null>(null);
  const rows = useRef(new Map<number, HTMLButtonElement>());
  const copied = useCopyFeedback<number>(tr);
  const listId = useId();
  const count = counter(isKo);
  const chars = (value: number) => tr(`${count(value)}자`, `${count(value)} chars`);
  const retry = () => void query.refetch();

  // An overlap link lands on its target once the list has re-rendered with it open.
  useEffect(() => {
    const row = jump ? rows.current.get(jump.index) : undefined;
    row?.scrollIntoView({ block: "center" });
    row?.focus({ preventScroll: true });
  }, [jump]);

  const counts = anchorFilterCounts(anchors);
  const check = anchorsToCheck(anchors);
  const matching = anchors.map((anchor, index) => ({ anchor, index })).filter(({ anchor }) => anchorMatches(anchor, filter));
  const visible = phone && !showAll ? matching.slice(0, PHONE_ANCHOR_LIMIT) : matching;
  const grouped = anchors.some((anchor) => anchor.scope);
  const toggle = (index: number) => setOpen((current) => {
    const next = new Set(current);
    if (!next.delete(index)) next.add(index);
    return next;
  });
  const goTo = (index: number) => {
    if (!anchorMatches(anchors[index], filter)) setFilter("all");
    setShowAll(true);
    setOpen((current) => new Set(current).add(index));
    setJump({ index });
  };
  const reader = (id: string) => agents.find((item) => item.id === id) ?? { id, name: id };

  const flags = (anchor: AgentAnchor) => {
    const limit = anchorLimitState(anchor);
    return [
      limit === "over" ? <Badge key="limit" tone="danger">{tr("한도 초과", "Over limit")}</Badge> : null,
      limit === "near" ? <Badge key="limit" tone="warning">{tr("한도 근접", "Near limit")}</Badge> : null,
      overlapsCanon(anchor) ? <Badge key="canon" tone="info">{tr("정본과 겹침", "Overlaps canon")}</Badge> : null,
      overlapsAnchor(anchor) ? <Badge key="mutual" tone="warning">{tr("서로 겹침", "Overlaps another")}</Badge> : null,
      anchor.masked_count ? <Badge key="masked">{tr(`가림 ${anchor.masked_count}`, `${anchor.masked_count} masked`)}</Badge> : null,
    ].filter(Boolean);
  };

  const body = (anchor: AgentAnchor, index: number) => (
    <div id={`${listId}-${index}`} className="mx-4 mb-3 flex min-w-0 flex-col gap-2.5 text-xs text-th-text-muted">
      <pre className={AUDIT_TEXT_CLASS}>{anchor.text}</pre>
      {anchor.limit != null ? (
        <div className="h-1.5 overflow-hidden rounded-full bg-[var(--bg-3)]" aria-hidden>
          <div className={`h-full ${anchorLimitState(anchor) ? "bg-[var(--warn)]" : "bg-[var(--accent)]"}`} style={{ width: `${Math.min(100, (anchor.chars / Math.max(anchor.limit, 1)) * 100)}%` }} />
        </div>
      ) : null}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <span className="tabular-nums" data-testid="audit-anchor-size">
          {anchor.limit != null ? tr(`${count(anchor.chars)} / ${count(anchor.limit)}자`, `${count(anchor.chars)} / ${count(anchor.limit)} chars`) : chars(anchor.chars)}
        </span>
        {anchor.scope ? <Badge>{tr(`범위 ${anchor.scope}`, `Scope ${anchor.scope}`)}</Badge> : null}
        {anchor.id ? <span className="font-mono [overflow-wrap:anywhere]">{anchor.id}</span> : null}
      </div>
      {anchor.overlap_candidates.length ? (
        <div className="flex flex-col items-start gap-1" data-testid="audit-anchor-overlaps">
          <span className="font-medium">{tr("겹치는 곳", "Overlaps with")}</span>
          {anchor.overlap_candidates.map((item, n) => (item.source === "anchor" ? (
            anchors[item.index] ? (
              <Button key={n} variant="ghost" size="sm" className="max-w-full" onClick={() => goTo(item.index)}>
                {`${tr(`앵커 ${item.index + 1}`, `Anchor ${item.index + 1}`)} · ${anchorTitle(anchors[item.index].text)}`}
              </Button>
            ) : null
          ) : (
            <span key={n}>{tr("정본", "Canon")} <span className="font-mono [overflow-wrap:anywhere]">{item.path}</span></span>
          )))}
        </div>
      ) : null}
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-medium">{tr("받는 에이전트", "Received by")}</span>
        {anchor.receiving_agents.map((id) => (
          <span key={id} className="inline-flex items-center gap-1.5 text-th-text-secondary">
            <AgentPersona agent={reader(id)} size={20} />
            {localeName(isKo ? "ko" : "en", reader(id))}
          </span>
        ))}
      </div>
      <Button variant="secondary" size="sm" className="self-start" icon={copied.done(index) ? <Check aria-hidden /> : <Copy aria-hidden />} onClick={() => copied.copy(index, anchor.text)}>
        {copied.label(index, tr("원문 복사", "Copy text"))}
      </Button>
    </div>
  );

  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid="audit-anchor-detail" data-state={state}>
      <AuditSummaryCard
        detail={detail}
        label={tr("앵커 요약", "Anchor summary")}
        meta={sessionMeta(provider, read?.session_started_at, state === "ready" ? read?.source : undefined, tr("세션 사본", "session copy"), isKo, tr)}
        fetching={query.isFetching}
        onRefresh={retry}
        stats={state === "ready" ? [
          { value: anchors.length, label: tr("앵커", "anchors") },
          { value: anchors.reduce((sum, anchor) => sum + anchor.chars, 0), label: tr("자", "chars") },
          { value: check, label: tr("확인할 것", "to check"), warn: check > 0 },
        ] : null}
      />
      {read ? <MementoNotice memento={read.memento} copied={state === "ready"} isKo={isKo} tr={tr} /> : null}

      {state !== "ready" ? (
        <AuditRequestState state={state} retrying={query.isFetching} onRetry={retry} tr={tr} />
      ) : (
        <>
          <SegmentedControl<AnchorFilter>
            variant="buttons"
            size="sm"
            value={filter}
            onChange={(next) => { setFilter(next); setShowAll(false); }}
            aria-label={tr("앵커 거르기", "Filter anchors")}
            options={ANCHOR_FILTERS.map((value) => ({ value, label: anchorFilterLabel(value, tr), count: counts[value], disabled: value !== filter && value !== "all" && !counts[value] }))}
          />
          {anchorGroups(visible).map((group) => (
            <section
              key={group.scope ?? ""}
              aria-label={group.scope ?? tr("앵커", "Anchors")}
              className={`${AUDIT_CARD_CLASS} overflow-hidden [&>*+*]:border-t [&>*+*]:border-[var(--line-soft)]`}
              data-testid="audit-anchor-group"
            >
              {grouped ? (
                <h3 className="m-0 flex items-center justify-between gap-3 px-4 py-2.5 text-[13px] font-medium text-th-text-primary">
                  <span className="min-w-0 [overflow-wrap:anywhere]">{group.scope ?? tr("범위 없음", "No scope")}</span>
                  <span className="text-xs tabular-nums text-th-text-muted">{group.rows.length}</span>
                </h3>
              ) : null}
              {group.rows.map(({ anchor, index }) => {
                const expanded = open.has(index);
                const marks = flags(anchor);
                return (
                  <div key={index} data-testid="audit-anchor" data-index={index}>
                    <button
                      ref={(element) => { if (element) rows.current.set(index, element); else rows.current.delete(index); }}
                      type="button"
                      aria-expanded={expanded}
                      aria-controls={expanded ? `${listId}-${index}` : undefined}
                      onClick={() => toggle(index)}
                      className="flex min-h-14 w-full min-w-0 items-center gap-3 px-4 py-2.5 text-left hover:bg-surface-subtle"
                    >
                      <span className="min-w-0 flex-1">
                        <span className="flex min-w-0 items-center gap-2">
                          {anchor.type ? <Badge>{anchor.type}</Badge> : null}
                          <span className="truncate text-sm font-medium text-th-text-primary">{anchorTitle(anchor.text)}</span>
                        </span>
                        {marks.length ? <span className="mt-1 flex flex-wrap gap-1">{marks}</span> : null}
                      </span>
                      <span className={`shrink-0 text-xs tabular-nums ${anchorLimitState(anchor) ? "text-[var(--warn)]" : "text-th-text-muted"}`}>{chars(anchor.chars)}</span>
                      <ChevronDown size={16} aria-hidden className={`shrink-0 text-th-text-muted transition-transform motion-reduce:transition-none ${expanded ? "rotate-180" : ""}`} />
                    </button>
                    {expanded ? body(anchor, index) : null}
                  </div>
                );
              })}
            </section>
          ))}
          {matching.length === 0 ? <p className="m-0 px-1 text-[13px] text-th-text-muted">{tr("맞는 앵커가 없어요.", "No matching anchors.")}</p> : null}
          {visible.length < matching.length ? (
            <div className="flex justify-center">
              <Button variant="ghost" onClick={() => setShowAll(true)}>{tr(`${matching.length - visible.length}개 더 보기`, `Show ${matching.length - visible.length} more`)}</Button>
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}
