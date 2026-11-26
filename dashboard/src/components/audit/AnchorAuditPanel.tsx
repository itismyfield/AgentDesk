import { queryOptions, useQuery } from "@tanstack/react-query";
import { AlertTriangle, Check, ChevronRight, Copy, Folder, Globe } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { getAgentAnchors, type AgentAnchor, type AgentAnchors, type AuditProvider } from "../../api/promptAudit";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import type { Agent } from "../../types";
import AgentPersona from "../AgentPersona";
import { formatTranscriptTimestamp } from "../agent-manager/turn-transcript-utils";
import {
  Cap, CardNote, Chip, Desc, Eyebrow, InspSection, KeyValue, KitCard, MeterBar, Num, Row, RowButton, RuleItem, StatusDot, Well,
  type DotTone,
} from "../common/KitPrimitives";
import {
  ANCHOR_FILTERS, anchorFilterCounts, anchorFilterLabel, anchorGroups, anchorLimitState, anchorMatches, anchorsToCheck, anchorTitle,
  overlapsAnchor, overlapsCanon, type AnchorFilter,
} from "./anchorModel";
import { auditState } from "./auditModel";
import {
  AuditSummaryCard, counter, FilterChipGroup, FULL_TOUCH_BUTTON_STYLE, sessionMeta, useCachedCaptions, useCachedReads,
  useCopyFeedback, type AuditDetailProps,
} from "./AuditDetailFrame";
import { AuditRequestState } from "./AuditWorkspace";

type Tr = (ko: string, en: string) => string;

const PHONE_ANCHOR_LIMIT = 5;
const AVATAR_STACK_LIMIT = 7;

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

export function useAnchorWarnings(list: Agent[]): (agent: Agent) => boolean {
  const cached = useCachedReads(list, anchorsQuery);
  return (agent) => {
    const data = cached(agent);
    return Boolean(data && data.source !== "unavailable" && anchorsToCheck(data.anchors) > 0);
  };
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
    <KitCard
      role="status"
      pad="sm"
      data-testid="audit-memento-notice"
      style={{
        borderColor: "color-mix(in srgb, var(--warn) 50%, var(--line))",
        background: "color-mix(in srgb, var(--warn) 8%, var(--bg-2))",
      }}
    >
      <div className="flex min-w-0 items-start gap-2.5">
        <AlertTriangle size={18} aria-hidden className="mt-0.5 shrink-0 text-[var(--warn)]" />
        <div className="min-w-0 flex-1">
          <b className="block text-sm leading-5 font-semibold text-[var(--fg)]">{title}</b>
          <Desc className="mt-0.5 block">
            {checked ? tr(`${checked}에 마지막으로 확인했어요. `, `Last checked ${checked}. `) : null}
            {copied ? tr("아래 앵커는 세션을 시작할 때 저장한 사본이에요.", "The anchors below are the copy saved when the session started.") : null}
          </Desc>
        </div>
      </div>
    </KitCard>
  );
}

function anchorFlags(anchor: AgentAnchor, tr: Tr) {
  const limit = anchorLimitState(anchor);
  return [
    limit === "over" ? <Chip key="limit" tone="err">{tr("한도 초과", "Over limit")}</Chip> : null,
    limit === "near" ? <Chip key="limit" tone="warn">{tr("한도 근접", "Near limit")}</Chip> : null,
    overlapsCanon(anchor) ? <Chip key="canon" tone="info">{tr("정본과 겹침", "Overlaps canon")}</Chip> : null,
    overlapsAnchor(anchor) ? <Chip key="mutual" tone="warn">{tr("서로 겹침", "Overlaps another")}</Chip> : null,
    anchor.masked_count ? <Chip key="masked" tone="neutral">{tr(`가림 ${anchor.masked_count}`, `${anchor.masked_count} masked`)}</Chip> : null,
  ].filter(Boolean);
}

