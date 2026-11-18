import type { KeyboardEvent, Ref } from "react";
import type { CampaignNode, CampaignNodeLive } from "../../api/campaigns";
import { campaignTaskNumber, campaignTaskTitle } from "./campaignModel";
import { Badge, type BadgeTone } from "../../kit/free/badge-clean";
import { LABELS, LiveChip, type Tr } from "./campaignPresentation";
import { StatusMarker, nodeMarker } from "./StatusMarker";

const STATUS_TONE: Record<string, BadgeTone> = { running: "info", blocked: "warning", failed: "danger", completed: "success" };

export const TASK_COLUMNS = "xl:grid-cols-[minmax(0,1fr)_minmax(96px,auto)_32px_88px]";

/** One task: progress marker, number and title, the stage note as one grey line, then status, round and assignee. */
export default function CampaignTaskRow({ node, live, selected, draft, hiddenDependencies, tr, rowRef, onSelect, onKeyDown }: {
  node: CampaignNode; live: CampaignNodeLive | undefined; selected: boolean; draft: boolean; hiddenDependencies: number; tr: Tr;
  rowRef: Ref<HTMLButtonElement>; onSelect: () => void; onKeyDown: (event: KeyboardEvent) => void;
}) {
  const number = campaignTaskNumber(node);
  return <button ref={rowRef} type="button" data-node-id={node.id} aria-pressed={selected} onClick={onSelect} onKeyDown={onKeyDown}
    className={`campaign-task-row grid w-full grid-cols-[minmax(0,1fr)] items-start gap-x-3 gap-y-1.5 rounded-none border-0 border-t border-th-border-subtle bg-transparent px-3 py-3 text-left ${TASK_COLUMNS}`}>
    <span className="campaign-task-title grid min-w-0 grid-cols-[14px_minmax(0,1fr)] items-start gap-x-2.5 gap-y-0.5">
      <StatusMarker {...nodeMarker(node.status)} className="mt-[5px]" />
      <span className="flex min-w-0 items-baseline gap-2">
        {number && <span className="campaign-task-id shrink-0 text-xs tabular-nums text-th-text-muted">{number}</span>}
        <strong className="min-w-0 text-sm font-medium leading-6 [overflow-wrap:anywhere]" title={node.title}>{campaignTaskTitle(node)}</strong>
        {draft && <small title={tr("수정 중인 초안", "Unsaved draft")}>*</small>}
        {hiddenDependencies > 0 && <small className="shrink-0 text-xs text-th-text-muted" title={tr("필터 밖 선행 작업", "Dependencies outside filters")}>↳ {hiddenDependencies}</small>}
      </span>
      <span className="campaign-task-stage col-start-2 block min-w-0 truncate text-xs text-th-text-muted" title={node.stage}>{node.stage}</span>
    </span>
    <span className="campaign-task-meta flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs text-th-text-muted xl:contents">
      <span className="campaign-task-status flex min-w-0 flex-wrap items-start gap-1">
        <Badge tone={STATUS_TONE[node.status] ?? "neutral"} live={node.status === "running"} dot={node.status !== "running"}>{LABELS[node.status] ? tr(...LABELS[node.status]) : node.status}</Badge>
        {node.post_merge_hold && <Badge tone="warning" title={node.post_merge_hold}>{tr("보류: 머지 뒤 남은 일", "On hold: post-merge work")}</Badge>}
        <LiveChip live={live} tr={tr} />
      </span>
      <span className="campaign-task-round tabular-nums">R{node.round}</span>
      <span className="campaign-task-owner min-w-0 truncate" title={node.assignee || ""}>{node.assignee || "—"}</span>
    </span>
  </button>;
}
