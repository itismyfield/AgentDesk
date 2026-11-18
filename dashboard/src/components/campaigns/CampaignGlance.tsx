import { TriangleAlert } from "lucide-react";
import { useId, useMemo, useState } from "react";
import type { CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { Badge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { CAMPAIGN_STAGES, campaignGlance, campaignStageStep, campaignTaskNumber, campaignTaskTitle, liveIsWorking } from "./campaignModel";
import { LABELS, LiveChip, type Tr } from "./campaignPresentation";
import { StatusMarker, nodeMarker } from "./StatusMarker";
import { TaskGroupHeading, TaskRow, moveListFocus } from "./TaskListParts";

const LIST_PREVIEW = 8;

function StageLabel({ node, tr }: { node: CampaignNode; tr: Tr }) {
  const step = campaignStageStep(node.stage);
  return step.index < 0 ? <span className="campaign-glance-raw" title={node.stage}>{step.raw}</span> : <span>{tr(...CAMPAIGN_STAGES[step.index].label)}</span>;
}

/** Plain-language first screen: running and blocked work as rows with their notes, every other status a closed group.
 * Technical fields stay in the task details. */
export default function CampaignGlance({ nodes, live = {}, tr, onOpen, selectedId = null, controls }: {
  nodes: CampaignNode[]; live?: Record<string, CampaignNodeLive>; tr: Tr; onOpen: (id: string) => void;
  selectedId?: string | null; controls?: string;
}) {
  const glance = useMemo(() => campaignGlance(nodes, live), [nodes, live]);
  const active = [...glance.active].sort((a, b) => Number(b.status === "blocked") - Number(a.status === "blocked"));
  const [showAll, setShowAll] = useState(false);
  const [openStatus, setOpenStatus] = useState<CampaignNodeStatus | null>(null);
  const [listAll, setListAll] = useState(false);
  const listId = useId();
  const open = glance.buckets.find((bucket) => bucket.status === openStatus);
  const listed = open ? (listAll ? open.nodes : open.nodes.slice(0, LIST_PREVIEW)) : [];
  const row = (node: CampaignNode, withNote: boolean) => {
    // Titles say what the task is; summaries are often lineage notes, so they stay secondary.
    const summary = node.summary?.trim();
    const text = campaignTaskTitle(node);
    const note = withNote && (summary && summary !== text || node.benefit?.trim() || node.blocker?.trim()) ? <span className="grid min-w-0 gap-0.5 text-xs leading-5">
      {summary && summary !== text && <span className="campaign-glance-summary line-clamp-2 text-[var(--fg-muted)] [overflow-wrap:anywhere]">{summary}</span>}
      {node.benefit?.trim() && <span className="campaign-glance-benefit truncate text-[var(--fg-faint)]">{tr("기대효과", "Benefit")}: {node.benefit}</span>}
      {node.blocker?.trim() && <span className="campaign-glance-blocker flex min-w-0 items-start gap-1 font-medium text-[var(--warn)] [overflow-wrap:anywhere]">
        <TriangleAlert aria-hidden className="mt-[3px] size-3 shrink-0" />{tr("막힘", "Blocked")}: {node.blocker}
      </span>}
    </span> : null;
    const working = node.status !== "completed" && node.status !== "skipped" && liveIsWorking(live[node.id]);
    return <TaskRow key={node.id} {...(withNote ? { "data-glance-id": node.id } : {})} selected={node.id === selectedId} aria-current={node.id === selectedId || undefined}
      aria-controls={controls} onClick={() => onOpen(node.id)} fullTitle={node.title}
      marker={<StatusMarker {...nodeMarker(working ? "running" : node.status, node.stage)} />} number={campaignTaskNumber(node)} title={text}
      meta={<StageLabel node={node} tr={tr} />} note={note}
      flags={withNote && <>{node.status === "blocked" && <Badge tone="warning" dot>{tr(...LABELS.blocked)}</Badge>}<LiveChip live={live[node.id]} tr={tr} /></>} />;
  };
  return <section className="campaign-glance grid min-w-0 gap-2 border-b border-[var(--line)] pb-4" aria-label={tr("한눈 요약", "At a glance")} onKeyDown={moveListFocus}>
    <h3 className="flex min-w-0 flex-wrap items-center gap-2 px-2 !text-[13px] !font-semibold text-[var(--fg)]">
      <StatusMarker tone="progress" ratio={0.5} />
      <span>{tr("지금 진행 중", "Running now")}<span className="font-normal tabular-nums text-[var(--fg-faint)]"> {glance.running}</span></span>
      {glance.blocked > 0 && <Badge tone="warning">{tr(`막힘 ${glance.blocked}`, `Blocked ${glance.blocked}`)}</Badge>}
    </h3>
    {glance.active.length === 0 && <p className="px-2 text-xs text-[var(--fg-muted)]">{tr("지금 진행 중인 작업이 없습니다.", "Nothing is running right now.")}</p>}
    {/* Below the mobile boundary (sm) the glance leads with one task, above it with three; the rest stay one press away. */}
    <div className={`campaign-glance-cards grid min-w-0 gap-0.5 ${showAll ? "" : "max-sm:[&>*:not(:first-child)]:hidden"}`}>
      {(showAll ? active : active.slice(0, 3)).map((node) => row(node, true))}
    </div>
    {glance.active.length > 1 && <div className={`px-1 ${glance.active.length <= 3 ? "sm:hidden" : ""}`}>
      <Button variant="ghost" size="sm" aria-expanded={showAll} onClick={() => setShowAll((value) => !value)}>{showAll ? tr("요약만 보기", "Show highlights") : tr(`${glance.blocked ? "진행·막힘" : "진행 중"} ${glance.active.length}개 모두 보기`, `Show all ${glance.active.length} active tasks`)}</Button>
    </div>}
    {glance.buckets.length > 0 && <div className="campaign-glance-buckets flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5">
      <span className="px-2 text-xs text-[var(--fg-muted)]">{tr("나머지 작업", "Other tasks")}</span>
      {glance.buckets.map((bucket) => <TaskGroupHeading key={bucket.status} className="!w-auto" open={openStatus === bucket.status} controls={listId}
        icon={<StatusMarker {...nodeMarker(bucket.status)} />} label={tr(...LABELS[bucket.status])} count={bucket.nodes.length}
        onToggle={() => { setListAll(false); setOpenStatus((current) => current === bucket.status ? null : bucket.status); }} />)}
    </div>}
    {open && <div className="grid min-w-0 gap-1">
      <div id={listId} role="list" className="campaign-glance-list grid min-w-0 gap-0.5" aria-label={tr(...LABELS[open.status])}>
        {listed.map((node) => <div key={node.id} role="listitem" className="min-w-0">{row(node, false)}</div>)}
      </div>
      {open.nodes.length > LIST_PREVIEW && <div className="px-1"><Button variant="ghost" size="sm" aria-expanded={listAll} onClick={() => setListAll((value) => !value)}>{listAll ? tr("처음 몇 개만 보기", "Show fewer") : tr(`${open.nodes.length - LIST_PREVIEW}개 더 보기`, `Show ${open.nodes.length - LIST_PREVIEW} more`)}</Button></div>}
    </div>}
  </section>;
}