/** Shared detail sections for one anchor: rendered inside the desktop right inspector or inline on phones. */
function AnchorDetailSections({
  anchor,
  index,
  anchors,
  agents,
  startedAt,
  phone,
  isKo,
  tr,
  copied,
  onJump,
}: {
  anchor: AgentAnchor;
  index: number;
  anchors: AgentAnchor[];
  agents: Agent[];
  startedAt: string | null | undefined;
  phone: boolean;
  isKo: boolean;
  tr: Tr;
  copied: ReturnType<typeof useCopyFeedback<number>>;
  onJump: (target: number) => void;
}) {
  const count = counter(isKo);
  const chars = (value: number) => tr(`${count(value)}자`, `${count(value)} chars`);
  const limit = anchorLimitState(anchor);
  const marks = anchorFlags(anchor, tr);
  const reader = (id: string) => agents.find((item) => item.id === id) ?? { id, name: id };
  const extraReaders = Math.max(0, anchor.receiving_agents.length - AVATAR_STACK_LIMIT);
  const trimmedText = anchor.text.trim();
  const heading = anchorTitle(anchor.text);

  return (
    <>
      <div className={`flex min-w-0 flex-col gap-2.5 px-3 py-3.5 sm:px-4 ${phone ? "border-t border-[var(--line-soft)]" : ""}`}>
        {(anchor.type || anchor.scope || marks.length > 0) ? (
          <div className="flex flex-wrap gap-1.5">
            {anchor.type ? <Chip tone="neutral">{anchor.type}</Chip> : null}
            {anchor.scope ? <Chip tone="accent">{anchor.scope === "모든 에이전트" ? tr("모든 에이전트", "All agents") : anchor.scope}</Chip> : null}
            {!phone ? marks : null}
          </div>
        ) : null}
        {!phone && trimmedText !== heading ? (
          <h3 className="m-0 text-base leading-6 font-semibold text-[var(--fg)]">{heading}</h3>
        ) : null}
        <Well
          data-testid="audit-anchor-text"
          className="max-h-80 overflow-y-auto px-3.5 py-3 pr-4 text-[13px] leading-relaxed whitespace-pre-wrap text-[var(--fg)] [overflow-wrap:anywhere]"
        >
          {trimmedText}
        </Well>
        <div className="flex items-center gap-2">
          {anchor.limit != null ? (
            <MeterBar
              ratio={anchor.chars / Math.max(anchor.limit, 1)}
              tone={limit === "over" ? "err" : limit === "near" ? "warn" : "muted"}
              className="flex-1"
            />
          ) : null}
          <Cap className="shrink-0 font-display tabular-nums" data-testid="audit-anchor-size">
            {anchor.limit != null
              ? tr(`${count(anchor.chars)} / ${count(anchor.limit)}자`, `${count(anchor.chars)} / ${count(anchor.limit)} chars`)
              : chars(anchor.chars)}
          </Cap>
        </div>
      </div>

      {anchor.overlap_candidates.length > 0 ? (
        <InspSection data-testid="audit-anchor-overlaps">
          <Eyebrow className={overlapsAnchor(anchor) ? "text-[var(--warn)]" : "text-[var(--info)]"}>
            {tr("겹치는 곳", "Overlaps with")}
          </Eyebrow>
          {anchor.overlap_candidates.map((item, n) => (item.source === "anchor" ? (
            anchors[item.index] ? (
              <div key={n} className="flex min-w-0 flex-col items-start gap-1.5">
                <Well className="w-full px-3 py-2 text-xs leading-[18px] whitespace-pre-wrap text-[var(--fg-dim)] line-clamp-3 [overflow-wrap:anywhere]">
                  {anchors[item.index].text.trim()}
                </Well>
                <Button variant="secondary" size="sm" className="max-w-full" onClick={() => onJump(item.index)}>
                  {`${tr(`앵커 ${item.index + 1}`, `Anchor ${item.index + 1}`)} · ${anchorTitle(anchors[item.index].text)}`}
                </Button>
              </div>
            ) : null
          ) : (
            <RuleItem key={n} tone="info">
              <span className="text-[var(--fg-muted)]">{tr("정본", "Canon")} </span>
              <span className="font-mono text-xs text-[var(--fg)] [overflow-wrap:anywhere]">{item.path}</span>
            </RuleItem>
          )))}
        </InspSection>
      ) : null}

      <InspSection>
        <Eyebrow>{tr("받는 에이전트", "Received by")} · <Num>{anchor.receiving_agents.length}</Num></Eyebrow>
        {anchor.receiving_agents.length <= 3 ? (
          <div className="flex flex-wrap items-center gap-2.5 text-xs text-[var(--fg-dim)]">
            {anchor.receiving_agents.map((id) => (
              <span key={id} className="inline-flex items-center gap-1.5">
                <AgentPersona agent={reader(id)} size={24} />
                <span>{localeName(isKo ? "ko" : "en", reader(id))}</span>
              </span>
            ))}
          </div>
        ) : (
          <div className="flex items-center">
            {anchor.receiving_agents.slice(0, AVATAR_STACK_LIMIT).map((id, idx) => (
              <span key={id} className={idx ? "-ml-1.5" : undefined} title={localeName(isKo ? "ko" : "en", reader(id))}>
                <AgentPersona agent={reader(id)} size={24} />
              </span>
            ))}
            {extraReaders > 0 ? <Cap className="ml-2">{tr(`외 ${extraReaders}`, `+${extraReaders}`)}</Cap> : null}
          </div>
        )}
      </InspSection>

      <InspSection>
        <KeyValue label={tr("범위", "Scope")}>{(anchor.scope === "모든 에이전트" ? tr("모든 에이전트", "All agents") : anchor.scope) ?? tr("범위 없음", "No scope")}</KeyValue>
        {anchor.id ? <KeyValue label="ID" valueClassName="font-mono">{anchor.id}</KeyValue> : null}
        {startedAt ? <KeyValue label={tr("세션 시작", "Session start")}>{formatTranscriptTimestamp(startedAt, isKo)}</KeyValue> : null}
        <div className="mt-1 flex flex-wrap gap-2">
          <Button
            variant="secondary"
            size="sm"
            icon={copied.done(index) ? <Check aria-hidden /> : <Copy aria-hidden />}
            onClick={() => copied.copy(index, anchor.text)}
          >
            {copied.label(index, tr("원문 복사", "Copy text"))}
          </Button>
        </div>
      </InspSection>
    </>
  );
}

