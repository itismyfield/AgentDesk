import { useId, useMemo, useState } from "react";
import { ChevronDown, RotateCcw, X } from "lucide-react";
import type { Department, DispatchedSession } from "../../types";
import { getProviderMeta } from "../../app/providerTheme";
import { formatRelativeTime } from "../../app/shellFormatting";
import { Button } from "../../kit/free/button-clean";
import { IconButton } from "../../kit/free/icon-button-clean";
import AgentPersona from "../AgentPersona";
import { Cap, Chip, RowButton, cn } from "../common/KitPrimitives";
import { AgentOverlay } from "../agent-manager/AgentOverlay";
import type { Translator } from "../agent-manager/types";
import { SessionInfoCard } from "./SessionInfoCard";
import {
  canCleanupStaleIdleSession, canReconcileStaleSession, cleanupStaleIdleSession, compareSessions,
  isStaleIdleSession, reconcileStaleSessionTurn, sessionLastActivityTs, sessionPersona,
  sessionStateText, sessionSummaryCounts, sessionTitle,
} from "./SessionPanelModel";

interface Props {
  sessions: DispatchedSession[];
  departments: Department[];
  locale: string;
  tr: Translator;
  isKo: boolean;
  onSessionsChange?: () => void;
}

/** Live sessions no agent owns, one row each; a row opens live output preview + server fields, and stale rows offer one-click reconciliation. */
export function SessionPanel({ sessions, departments, locale, tr, isKo, onSessionsChange }: Props) {
  const baseId = useId();
  const [open, setOpen] = useState<string | null>(null);
  const [reconcilingKey, setReconcilingKey] = useState<string | null>(null);
  const [reconciledKeys, setReconciledKeys] = useState<Record<string, string>>({});
  const effective = useMemo(
    () =>
      sessions
        .filter((s) => reconciledKeys[s.session_key] !== "disconnected")
        .map((s) => (reconciledKeys[s.session_key] === "ok" ? { ...s, status: "idle" as const, last_seen_at: Date.now() } : s)),
    [sessions, reconciledKeys],
  );
  const sorted = useMemo(() => [...effective].sort(compareSessions), [effective]);

  const handleReconcile = async (session: DispatchedSession) => {
    setReconcilingKey(session.session_key);
    try {
      const res = await reconcileStaleSessionTurn(session.session_key);
      const verdict = res.reconciled ? "ok" : (res.reason || tr("살아 있는 턴이라 그대로 두었어요.", "Turn is still live; kept as-is."));
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: verdict }));
      if (res.reconciled) onSessionsChange?.();
    } catch {
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: tr("정리 요청에 실패했어요. 다시 시도해 보세요.", "Reconcile failed. Try again.") }));
    } finally {
      setReconcilingKey(null);
    }
  };

  const handleCleanupIdle = async (session: DispatchedSession) => {
    setReconcilingKey(session.session_key);
    try {
      const res = await cleanupStaleIdleSession(session.session_key);
      const verdict = res.disconnected ? "disconnected" : (res.reason || tr("아직 활성 상태라 세션을 유지했어요.", "Session is still active; kept as-is."));
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: verdict }));
      if (res.disconnected) onSessionsChange?.();
    } catch {
      setReconciledKeys((prev) => ({ ...prev, [session.session_key]: tr("정리 요청에 실패했어요. 다시 시도해 보세요.", "Cleanup failed. Try again.") }));
    } finally {
      setReconcilingKey(null);
    }
  };

  return (
    <div data-testid="agents-sessions" className="flex min-w-0 flex-col gap-3">
      <Cap className="block">
        {tr(
          "에이전트에 연결되지 않은 채 열려 있는 세션이에요. 세션을 에이전트나 부서에 붙이는 저장은 아직 지원하지 않으며, 행을 열면 실시간 출력을 확인하고 멈춘 턴을 바로 정리할 수 있어요.",
          "Open sessions no agent owns. Saving an agent or department assignment is not supported yet; expand a row to preview live output or reconcile stale turns.",
        )}
      </Cap>
      <ul className="m-0 min-w-0 list-none overflow-hidden rounded-(--r-md) border border-(--line-soft) bg-(--bg-2) p-0">
        {sorted.map((session) => {
          const { title, child } = sessionTitle(session);
          const last = sessionLastActivityTs(session);
          const info = session.session_info?.split("\n")[0]?.trim();
          const line = [
            sessionStateText(session, tr),
            child ? tr("백그라운드 작업", "Background job") : null,
            info || (last === null ? null : tr(`마지막 신호 ${formatRelativeTime(last, isKo)}`, `Last signal ${formatRelativeTime(last, isKo)}`)),
          ].filter(Boolean).join(" · ");
          const expanded = open === session.id, detailId = `${baseId}-${session.id}`;
          const stale = isStaleIdleSession(session), note = reconciledKeys[session.session_key];
          const reconcilable = canReconcileStaleSession(session) && note !== "ok";
          const cleanableIdle = canCleanupStaleIdleSession(session) && note !== "disconnected";
          return (
            <li key={session.id} className="border-t border-(--line-soft) first:border-t-0">
              <div className="flex min-w-0 items-center gap-1.5 pr-2">
                <RowButton data-session-row={session.id} aria-expanded={expanded} aria-controls={expanded ? detailId : undefined} onClick={() => setOpen(expanded ? null : session.id)} className="min-h-14 flex-1 py-2.5 pr-2 text-[13px]">
                  <span aria-hidden className="flex"><AgentPersona {...sessionPersona(session)} size={32} /></span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-sm leading-5 font-medium" title={session.session_key}>{title}</span>
                    <span className="block truncate text-xs leading-4 text-(--fg-muted)" title={line}>{line}</span>
                  </span>
                  {note === "ok" ? <Chip tone="ok">{tr("정리됨", "Reconciled")}</Chip> : stale ? <Chip tone="warn">{tr("7일 넘게 멈춤", "Idle 7+ days")}</Chip> : null}
                  <span className="hidden w-20 shrink-0 truncate text-right text-xs text-(--fg-dim) sm:block">{getProviderMeta(session.provider).label}</span>
                  <ChevronDown size={16} aria-hidden className={cn("shrink-0 text-(--fg-muted) transition-transform motion-reduce:transition-none", expanded && "rotate-180")} />
                </RowButton>
                {reconcilable ? (
                  <Button variant="secondary" size="sm" icon={<RotateCcw size={13} aria-hidden />} loading={reconcilingKey === session.session_key} onClick={() => void handleReconcile(session)} data-testid={`reconcile-session-${session.id}`}>
                    {tr("멈춘 턴 정리", "Reconcile stale turn")}
                  </Button>
                ) : cleanableIdle ? (
                  <Button variant="secondary" size="sm" loading={reconcilingKey === session.session_key} onClick={() => void handleCleanupIdle(session)} data-testid={`cleanup-session-${session.id}`}>
                    {tr("정리", "Clean up")}
                  </Button>
                ) : null}
              </div>
              {note && note !== "ok" ? <Cap className="block bg-(--bg-1) px-3 py-1.5 text-(--warn) sm:px-4" role="status">{note}</Cap> : null}
              {expanded ? <SessionInfoCard id={detailId} session={session} departments={departments} locale={locale} tr={tr} isKo={isKo} /> : null}
            </li>
          );
        })}
      </ul>
    </div>
  );
}

/** One line with the honest breakdown of sessions no agent owns (`작업 중 · 멈춤 · 쉬는 중`), and the sheet that lists them. */
export function UnlinkedSessions({ className, ...props }: Props & { className?: string }) {
  const [sheetOpen, setSheetOpen] = useState(false);
  const { sessions, tr } = props;
  if (sessions.length === 0) return null;
  const label = tr("연결되지 않은 세션", "Unlinked sessions");
  return (
    <section aria-label={label} className={cn("flex min-w-0 items-center gap-3", className)}>
      <span className="min-w-0 flex-1">
        <span className="block text-sm leading-5 font-medium text-(--fg)">{tr(`에이전트에 연결되지 않은 세션 ${sessions.length}`, `Sessions without an agent: ${sessions.length}`)}</span>
        <Cap className="mt-0.5 block" data-testid="unlinked-sessions-summary">{sessionSummaryCounts(sessions, tr)}</Cap>
      </span>
      <Button variant="secondary" size="lg" onClick={() => setSheetOpen(true)}>{tr("보기", "View")}</Button>
      {sheetOpen ? (
        <AgentOverlay
          variant="sheet" ariaLabel={label} widthClassName="sm:max-w-2xl" onClose={() => setSheetOpen(false)}
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
