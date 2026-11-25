import { ChevronRight, Search, TriangleAlert } from "lucide-react";
import { useId, useState, type CSSProperties, type ReactNode } from "react";
import type { CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { formatRelativeTime } from "../../app/shellFormatting";
import { useWorkspaceAgents } from "../../contexts/WorkspaceContext";
import { localeName } from "../../i18n";
import { Button } from "../../kit/free/button-clean";
import { TextField } from "../../kit/free/text-field-clean";
import AgentPersona from "../AgentPersona";
import {
  Cap, CardNote, Chip, Desc, KitCard, Num, ROW_HOVER_CLASS, Row, StatusDot, Well, cn, type DotTone,
} from "../common/KitPrimitives";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { CampaignGateApprovalCard } from "./CampaignApprovalBanner";
import { findAgent, findTasks, taskCounts, type OverviewTask, type RecentEntry } from "./campaignOverviewModel";
import { LABELS, LiveChip, liveSession, type Tr } from "./campaignPresentation";
import type { Connection } from "./serverCampaignModel";
import { StatusMarker, nodeMarker } from "./StatusMarker";
import { TaskRow, moveListFocus } from "./TaskListParts";

export type TaskView = OverviewTask & { live?: CampaignNodeLive; hold?: string | null };

const STATUS_TEXT: Partial<Record<CampaignNodeStatus, string>> = { blocked: "text-[var(--warn)]", failed: "text-[var(--err)]" };
const DOT_TONE: Partial<Record<CampaignNodeStatus, DotTone>> = { running: "info", completed: "ok", blocked: "warn", failed: "err" };
const BUCKETS: CampaignNodeStatus[] = ["pending", "completed", "skipped", "failed"];
const LIST_PREVIEW = 8;
const SELECTED_CARD_STYLE = {
  borderColor: "var(--accent)",
  background: "color-mix(in srgb, var(--accent) 7%, var(--bg-2))",
  "--ui-crdc-shadow-selected": "none",
} as CSSProperties;

const clock = (date: Date, seconds = false) => date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: seconds ? "2-digit" : undefined });
// Today's entries read as a clock time; older ones lead with the month and day.
const when = (value: string) => { const date = new Date(value); return date.toDateString() === new Date().toDateString() ? clock(date) : `${date.getMonth() + 1}/${date.getDate()} ${clock(date)}`; };

