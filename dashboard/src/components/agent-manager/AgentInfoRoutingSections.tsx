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
  SectionErrorRetry,
  SectionLoading,
  type DetailFold,
} from "./AgentInfoCardSections";
import { bindingSourceLabel, inferBindingSource, isTurnSession } from "./AgentInfoCardModel";
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
const RAW_SNOWFLAKE_RE = /^#?(\d{15,22})$/;

function extractRawChannelSnowflake(value: string): string | null {
  const match = value.trim().match(RAW_SNOWFLAKE_RE);
  return match ? match[1] : null;
}

function ConnectionSummaryLabel({
  summary,
  tr,
}: {
  summary: { title: string; subtitle: string | null; webUrl: string | null; deepLink: string | null };
  tr: Translator;
}) {
  const rawId = extractRawChannelSnowflake(summary.title);
  if (!rawId) return <DiscordSummaryLabel summary={summary} />;
  const shortLabel = `${tr("#채널", "#channel")} · ...${rawId.slice(-6)}`;
  const text = summary.subtitle ? `${shortLabel} · ${summary.subtitle}` : shortLabel;
  const href = summary.deepLink ?? summary.webUrl;
  if (!href) {
    return (
      <span className="block min-w-0 flex-1 truncate font-mono text-xs text-(--fg)" title={rawId}>
        {text}
      </span>
    );
  }
  return (
    <a
      href={href}
      className="block min-w-0 flex-1 truncate rounded-sm font-mono text-xs leading-11 text-(--fg) hover:underline sm:leading-5 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-(--accent)"
      title={rawId}
    >
      {text}
    </a>
  );
}

/** Connections: Discord routes and live sessions. Finished sessions (hundreds on busy agents) stay a count and open
 * twenty at a time, without Discord lookups. */
export function AgentConnectionSection({ tr, fold, summary, bindings, sessions, resolveChannel, resolveParent, onRetrySessions }: {
  tr: Translator;
  fold: DetailFold;
  summary: string;
  bindings: DiscordBinding[];
  sessions: Read<{ live: DispatchedSession[]; finished: DispatchedSession[] }>;
  resolveChannel: ChannelLookup;
  resolveParent: ParentLookup;
  onRetrySessions?: () => void;
}) {
  const [shown, setShown] = useState(0);
  const live = sessions?.live ?? [];
  const finished = sessions?.finished ?? [];
  const rest = finished.length - shown;

  const closedLabel = (status: DispatchedSession["status"]) => {
    if (status === "aborted") return tr("중단됨", "Aborted");
    if (status === "disconnected") return tr("연결 끊김", "Disconnected");
    return tr("끝남", "Finished");
  };

  const sessionRow = (session: DispatchedSession, open: boolean) => {
    const channelInfo = open ? resolveChannel(session.thread_channel_id ?? null) : null;
    const target = describeDispatchedSession(session, channelInfo, resolveParent(channelInfo));
    const working = isTurnSession(session);
    const waitingBg = open && session.status === "awaiting_bg";
    return (
      <li key={session.id} className={rowClass}>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1">
            <ConnectionSummaryLabel summary={target} tr={tr} />
            <DiscordDeepLinkChip deepLink={target.deepLink} label={tr("앱", "App")} />
          </div>
          <div className="mt-0.5 truncate text-xs text-(--fg-muted)">{session.session_info || session.model || "AgentDesk session"}</div>
        </div>
        <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
          <Badge tone="neutral">{getProviderMeta(session.provider).label}</Badge>
          <Badge tone={working ? "success" : waitingBg ? "warning" : "neutral"} dot={open} live={working}>
            {working ? tr("작업중", "Working") : waitingBg ? tr("백그라운드 대기", "Background wait") : open ? tr("대기", "Idle") : closedLabel(session.status)}
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
            const counterRawId = counter ? extractRawChannelSnowflake(counter.title) : null;
            return (
              <li key={`${binding.channelId}:${source}`} className={`${rowClass} items-center`}>
                <div className="min-w-0 flex-1">
                  <div className="flex min-w-0 items-center gap-1">
                    <ConnectionSummaryLabel summary={target} tr={tr} />
                    <DiscordDeepLinkChip deepLink={target.deepLink} label={tr("앱", "App")} />
                  </div>
                  {counter ? (
                    <div className="mt-0.5 truncate text-xs text-(--fg-muted)">
                      counter:{" "}
                      {counterRawId ? (
                        <span className="font-mono text-xs" title={counterRawId}>
                          {`${tr("#채널", "#channel")} · ...${counterRawId.slice(-6)}`}
                        </span>
                      ) : (
                        formatDiscordSummary(counter)
                      )}
                    </div>
                  ) : null}
                </div>
                <Badge tone="purple">{bindingSourceLabel(source)}</Badge>
              </li>
            );
          })}
        </ul>
      ) : null}
      {sessions === undefined ? (
        <SectionLoading label={tr("불러오는 중...", "Loading...")} />
      ) : sessions === null ? (
        <SectionErrorRetry
          message={tr("세션을 불러오지 못했어요", "Could not load sessions")}
          retryLabel={tr("다시 시도", "Retry")}
          onRetry={onRetrySessions}
        />
      ) : live.length === 0 ? (
        <SectionEmpty>{tr("지금 열린 세션이 없어요", "No open sessions right now")}</SectionEmpty>
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
    </AgentDetailSection>
  );
}
