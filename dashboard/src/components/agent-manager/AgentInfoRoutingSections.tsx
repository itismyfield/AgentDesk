import type * as api from "../../api";
import type { DispatchedSession } from "../../types";
import type { DiscordBinding } from "../../api/client";
import { getProviderMeta } from "../../app/providerTheme";
import {
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type { Translator } from "./types";
import {
  DiscordDeepLinkChip,
  DiscordSummaryLabel,
} from "./AgentInfoCardSections";
import { bindingSourceLabel, inferBindingSource } from "./AgentInfoCardModel";
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
  claudeSessions: DispatchedSession[];
  loadingClaudeSessions: boolean;
  sourceOfTruthRows: SourceOfTruthRow[];
  resolveDiscordChannelInfo: (channelId: string | null | undefined) => api.DiscordChannelInfo | null;
  resolveDiscordParentInfo: (
    channelInfo: api.DiscordChannelInfo | null | undefined,
  ) => api.DiscordChannelInfo | null;
}

export function AgentInfoRoutingSections({
  tr,
  discordBindings,
  roleMapBindings,
  claudeSessions,
  loadingClaudeSessions,
  sourceOfTruthRows,
  resolveDiscordChannelInfo,
  resolveDiscordParentInfo,
}: AgentInfoRoutingSectionsProps) {
  return (
    <>
      <SurfaceSubsection title={tr("정본 연결", "Source of Truth")} className="md:col-span-2">
        <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
          {sourceOfTruthRows.map((row) => (
            <SurfaceCard key={row.label} className="p-3" style={{ background: "var(--th-bg-surface)" }}>
              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {row.label}
              </div>
              <div className="mt-1 break-all text-xs font-medium" style={{ color: row.tone }}>
                {row.value}
              </div>
            </SurfaceCard>
          ))}
        </div>
        {roleMapBindings.length > 0 && (
          <SurfaceNotice tone="warn" compact className="mt-3">
            {tr(
              "RoleMap 경로가 있으면 Discord source-of-truth는 role_map 우선으로 봅니다.",
              "When RoleMap exists, role_map is treated as the Discord source-of-truth.",
            )}
          </SurfaceNotice>
        )}
      </SurfaceSubsection>

      {discordBindings.length > 0 && (
        <SurfaceSubsection
          title={`${tr("Discord 라우팅", "Discord Routing")} (${discordBindings.length})`}
          description={tr(
            "RoleMap/Primary/Alt/Codex는 이 agent에 연결된 Discord 경로의 source다.",
            "RoleMap/Primary/Alt/Codex indicate how this agent is wired to Discord.",
          )}
          className="md:col-span-2"
        >
          <div className="space-y-1">
            {discordBindings.map((binding) => {
              const source = inferBindingSource(binding);
              const sourceLabel = bindingSourceLabel(source);
              const channelInfo = resolveDiscordChannelInfo(binding.channelId);
              const channelSummary = describeDiscordBinding(
                binding,
                channelInfo,
                resolveDiscordParentInfo(channelInfo),
              );
              const counterChannelInfo = resolveDiscordChannelInfo(
                binding.counterModelChannelId ?? null,
              );
              const counterSummary =
                binding.counterModelChannelId && binding.counterModelChannelId !== binding.channelId
                  ? describeDiscordTarget(
                      binding.counterModelChannelId,
                      counterChannelInfo,
                      resolveDiscordParentInfo(counterChannelInfo),
                    )
                  : null;

              return (
                <SurfaceCard
                  key={`${binding.channelId}:${source}`}
                  className="flex items-center gap-2 px-2.5 py-1.5"
                  style={{ background: "var(--th-bg-surface)" }}
                >
                  <span className="text-sm">💬</span>
                  <div className="min-w-0 flex-1">
                    <div className="flex min-w-0 items-center gap-2">
                      <DiscordSummaryLabel summary={channelSummary} />
                      <DiscordDeepLinkChip deepLink={channelSummary.deepLink} label={tr("앱", "App")} />
                    </div>
                    {counterSummary && (
                      <div className="mt-0.5 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {`counter: ${formatDiscordSummary(counterSummary)}`}
                      </div>
                    )}
                  </div>
                  <span
                    className="rounded px-1.5 py-0.5 text-xs"
                    style={{ background: "rgba(88,101,242,0.15)", color: "#7289da" }}
                  >
                    {sourceLabel}
                  </span>
                </SurfaceCard>
              );
            })}
          </div>
        </SurfaceSubsection>
      )}

      <SurfaceSubsection
        title={`${tr("연결된 AgentDesk 세션", "Linked AgentDesk Sessions")}${!loadingClaudeSessions ? ` (${claudeSessions.length})` : ""}`}
        className="md:col-span-2"
      >
        {loadingClaudeSessions ? (
          <SurfaceNotice tone="neutral" compact>
            {tr("불러오는 중...", "Loading...")}
          </SurfaceNotice>
        ) : claudeSessions.length === 0 ? (
          <SurfaceEmptyState className="text-xs">
            {tr("연결된 AgentDesk 세션 없음", "No linked AgentDesk sessions")}
          </SurfaceEmptyState>
        ) : (
          <div className="space-y-1.5">
            {claudeSessions.map((session) => {
              const providerMeta = getProviderMeta(session.provider);
              const sessionChannelInfo = resolveDiscordChannelInfo(
                session.thread_channel_id ?? null,
              );
              const sessionSummary = describeDispatchedSession(
                session,
                sessionChannelInfo,
                resolveDiscordParentInfo(sessionChannelInfo),
              );

              return (
                <SurfaceCard
                  key={session.id}
                  className="flex items-start justify-between gap-2 px-2.5 py-2"
                  style={{ background: "var(--th-bg-surface)" }}
                >
                  <div className="min-w-0">
                    <div className="flex min-w-0 items-center gap-2">
                      <DiscordSummaryLabel summary={sessionSummary} />
                      <DiscordDeepLinkChip deepLink={sessionSummary.deepLink} label={tr("앱", "App")} />
                    </div>
                    <div className="mt-0.5 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {session.session_info || session.model || "AgentDesk session"}
                    </div>
                  </div>
                  <div className="flex shrink-0 items-center gap-1">
                    <span
                      className="rounded px-1.5 py-0.5 text-xs"
                      style={{
                        background: providerMeta.bg,
                        color: providerMeta.color,
                        border: `1px solid ${providerMeta.border}`,
                      }}
                    >
                      {providerMeta.label}
                    </span>
                    <span
                      className="rounded px-1.5 py-0.5 text-xs"
                      style={{
                        background:
                          session.status === "working"
                            ? "rgba(16,185,129,0.15)"
                            : "rgba(100,116,139,0.15)",
                        color: session.status === "working" ? "#34d399" : "#94a3b8",
                      }}
                    >
                      {session.status === "working" ? tr("작업중", "Working") : tr("대기", "Idle")}
                    </span>
                  </div>
                </SurfaceCard>
              );
            })}
          </div>
        )}
      </SurfaceSubsection>
    </>
  );
}
