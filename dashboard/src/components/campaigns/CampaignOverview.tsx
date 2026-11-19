import { ChevronRight, LockKeyhole, TriangleAlert } from "lucide-react";
import { useId, useState, type ReactNode } from "react";
import type { CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { useWorkspaceAgents } from "../../contexts/WorkspaceContext";
import { formatRelativeTime } from "../../app/shellFormatting";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import AgentPersona from "../AgentPersona";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { safeCampaignLink } from "./campaignModel";
import { GATE_LABELS, findAgent, taskCounts, type OverviewTask, type RecentEntry } from "./campaignOverviewModel";
import { LABELS, LiveChip, type Tr } from "./campaignPresentation";
import { StatusMarker, nodeMarker } from "./StatusMarker";
import { TaskGroupHeading, TaskRow, moveListFocus } from "./TaskListParts";

export type TaskView = OverviewTask & { live?: CampaignNodeLive; hold?: string | null };

const STATUS_TEXT: Partial<Record<CampaignNodeStatus, string>> = { blocked: "text-[var(--warn)]", failed: "text-[var(--err)]" };
const DOT: Partial<Record<CampaignNodeStatus, string>> = { running: "bg-[var(--info)]", completed: "bg-[var(--ok)]", blocked: "bg-[var(--warn)]", failed: "bg-[var(--err)]" };
const BUCKETS: CampaignNodeStatus[] = ["pending", "completed", "skipped", "failed"];
const LIST_PREVIEW = 8;
/** How long an approval has waited, in minutes and then hours. */
function waited(since: string, tr: Tr) {
  const minutes = Math.max(0, Math.floor((Date.now() - Date.parse(since)) / 60_000));
  return minutes < 60 ? tr(`${minutes}분째 기다려요`, `Waiting ${minutes} min`) : tr(`${Math.floor(minutes / 60)}시간째 기다려요`, `Waiting ${Math.floor(minutes / 60)} h`);
}
const clock = (date: Date, seconds = false) => date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: seconds ? "2-digit" : undefined });
// Today's entries read as a clock time; older ones lead with the month and day.
const when = (value: string) => { const date = new Date(value); return date.toDateString() === new Date().toDateString() ? clock(date) : `${date.getMonth() + 1}/${date.getDate()} ${clock(date)}`; };

