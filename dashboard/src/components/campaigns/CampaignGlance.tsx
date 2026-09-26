import { useMemo, useState } from "react";
import type { CampaignNode, CampaignNodeStatus } from "../../api/campaigns";
import { CAMPAIGN_STAGES, campaignGlance, campaignIssueLabel, campaignStageStep } from "./campaignModel";
import { Badge, LABELS, type Tr } from "./campaignPresentation";

function gist(node: CampaignNode) {
  const label = campaignIssueLabel(node);
  const title = node.title.startsWith(label) ? node.title.slice(label.length).trim() : node.title;
  return { label, text: node.summary?.trim() || title || node.title };
}

function StageLabel({ node, tr }: { node: CampaignNode; tr: Tr }) {
  const step = campaignStageStep(node.stage);
  return step.index < 0 ? <span className="campaign-glance-raw" title={node.stage}>{step.raw}</span> : <span>{tr(...CAMPAIGN_STAGES[step.index].label)}</span>;
}

/** Plain-language first screen: technical fields stay in the task details. */
export default function CampaignGlance({ nodes, tr, onOpen }: { nodes: CampaignNode[]; tr: Tr; onOpen: (id: string) => void }) {
  const glance = useMemo(() => campaignGlance(nodes), [nodes]);
  const [openStatus, setOpenStatus] = useState<CampaignNodeStatus | null>(null);
  const open = glance.buckets.find((bucket) => bucket.status === openStatus);
  return <section className="campaign-glance" aria-label={tr("한눈 요약", "At a glance")}>
    <h3>{tr(`지금 진행 중 ${glance.running}`, `Running now ${glance.running}`)}</h3>
    {glance.active.length === 0 && <p className="campaign-muted">{tr("지금 진행 중인 작업이 없습니다.", "Nothing is running right now.")}</p>}
    {glance.buckets.length > 0 && <div className="campaign-glance-buckets">
      {glance.buckets.map((bucket) => <button key={bucket.status} aria-expanded={openStatus === bucket.status} onClick={() => setOpenStatus((current) => current === bucket.status ? null : bucket.status)}>
        {tr(...LABELS[bucket.status])} <strong>{bucket.nodes.length}</strong>
      </button>)}
    </div>}
    {open && <ul className="campaign-glance-list" aria-label={tr(...LABELS[open.status])}>
      {open.nodes.map((node) => { const { label, text } = gist(node); return <li key={node.id}><button onClick={() => onOpen(node.id)}><small>{label}</small><span>{text}</span><StageLabel node={node} tr={tr} /></button></li>; })}
    </ul>}
    <div className="campaign-glance-cards">
      {glance.active.map((node) => {
        const { label, text } = gist(node);
        const step = campaignStageStep(node.stage);
        return <button key={node.id} className={`campaign-glance-card${node.blocker ? " is-blocked" : ""}`} data-glance-id={node.id} onClick={() => onOpen(node.id)}>
          <span className="campaign-glance-title"><small>{label}</small>{node.status !== "running" && <Badge status={node.status} tr={tr} />}<strong>{text}</strong></span>
          <span className="campaign-glance-stage">
            <span className="campaign-stage-bar" aria-hidden>{CAMPAIGN_STAGES.map((stage, index) => <i key={stage.key} className={index < step.index ? "done" : index === step.index ? "current" : undefined} />)}</span>
            <StageLabel node={node} tr={tr} />
          </span>
          {node.benefit?.trim() ? <span className="campaign-glance-benefit">{tr("기대효과", "Benefit")}: {node.benefit}</span> : <span className="campaign-glance-benefit campaign-muted">{tr("기대효과 미기록", "No benefit recorded")}</span>}
          {node.blocker?.trim() && <span className="campaign-glance-blocker" role="note">{tr("막힘", "Blocked")}: {node.blocker}</span>}
        </button>;
      })}
    </div>
  </section>;
}
