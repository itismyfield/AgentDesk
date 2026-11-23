import { useState } from "react";
import type * as api from "../../api";
import type { Agent, DispatchedSession } from "../../types";
import type { DiscordBinding } from "../../api/client";
import { getProviderMeta } from "../../app/providerTheme";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { Cap } from "../common/KitPrimitives";
import type { Translator } from "./types";
import {
  AgentDetailSection,
  DiscordDeepLinkChip,
  DiscordSummaryLabel,
  SectionEmpty,
  SectionLoading,
  type DetailFold,
} from "./AgentInfoCardSections";
import { bindingSourceLabel, inferBindingSource, isTurnSession } from "./AgentInfoCardModel";
import { ClampedText } from "./AgentInfoProfileSections";
import type { Read } from "./useAgentDetailData";
import {
  describeDiscordBinding,
  describeDiscordTarget,
  describeDispatchedSession,
  formatDiscordSummary,
} from "./discord-routing";

type ChannelLookup = (channelId: string | null | undefined) => api.DiscordChannelInfo | null;
type ParentLookup = (channelInfo: api.DiscordChannelInfo | null | undefined) => api.DiscordChannelInfo | null;

const rowClass = "flex min-w-0 items-start gap-2.5 rounded-(--r-md) px-3 py-2 shadow-[inset_0_0_0_1px_var(--line-soft)]";
const FINISHED_PAGE = 20;

/** Connections: Discord routes and live sessions. Finished sessions (hundreds on busy agents) stay a count and open
 * twenty at a time, without Discord lookups. */
export function AgentConnectionSection({ tr, fold, summary, bindings, sessions, resolveChannel, resolveParent }: {
  tr: Translator;
  fold: DetailFold;
  summary: string;
  bindings: DiscordBinding[];
  sessions: Read<{ live: DispatchedSession[]; finished: DispatchedSession[] }>;
  resolveChannel: ChannelLookup;
  resolveParent: ParentLookup;
}) {
  const [shown, setShown] = useState(0);
  const live = sessions?.live ?? [];
  const finished = sessions?.finished ?? [];
  const rest = finished.length - shown;

  const sessionRow = (session: DispatchedSession, open: boolean) => {
    const channelInfo = open ? resolveChannel(session.thread_channel_id ?? null) : null;
    const target = describeDispatchedSession(session, channelInfo, resolveParent(channelInfo));
    const working = isTurnSession(session);
    return (
      <li key={session.id} className={rowClass}>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1">
            <DiscordSummaryLabel summary={target} />
            <DiscordDeepLinkChip deepLink={target.deepLink} label={tr("앱", "App")} />
          </div>
          <div className="mt-0.5 truncate text-xs text-(--fg-muted)">{session.session_info || session.model || "AgentDesk session"}</div>
        </div>
        <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
          <Badge tone="neutral">{getProviderMeta(session.provider).label}</Badge>
          <Badge tone={working ? "success" : "neutral"} dot={open} live={working}>
            {working ? tr("작업중", "Working") : open ? tr("대기", "Idle") : tr("끝남", "Finished")}
          </Badge>
        </div>
      </li>
    );
  };

  return (
    <AgentDetailSection title={tr("연결", "Connections")} summary={summary} fold={fold} testId="agent-detail-connections">
      {bindings.length > 0 ? (
        <ul className="m-0 flex list-none flex-col gap-1.5 p-0">
          {bindings.map((binding) => {
            const source = inferBindingSource(binding);
            const channelInfo = resolveChannel(binding.channelId);
            const target = describeDiscordBinding(binding, channelInfo, resolveParent(channelInfo));
            const counterInfo = resolveChannel(binding.counterModelChannelId ?? null);
            const counter = binding.counterModelChannelId && binding.counterModelChannelId !== binding.channelId
              ? describeDiscordTarget(binding.counterModelChannelId, counterInfo, resolveParent(counterInfo))
              : null;
            return (
              <li key={`${binding.channelId}:${source}`} className={`${rowClass} items-center`}>
                <div className="min-w-0 flex-1">
                  <div className="flex min-w-0 items-center gap-1">
                    <DiscordSummaryLabel summary={target} />
                    <DiscordDeepLinkChip deepLink={target.deepLink} label={tr("앱", "App")} />
                  </div>
                  {counter ? <div className="mt-0.5 truncate text-xs text-(--fg-muted)">{`counter: ${formatDiscordSummary(counter)}`}</div> : null}
                </div>
                <Badge tone="purple">{bindingSourceLabel(source)}</Badge>
              </li>
            );
          })}
        </ul>
      ) : null}
      {sessions === undefined ? (
        <SectionLoading label={tr("불러오는 중...", "Loading...")} />
      ) : live.length === 0 ? (
        <SectionEmpty>{sessions === null ? tr("세션을 불러오지 못했어요", "Could not load sessions") : tr("지금 열린 세션이 없어요", "No open sessions right now")}</SectionEmpty>
      ) : (
        <ul className="m-0 flex list-none flex-col gap-1.5 p-0">{live.map((session) => sessionRow(session, true))}</ul>
      )}
      {finished.length > 0 && shown > 0 ? (
        <>
          <Cap>{tr(`끝난 세션 ${finished.length}개`, `${finished.length} finished sessions`)}</Cap>
          <ul className="m-0 flex list-none flex-col gap-1.5 p-0" data-testid="agent-detail-finished">
            {finished.slice(0, shown).map((session) => sessionRow(session, false))}
          </ul>
        </>
      ) : null}
      {finished.length > 0 ? (
        <div className="flex flex-wrap gap-1.5">
          {rest > 0 ? (
            <Button variant="ghost" size="sm" className="-ml-2" onClick={() => setShown((count) => count + FINISHED_PAGE)}>
              {shown === 0
                ? tr(`끝난 세션 ${finished.length}개`, `${finished.length} finished sessions`)
                : tr(`${Math.min(FINISHED_PAGE, rest)}개 더 보기 · 남은 ${rest}개`, `Show ${Math.min(FINISHED_PAGE, rest)} more · ${rest} left`)}
            </Button>
          ) : null}
          {shown > 0 ? (
            <Button variant="ghost" size="sm" onClick={() => setShown(0)}>{tr("끝난 세션 접기", "Hide finished sessions")}</Button>
          ) : null}
        </div>
      ) : null}
    </AgentDetailSection>
  );
}