/** One agent's Memento anchors: `Well` summary strip, filter chips, grouped `KitCard` rows and selected anchor inspector. */
export function AnchorAuditDetail({ agents, ...detail }: AuditDetailProps & { agents: Agent[] }) {
  const { agent, provider, phone, isKo, tr } = detail;
  const query = useQuery(anchorsQuery(agent.id, provider));
  const state = auditState(query.error, query.data?.source);
  const read = query.error ? undefined : query.data;
  const anchors = useMemo(() => (state === "ready" ? read?.anchors ?? [] : []), [read, state]);
  const [filter, setFilter] = useState<AnchorFilter>("all");
  const [selected, setSelected] = useState<number>(0);
  const [open, setOpen] = useState<ReadonlySet<number>>(() => new Set());
  const [showAll, setShowAll] = useState(false);
  const [jump, setJump] = useState<{ index: number } | null>(null);
  const rows = useRef(new Map<number, HTMLButtonElement>());
  const copied = useCopyFeedback<number>(tr);
  const listId = useId();
  const count = counter(isKo);
  const chars = (value: number) => tr(`${count(value)}자`, `${count(value)} chars`);
  const retry = () => void query.refetch();

  // An overlap link lands on its target once the list has re-rendered with it open or selected.
  useEffect(() => {
    const row = jump ? rows.current.get(jump.index) : undefined;
    row?.scrollIntoView({ block: "center" });
    row?.focus({ preventScroll: true });
  }, [jump]);

  const counts = anchorFilterCounts(anchors);
  const check = anchorsToCheck(anchors);
  const totalChars = anchors.reduce((sum, anchor) => sum + anchor.chars, 0);
  const matching = anchors.map((anchor, index) => ({ anchor, index })).filter(({ anchor }) => anchorMatches(anchor, filter));
  const visible = phone && !showAll ? matching.slice(0, PHONE_ANCHOR_LIMIT) : matching;
  const grouped = anchors.some((anchor) => anchor.scope);
  const activeIndex = matching.some((item) => item.index === selected) ? selected : (matching[0]?.index ?? null);
  const activeAnchor = activeIndex != null ? anchors[activeIndex] : undefined;

  const togglePhone = (index: number) => setOpen((current) => {
    const next = new Set(current);
    if (!next.delete(index)) next.add(index);
    return next;
  });
  const goTo = (index: number) => {
    if (!anchorMatches(anchors[index], filter)) setFilter("all");
    setShowAll(true);
    setSelected(index);
    if (phone) setOpen((current) => new Set(current).add(index));
    setJump({ index });
  };

  const mementoTone: DotTone = read?.memento.status === "connected" ? "ok" : read?.memento.status === "disconnected" ? "warn" : "neutral";
  const mementoStatus = read ? {
    connected: tr("Memento 연결됨", "Memento connected"),
    disconnected: tr("Memento 연결 끊김", "Memento unreachable"),
    unknown: tr("Memento 미확인", "Memento unchecked"),
    unconfigured: tr("Memento 미설정", "Memento unconfigured"),
  }[read.memento.status] : "";
  const checkedClock = read?.memento.checked_at ? formatTranscriptTimestamp(read.memento.checked_at, isKo) : null;

  return (
    <div className="flex min-w-0 flex-col gap-3" data-testid="audit-anchor-detail" data-state={state}>
      <AuditSummaryCard
        detail={detail}
        label={tr("앵커 요약", "Anchor summary")}
        meta={sessionMeta(
          detail,
          read?.session_started_at,
          state === "ready" ? read?.source : undefined,
          anchors.length > 0 ? tr("세션 사본", "session copy") : tr("저장된 앵커 없음", "no saved anchors"),
        )}
        fetching={query.isFetching}
        onRefresh={retry}
        stats={phone && state === "ready" ? [
          { value: anchors.length, label: tr("앵커", "anchors") },
          { value: totalChars, label: tr("자", "chars") },
          { value: check, label: tr("확인할 것", "to check"), warn: check > 0 },
        ] : null}
      />
      {!phone && state === "ready" ? (
        <Well data-testid="audit-summary" className="flex flex-wrap items-center gap-x-4 gap-y-1.5 px-4 py-2.5 text-sm">
          {read ? (
            <span className="flex items-center gap-2">
              <StatusDot tone={mementoTone} />
              <span>{mementoStatus}</span>
            </span>
          ) : null}
          <span><Num className="font-semibold">{count(anchors.length)}</Num> <span className="text-[var(--fg-muted)]">{tr("앵커", "anchors")}</span></span>
          <span><Num className="font-semibold">{count(totalChars)}</Num> <span className="text-[var(--fg-muted)]">{tr("자", "chars")}</span></span>
          <span className={check > 0 ? "text-[var(--warn)]" : undefined}>
            <Num className="font-semibold">{count(check)}</Num>{" "}
            <span className={check > 0 ? undefined : "text-[var(--fg-muted)]"}>{tr("확인할 것", "to check")}</span>
          </span>
          <Cap className="ml-auto">
            {checkedClock ? `${tr(`${checkedClock} 확인`, `Checked ${checkedClock}`)} · ` : ""}
            {tr("기억을 고치는 일은 이 화면에서 하지 않아요", "Memories are not edited on this screen")}
          </Cap>
        </Well>
      ) : null}
      {read ? <MementoNotice memento={read.memento} copied={state === "ready" && anchors.length > 0} isKo={isKo} tr={tr} /> : null}

      {state !== "ready" ? (
        <AuditRequestState state={state} retrying={query.isFetching} onRetry={retry} tr={tr} />
      ) : (
        <div className="flex min-w-0 flex-col gap-5 xl:flex-row xl:items-start">
          <div className="flex min-w-0 flex-1 flex-col gap-2.5">
            <FilterChipGroup<AnchorFilter>
              value={filter}
              onChange={(next) => { setFilter(next); setShowAll(false); }}
              ariaLabel={tr("앵커 거르기", "Filter anchors")}
              options={ANCHOR_FILTERS.map((value) => ({
                value,
                label: anchorFilterLabel(value, tr),
                count: counts[value],
                disabled: value !== filter && value !== "all" && !counts[value],
              }))}
            />
            {matching.length === 0 ? (
              <KitCard><CardNote className="border-t-0">{tr("맞는 앵커가 없어요.", "No matching anchors.")}</CardNote></KitCard>
            ) : (
              <KitCard>
                {anchorGroups(visible).map((group) => {
                  const isGlobalScope = group.scope === "모든 에이전트";
                  const scopeLabel = isGlobalScope ? tr("모든 에이전트", "All agents") : group.scope;
                  return (
                    <section
                      key={group.scope ?? ""}
                      aria-label={scopeLabel ?? tr("앵커", "Anchors")}
                      className="border-t border-[var(--line-soft)] first:border-t-0"
                      data-testid="audit-anchor-group"
                    >
                      {grouped ? (
                        <h3 className="m-0 flex items-center gap-2 bg-[var(--bg-1)] px-3.5 py-2.5 text-[13px] leading-5 font-semibold text-[var(--fg)]">
                          {group.scope ? (
                            isGlobalScope
                              ? <Globe size={15} aria-hidden className="shrink-0 text-[var(--accent-fg)]" />
                              : <Folder size={15} aria-hidden className="shrink-0 text-[var(--accent-fg)]" />
                          ) : null}
                          <span className="min-w-0 flex-1 [overflow-wrap:anywhere]">{scopeLabel ?? tr("범위 없음", "No scope")}</span>
                          <Cap className="font-display tabular-nums font-normal">{group.rows.length}</Cap>
                        </h3>
                      ) : null}
                      {group.rows.map(({ anchor, index }) => {
                        const expanded = open.has(index);
                        const isSelected = phone ? expanded : activeIndex === index;
                        const marks = anchorFlags(anchor, tr);
                        const limit = anchorLimitState(anchor);
                        return (
                          <div
                            key={index}
                            ref={(el) => {
                              const btn = el?.querySelector("button");
                              if (btn) rows.current.set(index, btn);
                              else rows.current.delete(index);
                            }}
                            data-testid="audit-anchor"
                            data-index={index}
                            className={`border-t border-[var(--line-soft)] ${!grouped ? "first:border-t-0" : ""}`}
                          >
                            <RowButton
                              selected={isSelected}
                              aria-current={!phone && isSelected ? "true" : undefined}
                              aria-expanded={phone ? expanded : undefined}
                              aria-controls={phone && expanded ? `${listId}-${index}` : undefined}
                              onClick={() => (phone ? togglePhone(index) : setSelected(index))}
                              className={phone ? "min-h-14 py-3" : "min-h-11 px-3.5 py-2.5 sm:px-3.5"}
                            >
                              {anchor.type ? <Chip tone="neutral" className="min-w-10 justify-center">{anchor.type}</Chip> : null}
                              {phone ? (
                                <span className="min-w-0 flex-1">
                                  <span className="block truncate text-sm leading-5 font-medium text-[var(--fg)]">{anchorTitle(anchor.text)}</span>
                                  {marks.length > 0 ? (
                                    <span className="mt-1 flex flex-wrap gap-1">{marks}</span>
                                  ) : !grouped && anchor.scope ? (
                                    <Cap className="mt-0.5 block truncate">{anchor.scope === "모든 에이전트" ? tr("모든 에이전트", "All agents") : anchor.scope}</Cap>
                                  ) : null}
                                </span>
                              ) : (
                                <>
                                  <span className="min-w-[8rem] flex-1 truncate text-sm leading-5 font-medium text-[var(--fg)]">{anchorTitle(anchor.text)}</span>
                                  {marks.length > 0 ? <span className="flex shrink-0 flex-wrap justify-end gap-1">{marks}</span> : null}
                                </>
                              )}
                              <Cap
                                className="shrink-0 font-display tabular-nums sm:min-w-14 sm:text-right"
                                style={limit === "over" ? { color: "var(--err)" } : limit === "near" ? { color: "var(--warn)" } : undefined}
                              >
                                {chars(anchor.chars)}
                              </Cap>
                              {phone ? (
                                <ChevronRight
                                  size={16}
                                  aria-hidden
                                  className={`shrink-0 text-[var(--fg-muted)] transition-transform motion-reduce:transition-none ${expanded ? "rotate-90" : ""}`}
                                />
                              ) : null}
                            </RowButton>
                            {phone && expanded ? (
                              <div id={`${listId}-${index}`}>
                                <AnchorDetailSections
                                  anchor={anchor}
                                  index={index}
                                  anchors={anchors}
                                  agents={agents}
                                  startedAt={read?.session_started_at}
                                  phone
                                  isKo={isKo}
                                  tr={tr}
                                  copied={copied}
                                  onJump={goTo}
                                />
                              </div>
                            ) : null}
                          </div>
                        );
                      })}
                    </section>
                  );
                })}
                {visible.length < matching.length ? (
                  <Row className="justify-center py-1.5">
                    <Button variant="ghost" style={FULL_TOUCH_BUTTON_STYLE} onClick={() => setShowAll(true)}>
                      {tr(`${matching.length - visible.length}개 더 보기`, `Show ${matching.length - visible.length} more`)}
                    </Button>
                  </Row>
                ) : null}
              </KitCard>
            )}
          </div>

          {!phone && activeAnchor != null && activeIndex != null ? (
            <aside
              aria-label={tr("선택한 앵커", "Selected anchor")}
              data-testid="audit-anchor-inspector"
              className="w-full xl:sticky xl:top-4 xl:max-h-[calc(100dvh-2rem)] xl:w-[320px] xl:shrink-0 xl:overflow-y-auto"
            >
              <KitCard>
                <AnchorDetailSections
                  anchor={activeAnchor}
                  index={activeIndex}
                  anchors={anchors}
                  agents={agents}
                  startedAt={read?.session_started_at}
                  phone={false}
                  isKo={isKo}
                  tr={tr}
                  copied={copied}
                  onJump={goTo}
                />
              </KitCard>
            </aside>
          ) : null}
        </div>
      )}
    </div>
  );
}