/** The 현황 tab: summary numbers, what needs someone, running work as cards, the other statuses as lists, recent changes. */
export default function CampaignOverview({ tasks, limit, showPr, asOf, summaryExtra, recent, footer, selectedId, controls, onOpen, language, tr }: {
  tasks: TaskView[]; limit: number | null; showPr: boolean; asOf: string | null; summaryExtra?: ReactNode;
  recent: { title: string; entries: RecentEntry[] | null; note?: string | null }; footer?: ReactNode;
  selectedId: string | null; controls?: string; onOpen: (id: string) => void; language: string; tr: Tr;
}) {
  const phone = useIsMobile();
  const roster = useWorkspaceAgents();
  const counts = taskCounts(tasks);
  const ko = language === "ko";
  const running = tasks.filter((task) => task.status === "running").sort((a, b) => (Date.parse(b.updatedAt ?? "") || 0) - (Date.parse(a.updatedAt ?? "") || 0));
  const gates = tasks.filter((task) => task.gate);
  const stuck = tasks.filter((task) => task.attention && !task.gate);
  const [allRunning, setAllRunning] = useState(false);
  const [allStuck, setAllStuck] = useState(false);
  const [openStatus, setOpenStatus] = useState<CampaignNodeStatus | null>(null);
  const [listAll, setListAll] = useState(false);
  const listId = useId();
  const cardLimit = phone ? 2 : 6;
  const stuckLimit = phone ? 1 : 3;
  const buckets = BUCKETS.map((status) => ({ status, tasks: tasks.filter((task) => task.status === status) })).filter((bucket) => bucket.tasks.length > 0);
  const open = buckets.find((bucket) => bucket.status === openStatus);
  const row = (task: TaskView, note?: ReactNode) => <TaskRow key={task.id} data-node-id={task.id} data-task-id={task.id} selected={task.id === selectedId} aria-current={task.id === selectedId || undefined}
    aria-controls={controls} onClick={() => onOpen(task.id)} className="campaign-task-row" numberClassName="campaign-task-id" textClassName="campaign-task-title"
    marker={<StatusMarker {...task.marker} />} number={task.number} title={task.title}
    meta={<><span className={STATUS_TEXT[task.status] ?? ""}>{tr(...LABELS[task.status])}</span>{task.stage && <> · <span className="campaign-task-stage" title={task.stage}>{task.stage}</span></>}</>}
    note={note} flags={<>{task.hold && <KitBadge tone="warning" title={task.hold}>{tr("보류: 머지 뒤 남은 일", "On hold: post-merge work")}</KitBadge>}<LiveChip live={task.live} tr={tr} /></>} />;
  const stat = (label: string, value: string) => <div className="grid gap-0.5"><span className="text-[11px] text-[var(--fg-muted)]">{label}</span><strong className="text-lg font-semibold tabular-nums text-[var(--fg)]">{value}</strong></div>;

  return <div className="campaign-overview @container/overview flex min-w-0 flex-col gap-4">
    <section aria-label={tr("요약", "Summary")} className="flex min-w-0 flex-wrap items-center gap-x-6 gap-y-3 rounded-xl border border-[var(--line)] bg-[var(--th-card-bg)] px-4 py-3 max-[899px]:order-last">
      {stat(tr("진행 작업", "Running"), limit ? `${counts.running} / ${limit}` : String(counts.running))}
      {showPr && stat(tr("PR 연결", "With PR"), `${counts.withPr} / ${counts.running}`)}
      {stat(tr("완료", "Done"), `${counts.done} / ${tasks.length}`)}
      {summaryExtra && <div className="flex min-w-0 flex-wrap items-center gap-3 @[36rem]/overview:border-l @[36rem]/overview:border-[var(--line)] @[36rem]/overview:pl-6">{summaryExtra}</div>}
    </section>

    {(gates.length > 0 || stuck.length > 0) && <section aria-label={tr("주의", "Needs attention")} className="campaign-attention grid min-w-0 gap-2">
      {gates.map((task) => <div key={task.id} className="campaign-gate grid min-w-0 gap-3 rounded-xl border border-[color-mix(in_srgb,var(--warn)_45%,var(--line))] bg-[color-mix(in_srgb,var(--warn)_10%,var(--th-card-bg))] px-4 py-3 @[36rem]/overview:grid-cols-[minmax(0,1fr)_auto] @[36rem]/overview:items-center">
        <button type="button" data-task-id={task.id} aria-controls={controls} onClick={() => onOpen(task.id)} className="!m-0 flex min-w-0 items-start gap-3 !border-0 !bg-transparent !p-0 text-left">
          <LockKeyhole aria-hidden className="mt-0.5 size-5 shrink-0 text-[var(--warn)]" />
          <span className="grid min-w-0 gap-0.5">
            <strong className="text-sm font-semibold text-[var(--fg)] [word-break:keep-all]">{tr(`${GATE_LABELS[task.gate!.gate][0]} 승인 대기`, `Waiting for ${GATE_LABELS[task.gate!.gate][1].toLowerCase()} approval`)} · {task.title}</strong>
            <span className="text-xs text-[var(--fg-muted)]">{task.gate!.opened_at ? waited(task.gate!.opened_at, tr) : task.stage}</span>
          </span>
        </button>
        <span className="grid min-w-0 gap-1">
          <span className="grid grid-cols-2 gap-2 @[36rem]/overview:flex @[36rem]/overview:items-center">
            {safeCampaignLink(task.gate!.evidence_url ?? null)
              ? <a href={safeCampaignLink(task.gate!.evidence_url ?? null)} target="_blank" rel="noopener noreferrer" className="justify-center rounded-[var(--r-sm)] border border-[var(--line)] px-3 text-xs text-[var(--fg)]">{tr("근거 보기", "View evidence")}</a>
              : <span />}
            <Button variant="primary" size="sm" disabled title={tr("승인 API 준비 중(#8249)", "Approval API in progress (#8249)")}>{tr("승인", "Approve")}</Button>
          </span>
          <span className="text-[11px] text-[var(--fg-muted)]">{tr("승인 API 준비 중(#8249)", "Approval API in progress (#8249)")}</span>
        </span>
      </div>)}
      {stuck.length > 0 && <div className="grid min-w-0 gap-0.5 rounded-xl border border-[var(--line)] p-1.5" onKeyDown={moveListFocus}>
        <h3 className="flex items-center gap-1.5 px-2 pt-1 !text-[13px] !font-semibold text-[var(--warn)]"><TriangleAlert aria-hidden className="size-3.5" />{tr(`막힘·실패 ${stuck.length}`, `Blocked or failed ${stuck.length}`)}</h3>
        {(allStuck ? stuck : stuck.slice(0, stuckLimit)).map((task) => row(task, task.blocker && <span className="campaign-blocker line-clamp-2 text-xs text-[var(--warn)] [overflow-wrap:anywhere]">{tr("막힘", "Blocked")}: {task.blocker}</span>))}
        {stuck.length > stuckLimit && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={allStuck} onClick={() => setAllStuck((value) => !value)}>{allStuck ? tr("접기", "Show fewer") : tr(`${stuck.length - stuckLimit}개 더 보기`, `Show ${stuck.length - stuckLimit} more`)}</Button></div>}
      </div>}
    </section>}

    <section aria-label={tr("진행 중인 작업", "Running tasks")} className="grid min-w-0 gap-2">
      <div className="flex items-baseline justify-between gap-2 px-1">
        <h3 className="!text-[13px] !font-semibold text-[var(--fg)]">{tr("진행 중인 작업", "Running tasks")} <span className="font-normal tabular-nums text-[var(--fg-faint)]">{running.length}</span></h3>
        {asOf && <span className="text-[11px] tabular-nums text-[var(--fg-faint)]">{tr("기준", "As of")} {clock(new Date(asOf), true)}</span>}
      </div>
      {running.length === 0 && <p className="px-1 text-xs text-[var(--fg-muted)]">{tr("지금 진행 중인 작업이 없어요.", "Nothing is running right now.")}</p>}
      <div className="grid min-w-0 gap-3 @[40rem]/overview:grid-cols-2">
        {(allRunning ? running : running.slice(0, cardLimit)).map((task) => {
          const agent = findAgent(roster, task.agent);
          return <button key={task.id} type="button" data-glance-id={task.id} data-task-id={task.id} aria-current={task.id === selectedId || undefined} aria-controls={controls} onClick={() => onOpen(task.id)}
            className={`campaign-task-card !m-0 flex min-w-0 flex-col gap-2 !rounded-xl !border !px-4 !py-3.5 text-left ${task.id === selectedId ? "!border-[var(--accent)] !bg-[var(--accent-soft)]" : "!border-[var(--line)] !bg-[var(--th-card-bg)] hover:!bg-[color-mix(in_oklch,var(--fg)_4%,var(--th-card-bg))]"}`}>
            <span className="flex min-w-0 items-start gap-2.5">
              {agent && <AgentPersona agent={agent} size={32} />}
              <span className="grid min-w-0 flex-1">
                <strong className="text-sm font-semibold leading-5 text-[var(--fg)] [overflow-wrap:anywhere] [word-break:keep-all]">{task.title}</strong>
                <span className="truncate font-mono text-[11px] text-[var(--fg-faint)]">{task.number ? `${task.number} · ${task.id}` : task.id}</span>
              </span>
              {phone ? <ChevronRight aria-hidden className="mt-0.5 size-4 shrink-0 text-[var(--fg-muted)]" /> : task.step && <KitBadge tone="info">{tr(...task.step)}</KitBadge>}
            </span>
            {task.summary && <span className="text-[13px] text-[var(--fg)] [overflow-wrap:anywhere]">{task.summary}</span>}
            <span className="campaign-task-stage line-clamp-2 text-[13px] font-medium text-[var(--fg)] [overflow-wrap:anywhere]" title={task.stage}>{task.stage}</span>
            {task.benefit && <span className="line-clamp-2 text-xs text-[var(--fg-muted)]">{tr("끝나면", "When done")}: {task.benefit}</span>}
            {task.blocker && <span className="campaign-blocker line-clamp-3 text-xs text-[var(--warn)] [overflow-wrap:anywhere]">{tr("막힘", "Blocked")}: {task.blocker}</span>}
            {(task.updatedAt || task.session || task.live) && <span className="mt-auto flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 border-t border-[var(--line)] pt-2 text-[11px] text-[var(--fg-muted)]">
              {task.updatedAt && <span className="tabular-nums">{tr("갱신", "Updated")} {formatRelativeTime(Date.parse(task.updatedAt), ko)}</span>}
              {task.session && <span className="min-w-0 truncate">{task.session.includes(" · ") ? task.session : `${tr("세션", "Session")} ${task.session.slice(0, 8)}`}</span>}
              <LiveChip live={task.live} tr={tr} />
            </span>}
          </button>;
        })}
      </div>
      {running.length > cardLimit && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={allRunning} onClick={() => setAllRunning((value) => !value)}>{allRunning ? tr("접기", "Show fewer") : tr(`진행 중 ${running.length}개 모두 보기`, `Show all ${running.length} running`)}</Button></div>}
    </section>

    {buckets.length > 0 && <section aria-label={tr("나머지 작업", "Other tasks")} className="grid min-w-0 gap-2" onKeyDown={moveListFocus}>
      <div className="campaign-status-chips grid min-w-0 grid-cols-[repeat(auto-fit,minmax(6rem,1fr))] gap-2 @[40rem]/overview:flex @[40rem]/overview:flex-wrap">
        {buckets.map((bucket) => <span key={bucket.status} className="min-w-0 rounded-[var(--r-sm)] border border-[var(--line)]">
          <TaskGroupHeading className="justify-center !px-3" open={openStatus === bucket.status} controls={listId}
            icon={<StatusMarker {...nodeMarker(bucket.status)} />} label={tr(...LABELS[bucket.status])} count={bucket.tasks.length}
            onToggle={() => { setListAll(false); setOpenStatus((current) => current === bucket.status ? null : bucket.status); }} />
        </span>)}
      </div>
      {open && <div className="grid min-w-0 gap-1">
        <div id={listId} role="list" className="grid min-w-0 gap-0.5" aria-label={tr(...LABELS[open.status])}>
          {(listAll ? open.tasks : open.tasks.slice(0, LIST_PREVIEW)).map((task) => <div key={task.id} role="listitem" className="min-w-0">{row(task)}</div>)}
        </div>
        {open.tasks.length > LIST_PREVIEW && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={listAll} onClick={() => setListAll((value) => !value)}>{listAll ? tr("처음 몇 개만 보기", "Show fewer") : tr(`${open.tasks.length - LIST_PREVIEW}개 더 보기`, `Show ${open.tasks.length - LIST_PREVIEW} more`)}</Button></div>}
      </div>}
    </section>}

    <section aria-label={recent.title} className="campaign-recent grid min-w-0 rounded-xl border border-[var(--line)] bg-[var(--th-card-bg)]">
      <h3 className="campaign-recent-heading border-b border-[var(--line)] px-4 py-3 !text-[13px] !font-semibold text-[var(--fg)]">
        {recent.title} · <span className="tabular-nums">{recent.entries?.length ?? "—"}</span>{recent.note && <span className="font-normal text-[var(--warn)]"> · {recent.note}</span>}
      </h3>
      {recent.entries?.length === 0 && <p className="px-4 py-3 text-xs text-[var(--fg-muted)]">{tr("아직 기록이 없어요.", "Nothing recorded yet.")}</p>}
      <ol className="grid min-w-0">
        {recent.entries?.map((entry) => <li key={entry.key} className="flex min-w-0 items-baseline gap-3 border-b border-[var(--line)] px-4 py-2.5 text-[13px] last:border-0">
          <time dateTime={entry.at} className="shrink-0 font-mono text-[11px] text-[var(--fg-muted)]">{when(entry.at)}</time>
          <span aria-hidden className={`size-1.5 shrink-0 self-center rounded-full ${entry.status ? DOT[entry.status] ?? "bg-[var(--fg-faint)]" : "bg-[var(--info)]"}`} />
          <span className="min-w-0 truncate text-[var(--fg)]" title={entry.text}>{entry.text}</span>
        </li>)}
      </ol>
    </section>
    {footer}
  </div>;
}
