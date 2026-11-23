import type * as api from "../../api";
import type { DispatchedSession } from "../../types";
import type { DiscordBinding } from "../../api/client";
import { getProviderMeta } from "../../app/providerTheme";
import { Badge } from "../../kit/free/badge-clean";
import type { Translator } from "./types";
import {
  AgentDetailSection,
  DiscordDeepLinkChip,
  DiscordSummaryLabel,
  SectionEmpty,
  SectionLoading,
} from "./AgentInfoCardSections";
import { bindingSourceLabel, inferBindingSource, isTurnSession } from "./AgentInfoCardModel";
import {
  describeDiscordBinding,
  describeDiscordTarget,
  describeDispatchedSession,
  formatDiscordSummary,
} from "./discord-routing";

interface SourceOfTruthRow {
  label: string;
  value: string;
  tone?: string;
}

interface AgentInfoRoutingSectionsProps {
  tr: Translator;
  discordBindings: DiscordBinding[];
  roleMapBindings: DiscordBinding[];
  /** Live sessions only; finished ones are counted in `closedSessionCount`. */
  claudeSessions: DispatchedSession[];
  closedSessionCount: number;
  loadingClaudeSessions: boolean;
  sourceOfTruthRows: SourceOfTruthRow[];
  resolveDiscordChannelInfo: (channelId: string | null | undefined) => api.DiscordChannelInfo | null;
  resolveDiscordParentInfo: (
    channelInfo: api.DiscordChannelInfo | null | undefined,
  ) => api.DiscordChannelInfo | null;
}

const rowClass = "flex min-w-0 items-start gap-2.5 rounded-(--r-md) px-3 py-2 shadow-[inset_0_0_0_1px_var(--line-soft)]";

export function AgentInfoRoutingSections({
  tr,
  discordBindings,
  roleMapBindings,
  claudeSessions,
  closedSessionCount,
  loadingClaudeSessions,
  sourceOfTruthRows,
  resolveDiscordChannelInfo,
  resolveDiscordParentInfo,
}: AgentInfoRoutingSectionsProps) {
  return (
    <>
      <AgentDetailSection title={tr("정본 연결", "Source of Truth")}>
        <dl className="m-0 grid min-w-0 gap-1 text-[13px]">
          {sourceOfTruthRows.map((row) => (
            <div key={row.label} className="flex min-w-0 items-start gap-3">
              <dt className="w-24 shrink-0 leading-6 text-(--fg-muted)">{row.label}</dt>
              <dd className="m-0 min-w-0 flex-1 break-all font-mono text-xs leading-6" style={{ color: row.tone ?? "var(--fg)" }}>
                {row.value}
              </dd>
            </div>
          ))}
        </dl>
        {roleMapBindings.length > 0 && (
          <p className="m-0 rounded-(--r-md) bg-[color-mix(in_oklch,var(--warn)_12%,var(--bg-1))] px-3 py-2 text-xs leading-5 text-(--fg-dim)">
            {tr(
              "RoleMap 경로가 있으면 Discord source-of-truth는 role_map 우선으로 봅니다.",
              "When RoleMap exists, role_map is treated as the Discord source-of-truth.",
            )}
          </p>
        )}
      </AgentDetailSection>

      {discordBindings.length > 0 && (
        <AgentDetailSection title={`${tr("Discord 라우팅", "Discord Routing")} (${discordBindings.length})`}>
          <p className="m-0 text-xs leading-5 text-(--fg-muted)">
            {tr(
              "RoleMap/Primary/Alt/Codex는 이 agent에 연결된 Discord 경로의 source다.",
              "RoleMap/Primary/Alt/Codex indicate how this agent is wired to Discord.",
            )}
          </p>
          <ul className="m-0 flex list-none flex-col gap-1.5 p-0">
            {discordBindings.map((binding) => {
              const source = inferBindingSource(binding);
              const sourceLabel = bindingSourceLabel(source);
              const channelInfo = resolveDiscordChannelInfo(binding.channelId);
              const channelSummary = describeDiscordBinding(
                binding,
                channelInfo,
                resolveDiscordParentInfo(channelInfo),
              );
              const counterChannelInfo = resolveDiscordChannelInfo(binding.counterModelChannelId ?? null);
              const counterSummary =
                binding.counterModelChannelId && binding.counterModelChannelId !== binding.channelId
                  ? describeDiscordTarget(
                      binding.counterModelChannelId,
                      counterChannelInfo,
                      resolveDiscordParentInfo(counterChannelInfo),
                    )
                  : null;

              return (
                <li key={`${binding.channelId}:${source}`} className={`${rowClass} items-center`}>
                  <div className="min-w-0 flex-1">
                    <div className="flex min-w-0 items-center gap-1">
                      <DiscordSummaryLabel summary={channelSummary} />
                      <DiscordDeepLinkChip deepLink={channelSummary.deepLink} label={tr("앱", "App")} />
                    </div>
                    {counterSummary && (
                      <div className="mt-0.5 truncate text-xs text-(--fg-muted)">
                        {`counter: ${formatDiscordSummary(counterSummary)}`}
                      </div>
                    )}
                  </div>
                  <Badge tone="purple">{sourceLabel}</Badge>
                </li>
              );
            })}
          </ul>
        </AgentDetailSection>
      )}

      <AgentDetailSection
        title={`${tr("연결된 AgentDesk 세션", "Linked AgentDesk Sessions")}${!loadingClaudeSessions ? ` (${claudeSessions.length})` : ""}`}
      >
        {loadingClaudeSessions ? (
          <SectionLoading label={tr("불러오는 중...", "Loading...")} />
        ) : claudeSessions.length === 0 ? (
          <SectionEmpty>{tr("지금 열린 세션이 없어요", "No open sessions right now")}</SectionEmpty>
        ) : (
          <ul className="m-0 flex list-none flex-col gap-1.5 p-0">
            {claudeSessions.map((session) => {
              const providerMeta = getProviderMeta(session.provider);
              const sessionChannelInfo = resolveDiscordChannelInfo(session.thread_channel_id ?? null);
              const sessionSummary = describeDispatchedSession(
                session,
                sessionChannelInfo,
                resolveDiscordParentInfo(sessionChannelInfo),
              );
              const working = isTurnSession(session);

              return (
                <li key={session.id} className={rowClass}>
                  <div className="min-w-0 flex-1">
                    <div className="flex min-w-0 items-center gap-1">
                      <DiscordSummaryLabel summary={sessionSummary} />
                      <DiscordDeepLinkChip deepLink={sessionSummary.deepLink} label={tr("앱", "App")} />
                    </div>
                    <div className="mt-0.5 truncate text-xs text-(--fg-muted)">
                      {session.session_info || session.model || "AgentDesk session"}
                    </div>
                  </div>
                  <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
                    <Badge tone="neutral">{providerMeta.label}</Badge>
                    <Badge tone={working ? "success" : "neutral"} dot live={working}>
                      {working ? tr("작업중", "Working") : tr("대기", "Idle")}
                    </Badge>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
        {!loadingClaudeSessions && closedSessionCount > 0 ? (
          <p className="m-0 text-xs leading-5 text-(--fg-muted)">{tr(`끝난 세션 ${closedSessionCount}개`, `${closedSessionCount} finished sessions`)}</p>
        ) : null}
      </AgentDetailSection>
    </>
  );
}
