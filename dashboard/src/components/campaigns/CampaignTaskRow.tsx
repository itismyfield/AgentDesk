import type { Ref } from "react";
import type { CampaignNode, CampaignNodeLive } from "../../api/campaigns";
import { Badge } from "../../kit/free/badge-clean";
import { campaignTaskNumber, campaignTaskTitle } from "./campaignModel";
import { LABELS, LiveChip, type Tr } from "./campaignPresentation";
import { StatusMarker, nodeMarker } from "./StatusMarker";
import { TaskRow } from "./TaskListParts";

// The status word carries the marker's colour for the states that need someone.
const STATUS_TEXT: Record<string, string> = { blocked: "text-[var(--warn)]", failed: "text-[var(--err)]" };

/** One task: progress marker, number and title, then status and the ledger's stage note on one muted line; draft,
 * hidden-dependency, hold and live flags; assignee and round at the end. */
export default function CampaignTaskRow({ node, live, selected, draft, hiddenDependencies, controls, tr, rowRef, onSelect }: {
  node: CampaignNode; live: CampaignNodeLive | undefined; selected: boolean; draft: boolean; hiddenDependencies: number;
  /** The docked detail pane the row opens into, when there is one. */
  controls?: string; tr: Tr; rowRef: Ref<HTMLButtonElement>; onSelect: () => void;
}) {
  const stage = node.stage.trim();
  return <TaskRow buttonRef={rowRef} data-node-id={node.id} selected={selected} aria-current={selected || undefined} aria-controls={controls} onClick={onSelect}
    className="campaign-task-row" numberClassName="campaign-task-id" textClassName="campaign-task-title"
    marker={<StatusMarker {...nodeMarker(node.status, node.stage)} />} number={campaignTaskNumber(node)} title={campaignTaskTitle(node)} fullTitle={node.title}
    meta={<>
      <span className={STATUS_TEXT[node.status] ?? ""}>{LABELS[node.status] ? tr(...LABELS[node.status]) : node.status}</span>
      {stage && <> · <span className="campaign-task-stage" title={stage}>{stage}</span></>}
    </>}
    flags={<>
      {draft && <span className="shrink-0 text-xs text-[var(--accent)]" title={tr("수정 중인 초안", "Unsaved draft")}>*</span>}
      {hiddenDependencies > 0 && <span className="shrink-0 text-xs tabular-nums text-[var(--fg-faint)]" title={tr("필터 밖 선행 작업", "Dependencies outside filters")}>↳ {hiddenDependencies}</span>}
      {node.post_merge_hold && <Badge tone="warning" title={node.post_merge_hold}>{tr("보류: 머지 뒤 남은 일", "On hold: post-merge work")}</Badge>}
      <LiveChip live={live} tr={tr} />
    </>}
    trailing={<span className="flex shrink-0 items-baseline gap-2 text-xs tabular-nums text-[var(--fg-faint)]">
      {node.assignee && <span className="max-w-[8rem] truncate @max-[48rem]/tasks:hidden" title={node.assignee}>{node.assignee}</span>}
      <span title={tr(`회차 ${node.round}`, `Round ${node.round}`)}>R{node.round}</span>
    </span>} />;
}