/** The 현황 tab: landing approval banner, summary numbers, what needs someone, running work as cards, the other statuses as lists, recent changes. */
export default function CampaignOverview({ tasks, limit, showPr, hasTracker = false, asOf, summaryExtra, recent, footer, selectedId, controls, onOpen, campaignId = null, connection = "live", onApproved, language, tr }: {
  tasks: TaskView[]; limit: number | null; showPr: boolean; hasTracker?: boolean; asOf: string | null; summaryExtra?: ReactNode;
  recent: { title: string; entries: RecentEntry[] | null; note?: string | null }; footer?: ReactNode;
  selectedId: string | null; controls?: string; onOpen: (id: string) => void;
  campaignId?: string | null; connection?: Connection; onApproved?: () => void;
  language: string; tr: Tr;
}) {
  const phone = useIsMobile();
  const roster = useWorkspaceAgents();
  const counts = taskCounts(tasks);
  const ko = language === "ko";
  const running = tasks.filter((task) => task.status === "running").sort((a, b) => (Date.parse(b.updatedAt ?? "") || 0) - (Date.parse(a.updatedAt ?? "") || 0));
  const landingGates = tasks.filter((task) => task.gate?.gate === "landing");
  const otherGates = tasks.filter((task) => task.gate && task.gate.gate !== "landing");
  const stuck = tasks.filter((task) => task.attention && !task.gate);
  const [allRunning, setAllRunning] = useState(false);
  const [allStuck, setAllStuck] = useState(false);
  const [allLanding, setAllLanding] = useState(false);
  const [openStatus, setOpenStatus] = useState<CampaignNodeStatus | null>(null);
  const [listAll, setListAll] = useState(false);
  const [query, setQuery] = useState("");
  const [finding, setFinding] = useState(false);
  const [foundAll, setFoundAll] = useState(false);
  const found = findTasks(tasks, query);
  const listId = useId();
  const findId = useId();
  const cardLimit = phone ? 2 : 6;
  const stuckLimit = phone ? 1 : 3;
  const buckets = BUCKETS.map((status) => ({ status, tasks: tasks.filter((task) => task.status === status) })).filter((bucket) => bucket.tasks.length > 0);
  const open = buckets.find((bucket) => bucket.status === openStatus);
  const row = (task: TaskView, note?: ReactNode) => <TaskRow key={task.id} data-node-id={task.id} data-task-id={task.id} selected={task.id === selectedId} aria-current={task.id === selectedId || undefined}
    aria-controls={controls} onClick={() => onOpen(task.id)} className="campaign-task-row" numberClassName="campaign-task-id" textClassName="campaign-task-title"
    marker={<StatusMarker {...task.marker} />} number={task.number} title={task.title}
    meta={<><span className={STATUS_TEXT[task.status] ?? ""}>{tr(...LABELS[task.status])}</span>{task.stage && <> · <span className="campaign-task-stage" title={task.stage}>{task.stage}</span></>}</>}
    note={note} flags={<>{task.hold && <Chip tone="warn" title={task.hold}>{tr("보류: 머지 뒤 남은 일", "On hold: post-merge work")}</Chip>}<LiveChip live={task.live} tr={tr} /></>} />;
  const reason = (task: TaskView) => task.status === "pending" && task.blocker
    && <span className="campaign-reason line-clamp-2 text-xs text-[var(--fg-muted)] [overflow-wrap:anywhere]">{tr("대기 사유", "Waiting on")}: {task.blocker}</span>;
  // Phones open the search from a chip so the first screen keeps its height.
  const search = (!phone || finding) && <TextField type="search" value={query} autoFocus={phone} className={phone ? "" : "col-span-full @[40rem]/overview:ml-auto @[40rem]/overview:w-64"}
    onChange={(event) => { setFoundAll(false); setQuery(event.target.value); }} aria-label={tr("작업 찾기", "Find a task")} aria-controls={findId}
    placeholder={tr("제목, 번호, 담당, 세션…", "Title, ID, assignee, session…")} prefix={<Search aria-hidden />} />;
  const stat = (label: string, value: string) => <div className="grid gap-0.5"><Cap>{label}</Cap><Num className="text-lg leading-6 font-semibold text-[var(--fg)]">{value}</Num></div>;

  const renderApproval = (task: TaskView) => (
    <CampaignGateApprovalCard
      key={task.id}
      taskId={task.id}
      taskTitle={task.title}
      taskNumber={task.number}
      gate={task.gate!}
      stageText={task.step ? tr(...task.step) : task.stage || null}
      prUrl={task.prUrl}
      campaignId={campaignId}
      connection={connection}
      onOpenTask={onOpen}
      onApproved={onApproved}
      tr={tr}
    />
  );

  return <div className="campaign-overview @container/overview flex min-w-0 flex-col gap-4">
    {landingGates.length > 0 && <section aria-label={tr("착지 승인 대기", "Awaiting landing approval")} className="campaign-landing-banner grid min-w-0 gap-2">
      {(allLanding ? landingGates : landingGates.slice(0, 1)).map(renderApproval)}
      {landingGates.length > 1 && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={allLanding} onClick={() => setAllLanding((v) => !v)}>
        {allLanding ? tr("착지 승인 배너 접기", "Show fewer approvals") : tr(`착지 승인 대기 ${landingGates.length - 1}개 더 보기`, `Show ${landingGates.length - 1} more landing approvals`)}
      </Button></div>}
    </section>}

    <Well as="section" aria-label={tr("요약", "Summary")} className="flex min-w-0 flex-wrap items-center gap-x-6 gap-y-3 px-4 py-3 max-[899px]:order-last">
      {stat(tr("진행 작업", "Running"), limit ? `${counts.running} / ${limit}` : String(counts.running))}
      {showPr && stat(tr("진행 중 PR", "Running with PR"), `${counts.withPr} / ${counts.running}`)}
      {!hasTracker && stat(tr("완료", "Done"), `${counts.done} / ${tasks.length}`)}
      {summaryExtra && <div className="flex min-w-0 flex-wrap items-center gap-3 @[36rem]/overview:border-l @[36rem]/overview:border-[var(--line-soft)] @[36rem]/overview:pl-6">{summaryExtra}</div>}
    </Well>

    {(otherGates.length > 0 || stuck.length > 0) && <section aria-label={tr("주의", "Needs attention")} className="campaign-attention grid min-w-0 gap-2">
      {otherGates.map(renderApproval)}
      {stuck.length > 0 && <KitCard pad="none">
        <div className="grid min-w-0 gap-0.5 p-1.5" onKeyDown={moveListFocus}>
          <h3 className="m-0 flex items-center gap-1.5 px-2 pt-1 !text-[13px] !font-semibold text-[var(--warn)]"><TriangleAlert aria-hidden className="size-3.5" />{tr(`막힘·실패 ${stuck.length}`, `Blocked or failed ${stuck.length}`)}</h3>
          {(allStuck ? stuck : stuck.slice(0, stuckLimit)).map((task) => row(task, task.blocker && <span className="campaign-blocker line-clamp-2 text-xs text-[var(--warn)] [overflow-wrap:anywhere]">{tr("막힘", "Blocked")}: {task.blocker}</span>))}
          {stuck.length > stuckLimit && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={allStuck} onClick={() => setAllStuck((value) => !value)}>{allStuck ? tr("접기", "Show fewer") : tr(`${stuck.length - stuckLimit}개 더 보기`, `Show ${stuck.length - stuckLimit} more`)}</Button></div>}
        </div>
      </KitCard>}
    </section>}

    <section aria-label={tr("진행 중인 작업", "Running tasks")} className="grid min-w-0 gap-2">
      <div className="flex items-baseline justify-between gap-2 px-1">
        <h3 className="m-0 !text-sm !font-semibold text-[var(--fg)]">{tr("진행 중인 작업", "Running tasks")} <Num className="text-xs font-normal text-[var(--fg-muted)]">{running.length}</Num></h3>
        {asOf && <Cap className="font-display tabular-nums">{tr("기준", "As of")} {clock(new Date(asOf), true)}</Cap>}
      </div>
      {running.length === 0 && <p className="px-1 text-xs text-[var(--fg-muted)]">{tr("지금 진행 중인 작업이 없어요.", "Nothing is running right now.")}</p>}
      <div className="grid min-w-0 gap-3 @[40rem]/overview:grid-cols-2">
        {(allRunning ? running : running.slice(0, cardLimit)).map((task) => {
          const agent = findAgent(roster, task.agent);
          const selected = task.id === selectedId;
          // A ledger's stage is a coordinator's note, kept for the detail; its card reads the status and the step it names.
          const progress = task.progress ?? (task.step && `${tr(...LABELS[task.status])} · ${tr(...task.step)}`);
          const session = task.sessionNow ?? liveSession(task.live, tr);
          const seen = session ? session.seenAt : task.updatedAt;
          return <KitCard key={task.id} as="article" pad="none" selected={selected} style={selected ? SELECTED_CARD_STYLE : undefined}>
            <button type="button" data-glance-id={task.id} data-task-id={task.id} aria-current={selected || undefined} aria-controls={controls} onClick={() => onOpen(task.id)}
              className={cn("campaign-task-card m-0 flex h-full w-full min-w-0 cursor-pointer flex-col gap-2 border-0 bg-transparent px-3.5 py-3 text-left font-inherit text-inherit motion-reduce:transition-none sm:px-4 sm:py-3.5", !selected && ROW_HOVER_CLASS)}>
              <span className="flex min-w-0 items-start gap-2.5">
                {agent && <AgentPersona agent={agent} size={32} />}
                <span className="grid min-w-0 flex-1">
                  <strong title={task.title} className="line-clamp-2 text-sm font-semibold leading-5 text-[var(--fg)] [overflow-wrap:anywhere] [word-break:keep-all]">{task.title}</strong>
                  {(task.number || !phone) && <span className="truncate font-mono text-xs text-[var(--fg-muted)]">{phone ? task.number : task.number ? `${task.number} · ${task.id}` : task.id}</span>}
                </span>
                {phone ? <ChevronRight aria-hidden className="mt-0.5 size-4 shrink-0 text-[var(--fg-muted)]" /> : task.step && <Chip tone="info">{tr(...task.step)}</Chip>}
              </span>
              {task.summary && <span title={task.summary} className="campaign-task-summary truncate text-[13px] leading-[18px] text-[var(--fg)]">{task.summary}</span>}
              {progress && <span className="campaign-task-progress truncate text-[13px] leading-[18px] font-medium text-[var(--fg)]">{progress}</span>}
              {task.benefit && <Desc className="line-clamp-2 max-[899px]:line-clamp-1">{tr("끝나면", "When done")}: {task.benefit}</Desc>}
              {task.blocker && <Desc className="campaign-blocker line-clamp-2 text-[var(--warn)] [overflow-wrap:anywhere] max-[899px]:line-clamp-1">{tr("막힘", "Blocked")}: {task.blocker}</Desc>}
              {(session || task.agent || seen || task.live) && <Cap className="campaign-task-foot mt-auto flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 border-t border-[var(--line-soft)] pt-2">
                {session && <span className="flex min-w-0 items-center gap-1.5"><StatusDot tone={session.tone} /><span className="truncate">{session.text}</span></span>}
                {seen && <Num>{session ? formatRelativeTime(Date.parse(seen), ko) : `${tr("갱신", "Updated")} ${formatRelativeTime(Date.parse(seen), ko)}`}</Num>}
                {task.agent && <span className="min-w-0 truncate">{tr("담당", "Owner")} {agent ? localeName(language, agent) : task.agent}</span>}
                <LiveChip live={task.live} tr={tr} />
              </Cap>}
            </button>
          </KitCard>;
        })}
      </div>
      {running.length > cardLimit && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={allRunning} onClick={() => setAllRunning((value) => !value)}>{allRunning ? tr("접기", "Show fewer") : tr(`진행 중 ${running.length}개 모두 보기`, `Show all ${running.length} running`)}</Button></div>}
    </section>

    {tasks.length > 0 && <section aria-label={tr("나머지 작업", "Other tasks")} className="grid min-w-0 gap-2" onKeyDown={moveListFocus}>
      <div className="campaign-status-chips grid min-w-0 grid-cols-[repeat(auto-fit,minmax(6rem,1fr))] items-center gap-2 @[40rem]/overview:flex @[40rem]/overview:flex-wrap">
        {buckets.map((bucket) => <Button key={bucket.status} variant={openStatus === bucket.status ? "primary" : "secondary"} size="sm" data-list-nav
          aria-expanded={openStatus === bucket.status} aria-controls={listId} icon={phone ? undefined : <StatusMarker {...nodeMarker(bucket.status)} />}
          className="w-full @[40rem]/overview:w-auto"
          onClick={() => { setListAll(false); setOpenStatus((current) => current === bucket.status ? null : bucket.status); }}>
          {tr(...LABELS[bucket.status])}<span className="font-display tabular-nums opacity-75"> {bucket.tasks.length}</span>
        </Button>)}
        {phone ? <Button variant={finding ? "primary" : "secondary"} size="sm" data-list-nav aria-expanded={finding} aria-controls={findId}
          icon={<Search aria-hidden />} className="campaign-find col-span-full w-full" onClick={() => setFinding((value) => !value)}>
          {tr("작업 찾기", "Find a task")}
        </Button> : search}
      </div>
      {phone && search}
      {query.trim() && <KitCard pad="none">
        <div className="grid min-w-0 gap-1 p-1.5">
          <div id={findId} role="list" className="grid min-w-0 gap-0.5" aria-label={tr("찾은 작업", "Found tasks")}>
            {(foundAll ? found : found.slice(0, LIST_PREVIEW)).map((task) => <div key={task.id} role="listitem" className="min-w-0">{row(task, reason(task))}</div>)}
          </div>
          {found.length === 0 && <p className="px-2 py-2 text-xs text-[var(--fg-muted)]">{tr("찾는 작업이 없어요.", "No task matches.")}</p>}
          {found.length > LIST_PREVIEW && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={foundAll} onClick={() => setFoundAll((value) => !value)}>{foundAll ? tr("처음 몇 개만 보기", "Show fewer") : tr(`${found.length - LIST_PREVIEW}개 더 보기`, `Show ${found.length - LIST_PREVIEW} more`)}</Button></div>}
        </div>
      </KitCard>}
      {open && <KitCard pad="none">
        <div className="grid min-w-0 gap-1 p-1.5">
          <div id={listId} role="list" className="grid min-w-0 gap-0.5" aria-label={tr(...LABELS[open.status])}>
            {(listAll ? open.tasks : open.tasks.slice(0, LIST_PREVIEW)).map((task) => <div key={task.id} role="listitem" className="min-w-0">{row(task, reason(task))}</div>)}
          </div>
          {open.tasks.length > LIST_PREVIEW && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={listAll} onClick={() => setListAll((value) => !value)}>{listAll ? tr("처음 몇 개만 보기", "Show fewer") : tr(`${open.tasks.length - LIST_PREVIEW}개 더 보기`, `Show ${open.tasks.length - LIST_PREVIEW} more`)}</Button></div>}
        </div>
      </KitCard>}
    </section>}

    <KitCard as="section" aria-label={recent.title} pad="none" className="campaign-recent">
      <h3 className="campaign-recent-heading m-0 flex min-h-11 flex-wrap items-center gap-1 px-4 py-3 !text-sm !font-semibold text-[var(--fg)]">
        {recent.title} · <Num className="font-normal text-[var(--fg-muted)]">{recent.entries?.length ?? "—"}</Num>{recent.note && <span className="font-normal text-[var(--warn)]"> · {recent.note}</span>}
      </h3>
      {recent.entries?.length === 0 && <CardNote>{tr("아직 기록이 없어요.", "Nothing recorded yet.")}</CardNote>}
      {!!recent.entries?.length && <ol className="m-0 list-none border-t border-[var(--line-soft)] p-0">
        {recent.entries.map((entry) => <Row as="li" key={entry.key} density="sm">
          <time dateTime={entry.at} className="w-11 shrink-0 font-display text-xs tabular-nums text-[var(--fg-muted)]">{when(entry.at)}</time>
          <StatusDot tone={entry.status ? DOT_TONE[entry.status] ?? "neutral" : "info"} />
          <span className="min-w-0 flex-1 truncate text-[13px] text-[var(--fg)]" title={entry.text}>{entry.text}</span>
        </Row>)}
      </ol>}
    </KitCard>
    {footer}
  </div>;
}
