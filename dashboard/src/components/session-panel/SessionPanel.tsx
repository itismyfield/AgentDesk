import { useId, useMemo, useState } from "react";
import { ChevronDown, X } from "lucide-react";
import type { Department, DispatchedSession } from "../../types";
import { getProviderMeta } from "../../app/providerTheme";
import { formatRelativeTime } from "../../app/shellFormatting";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import AgentPersona from "../AgentPersona";
import { Cap, Chip, RowButton, cn } from "../common/KitPrimitives";
import { agentStateLabel } from "../home/homeModel";
import { AgentOverlay } from "../agent-manager/AgentOverlay";
import type { Translator } from "../agent-manager/types";
import { SessionInfoCard } from "./SessionInfoCard";
import { compareSessions, isStaleIdleSession, sessionLastActivityTs, sessionPersona, sessionState, sessionTitle } from "./SessionPanelModel";

interface Props {
  sessions: DispatchedSession[];
  departments: Department[];
  locale: string;
  tr: Translator;
  isKo: boolean;
}

/** "Working 1 · Resting 5": live sessions per home state, in home's order. */
function stateCounts(sessions: DispatchedSession[], tr: Translator): string {
  return (["working", "background", "resting"] as const)
    .map((state) => [state, sessions.filter((session) => sessionState(session) === state).length] as const)
    .filter(([, count]) => count > 0)
    .map(([state, count]) => `${agentStateLabel(state, tr)} ${count}`)
    .join(" · ");
}

/** Live sessions no agent owns, one row each; a row opens what the server knows about it. Read-only: the server takes
 * no agent or department for a session (`PATCH /api/dispatched-sessions/{id}` ignores both). */
export function SessionPanel({ sessions, departments, locale, tr, isKo }: Props) {
  const baseId = useId();
  const [open, setOpen] = useState<string | null>(null);
  const sorted = useMemo(() => [...sessions].sort(compareSessions), [sessions]);
  return (
    <div data-testid="agents-sessions" className="flex min-w-0 flex-col gap-3">
      <Cap className="block">
        {tr(
          "에이전트에 연결되지 않은 채 열려 있는 세션이에요. 세션을 에이전트나 부서에 붙이는 저장은 아직 서버가 받지 않아요.",
          "Open sessions no agent owns. The server does not yet save a session's agent or department.",
        )}
      </Cap>
      <ul className="m-0 min-w-0 list-none overflow-hidden rounded-(--r-md) border border-(--line-soft) bg-(--bg-2) p-0">
        {sorted.map((session) => {
          const { title, child } = sessionTitle(session);
          const state = sessionState(session);
          const last = sessionLastActivityTs(session);
          const info = session.session_info?.split("\n")[0]?.trim();
          const line = [
            agentStateLabel(state, tr),
            child ? tr("백그라운드 작업", "Background job") : null,
            info || (last === null ? null : tr(`마지막 신호 ${formatRelativeTime(last, isKo)}`, `Last signal ${formatRelativeTime(last, isKo)}`)),
          ].filter(Boolean).join(" · ");
          const expanded = open === session.id;
          const detailId = `${baseId}-${session.id}`;
          return (
            <li key={session.id} className="border-t border-(--line-soft) first:border-t-0">
              <RowButton
                data-session-row={session.id}
                aria-expanded={expanded}
                aria-controls={expanded ? detailId : undefined}
                onClick={() => setOpen(expanded ? null : session.id)}
                className="min-h-14 py-2.5 text-[13px]"
              >
                <span aria-hidden className="flex"><AgentPersona {...sessionPersona(session)} size={32} /></span>
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm leading-5 font-medium" title={session.session_key}>{title}</span>
                  <span className="block truncate text-xs leading-4 text-(--fg-muted)" title={line}>{line}</span>
                </span>
                {isStaleIdleSession(session) ? <Chip tone="warn">{tr("7일 넘게 멈춤", "Idle 7+ days")}</Chip> : null}
                <span className="hidden w-20 shrink-0 truncate text-right text-xs text-(--fg-dim) sm:block">{getProviderMeta(session.provider).label}</span>
                <ChevronDown size={16} aria-hidden className={cn("shrink-0 text-(--fg-muted) transition-transform motion-reduce:transition-none", expanded && "rotate-180")} />
              </RowButton>
              {expanded ? <SessionInfoCard id={detailId} session={session} departments={departments} locale={locale} tr={tr} isKo={isKo} /> : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** One line with the count of sessions no agent owns, and the sheet that lists them. Nothing when there are none. */
export function UnlinkedSessions({ className, ...props }: Props & { className?: string }) {
  const [sheetOpen, setSheetOpen] = useState(false);
  const { sessions, tr } = props;
  if (sessions.length === 0) return null;
  const label = tr("연결되지 않은 세션", "Unlinked sessions");
  return (
    <section aria-label={label} className={cn("flex min-w-0 items-center gap-3", className)}>
      <span className="min-w-0 flex-1">
        <span className="block text-sm leading-5 font-medium text-(--fg)">
          {tr(`에이전트에 연결되지 않은 세션 ${sessions.length}`, `Sessions without an agent: ${sessions.length}`)}
        </span>
        <Cap className="mt-0.5 block">{stateCounts(sessions, tr)}</Cap>
      </span>
      <Button variant="secondary" size="lg" onClick={() => setSheetOpen(true)}>{tr("보기", "View")}</Button>
      {sheetOpen ? (
        <AgentOverlay
          variant="sheet"
          ariaLabel={label}
          widthClassName="sm:max-w-2xl"
          onClose={() => setSheetOpen(false)}
          header={(
            <div className="flex items-center justify-between gap-3">
              <span className="flex items-baseline gap-1.5 text-sm font-semibold">
                {label}
                <span className="font-display text-xs font-normal text-(--fg-muted) tabular-nums">{sessions.length}</span>
              </span>
              <IconButton icon={<X size={15} />} label={tr("닫기", "Close")} onClick={() => setSheetOpen(false)} />
            </div>
          )}
        >
          <SessionPanel {...props} />
        </AgentOverlay>
      ) : null}
    </section>
  );
}