/** Source of truth: the ids and name the server keys on, the RoleMap note, and the personality prompt. */
export function AgentSourceSection({ agent, tr, fold, roleMap }: { agent: Agent; tr: Translator; fold: DetailFold; roleMap: boolean }) {
  const rows = [
    { label: tr("에이전트 ID", "Agent ID"), value: agent.id },
    { label: tr("이름", "Name"), value: agent.name },
    ...(agent.role_id ? [{ label: tr("역할 ID", "Role ID"), value: agent.role_id }] : []),
  ];
  return (
    <AgentDetailSection title={tr("정본", "Source of truth")} fold={fold} testId="agent-detail-source">
      <dl className="m-0 grid min-w-0 grid-cols-[5.5rem_minmax(0,1fr)] gap-x-3 gap-y-1 text-[13px] leading-6">
        {rows.map((row) => (
          <div key={row.label} className="contents">
            <dt className="text-(--fg-muted)">{row.label}</dt>
            <dd className="m-0 min-w-0 break-all font-mono text-xs leading-6 text-(--fg)">{row.value}</dd>
          </div>
        ))}
      </dl>
      {roleMap ? (
        <p className="m-0 rounded-(--r-md) bg-[color-mix(in_oklch,var(--warn)_12%,var(--bg-1))] px-3 py-2 text-xs leading-5 text-(--fg-dim)">
          {tr(
            "RoleMap 경로가 있으면 Discord source-of-truth는 role_map 우선으로 봅니다.",
            "When RoleMap exists, role_map is treated as the Discord source-of-truth.",
          )}
        </p>
      ) : null}
      {agent.personality ? (
        <>
          <Cap>{tr("성격", "Personality")}</Cap>
          <ClampedText key={agent.id} text={agent.personality} tr={tr} />
        </>
      ) : null}
    </AgentDetailSection>
  );
}
