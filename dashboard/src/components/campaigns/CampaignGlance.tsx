import { useMemo, useState } from "react";
import type { CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { CAMPAIGN_STAGES, campaignGlance, campaignStageStep, campaignTaskNumber, campaignTaskTitle } from "./campaignModel";
import { Badge, LABELS, LiveChip, type Tr } from "./campaignPresentation";

// Titles say what the task is; summaries are often lineage notes, so they stay secondary.
function gist(node: CampaignNode) {
  const text = campaignTaskTitle(node);
  const summary = node.summary?.trim();
  return { label: campaignTaskNumber(node), text, summary: summary && summary !== text ? summary : null };
}
const LIST_PREVIEW = 8;

function StageLabel({ node, tr }: { node: CampaignNode; tr: Tr }) {
  const step = campaignStageStep(node.stage);
  return step.index < 0 ? <span className="campaign-glance-raw" title={node.stage}>{step.raw}</span> : <span>{tr(...CAMPAIGN_STAGES[step.index].label)}</span>;
}

/** Plain-language first screen: technical fields stay in the task details. */
export default function CampaignGlance({ nodes, live = {}, tr, onOpen }: { nodes: CampaignNode[]; live?: Record<string, CampaignNodeLive>; tr: Tr; onOpen: (id: string) => void }) {
  const glance = useMemo(() => campaignGlance(nodes, live), [nodes, live]);
  const active = [...glance.active].sort((a, b) => Number(b.status === "blocked") - Number(a.status === "blocked"));
  const [showAll, setShowAll] = useState(false);
  const [openStatus, setOpenStatus] = useState<CampaignNodeStatus | null>(null);
  const [listAll, setListAll] = useState(false);
  const open = glance.buckets.find((bucket) => bucket.status === openStatus);
  const listed = open ? (listAll ? open.nodes : open.nodes.slice(0, LIST_PREVIEW)) : [];
  return <section className="campaign-glance grid gap-3 border-b border-th-border-subtle pb-5" aria-label={tr("한눈 요약", "At a glance")}>
    <h3>{tr(`지금 진행 중 ${glance.running}`, `Running now ${glance.running}`)}{glance.blocked > 0 && <span className="campaign-glance-blocked-count">{tr(` · 막힘 ${glance.blocked}`, ` · Blocked ${glance.blocked}`)}</span>}</h3>
    {glance.active.length === 0 && <p className="campaign-muted">{tr("지금 진행 중인 작업이 없습니다.", "Nothing is running right now.")}</p>}
    <div className={`campaign-glance-cards grid min-w-0 gap-2 lg:grid-cols-3 ${showAll ? "" : "max-sm:[&>button:not(:first-child)]:hidden"}`}>
      {(showAll ? active : active.slice(0, 3)).map((node) => {
        const { label, text, summary } = gist(node);
        const step = campaignStageStep(node.stage);
        return <button key={node.id} className={`campaign-glance-card grid min-w-0 content-start gap-2 border-th-border-subtle bg-th-bg-surface p-3 text-left ${node.blocker ? " is-blocked" : ""}`} data-glance-id={node.id} onClick={() => onOpen(node.id)}>
          <span className="campaign-glance-title flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-1 [&_strong]:basis-full [&_strong]:text-sm [&_strong]:font-medium [&_strong]:leading-6 [&_strong]:[overflow-wrap:anywhere] [&_small]:text-xs [&_small]:text-th-text-muted">{label && <small>{label}</small>}{node.status === "blocked" && <Badge status={node.status} tr={tr} />}<LiveChip live={live[node.id]} tr={tr} /><strong>{text}</strong></span>
          {summary && <span className="campaign-glance-summary line-clamp-2 text-xs leading-5 text-th-text-secondary [overflow-wrap:anywhere]">{summary}</span>}
          <span className="campaign-glance-stage">
            <span className="campaign-stage-bar" aria-hidden>{CAMPAIGN_STAGES.map((stage, index) => <i key={stage.key} className={index < step.index ? "done" : index === step.index ? "current" : undefined} />)}</span>
            <StageLabel node={node} tr={tr} />
          </span>
          {node.benefit?.trim() ? <span className="campaign-glance-benefit text-xs leading-5 text-th-text-muted [overflow-wrap:anywhere]">{tr("기대효과", "Benefit")}: {node.benefit}</span> : null}
          {node.blocker?.trim() && <span className="campaign-glance-blocker" role="note">{tr("막힘", "Blocked")}: {node.blocker}</span>}
        </button>;
      })}
    </div>
    {glance.active.length > 1 && <button type="button" className={`justify-self-start ${glance.active.length <= 3 ? "sm:hidden" : ""}`} aria-expanded={showAll} onClick={() => setShowAll((value) => !value)}>{showAll ? tr("요약만 보기", "Show highlights") : tr(`${glance.blocked ? "진행·막힘" : "진행 중"} ${glance.active.length}개 모두 보기`, `Show all ${glance.active.length} active tasks`)}</button>}
    {glance.buckets.length > 0 && <div className="campaign-glance-buckets items-center">
      <span className="text-xs text-th-text-muted">{tr("나머지 작업", "Other tasks")}</span>
      {glance.buckets.map((bucket) => <button key={bucket.status} aria-expanded={openStatus === bucket.status} onClick={() => { setListAll(false); setOpenStatus((current) => current === bucket.status ? null : bucket.status); }}>
        {tr(...LABELS[bucket.status])} <strong>{bucket.nodes.length}</strong>
      </button>)}
    </div>}
    {open && <div className="grid gap-2">
      <ul className="campaign-glance-list divide-y divide-th-border-subtle rounded-lg border border-th-border-subtle" aria-label={tr(...LABELS[open.status])}>
        {listed.map((node) => { const { label, text } = gist(node); return <li key={node.id}>
          <button className="grid w-full grid-cols-[auto_minmax(0,1fr)] items-baseline gap-x-2 gap-y-0.5 rounded-none border-0 bg-transparent px-3 py-2.5 text-left hover:bg-th-bg-surface" onClick={() => onOpen(node.id)}>
            <small className="text-xs tabular-nums text-th-text-muted">{label ?? "·"}</small>
            <span className="line-clamp-2 text-sm leading-5 text-th-text-primary [overflow-wrap:anywhere]">{text}</span>
            <span className="campaign-glance-list-stage col-start-2 min-w-0 truncate text-xs text-th-text-muted"><StageLabel node={node} tr={tr} /></span>
          </button>
        </li>; })}
      </ul>
      {open.nodes.length > LIST_PREVIEW && <button type="button" className="justify-self-start" aria-expanded={listAll} onClick={() => setListAll((value) => !value)}>{listAll ? tr("처음 몇 개만 보기", "Show fewer") : tr(`${open.nodes.length - LIST_PREVIEW}개 더 보기`, `Show ${open.nodes.length - LIST_PREVIEW} more`)}</button>}
    </div>}
  </section>;
}
