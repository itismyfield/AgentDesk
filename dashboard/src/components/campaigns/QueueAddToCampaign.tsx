import { useState } from "react";
import { addCampaignNode, getCampaign, type Campaign } from "../../api/campaigns";
import type { KanbanCard } from "../../types";
import { buildGitHubIssueUrl } from "../agent-manager/kanban-utils";
import { Select } from "../common/Select";
import type { Tr } from "./campaignPresentation";
import { campaignNodeFromCard } from "./campaignQueueModel";

const OPEN = new Set<Campaign["status"]>(["planned", "active", "paused"]);

/** Put the card's issue into a campaign as a new pending task. */
export default function QueueAddToCampaign({ card, campaigns, tr, onAdded }: {
  card: KanbanCard; campaigns: Campaign[]; tr: Tr; onAdded: (campaign: Campaign) => void;
}) {
  const open = campaigns.filter((campaign) => OPEN.has(campaign.status));
  const [target, setTarget] = useState(() => open.find((campaign) => campaign.status === "active")?.id ?? open[0]?.id ?? "");
  const [state, setState] = useState<{ kind: "idle" | "saving" | "done" | "error"; text?: string }>({ kind: "idle" });
  const issueUrl = buildGitHubIssueUrl(card.github_repo, card.github_issue_number, card.github_issue_url);
  const chosen = open.find((campaign) => campaign.id === target);
  if (!open.length) return null;
  const add = async () => {
    if (!chosen || !issueUrl || !card.github_issue_number) return;
    setState({ kind: "saving" });
    try {
      // The newest revision, so a task added elsewhere meanwhile is kept.
      const latest = await getCampaign(chosen.id);
      const node = campaignNodeFromCard({ ...card, github_issue_number: card.github_issue_number }, latest, issueUrl, tr("대기열에서 추가", "Added from the queue"), new Date().toISOString());
      const saved = await addCampaignNode(latest, node);
      onAdded(saved);
      setState({ kind: "done", text: tr(`'${saved.title}'에 넣었습니다.`, `Added to '${saved.title}'.`) });
    } catch (cause) {
      setState({ kind: "error", text: cause instanceof Error ? cause.message : tr("캠페인에 넣지 못했습니다.", "Could not add it to the campaign.") });
    }
  };
  return <section aria-label={tr("캠페인에 넣기", "Put into a campaign")} className="grid gap-2 rounded-lg border border-th-border-subtle p-3">
    <h4 className="text-sm font-medium text-th-text-heading">{tr("캠페인에 넣기", "Put into a campaign")}</h4>
    {issueUrl ? <>
      <div className="flex flex-wrap items-center gap-2">
        <Select aria-label={tr("넣을 캠페인", "Campaign to add to")} wrapperClassName="min-w-0 flex-1 basis-56" value={target} onChange={(event) => setTarget(event.target.value)}>
          {open.map((campaign) => <option key={campaign.id} value={campaign.id}>{campaign.title}</option>)}
        </Select>
        <button type="button" disabled={!chosen || state.kind === "saving" || state.kind === "done"} onClick={() => void add()}
          className="min-h-11 shrink-0 rounded-md border border-th-border-subtle px-4 text-sm font-medium text-th-text-primary disabled:opacity-50">
          {state.kind === "saving" ? tr("넣는 중…", "Adding…") : tr("넣기", "Add")}
        </button>
      </div>
      {chosen?.auto_queue && chosen.status === "active" && <p className="text-xs text-th-text-muted">{tr("이 캠페인은 자동 진행이 켜져 있어, 선행 작업이 없으면 바로 실행될 수 있습니다.", "Auto-run is on for this campaign, so the task may start right away.")}</p>}
      {state.text && <p role={state.kind === "error" ? "alert" : "status"} className={`text-xs ${state.kind === "error" ? "text-[var(--th-accent-danger)]" : "text-th-text-secondary"}`}>{state.text}</p>}
    </> : <p className="text-xs text-th-text-muted">{tr("GitHub 이슈가 연결된 카드만 캠페인에 넣을 수 있습니다.", "Only cards linked to a GitHub issue can go into a campaign.")}</p>}
  </section>;
}
