import type { ServerCampaignSummary } from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";
import { displayText } from "./serverCampaignModel";

/** Server campaigns with what the server reported for each: no client-side status. */
export default function ServerCampaignList({ summaries, selectedId, onSelect, tr }: {
  summaries: ServerCampaignSummary[]; selectedId: string | null; onSelect: (id: string) => void; tr: Tr;
}) {
  return <details className="rounded-lg border border-th-border-subtle px-3 text-xs"><summary className="cursor-pointer text-th-text-muted">{tr(`서버 캠페인 ${summaries.length}개`, `${summaries.length} server campaigns`)}</summary><ul className="campaign-server-list grid min-w-0 gap-2 sm:grid-cols-2 xl:grid-cols-3" aria-label={tr("서버 캠페인 목록", "Server campaigns")}>
    {summaries.map(({ definition, view }) => <li key={view.campaign_id}>
      <button type="button" aria-pressed={view.campaign_id === selectedId} onClick={() => onSelect(view.campaign_id)}>
        <strong>{definition.title}</strong>
        <span className={`campaign-badge campaign-status-${view.display}`}>{displayText(view, tr)}</span>
        <small>{definition.repository === "adk" ? "AgentDesk" : "CookingHeart"} · {definition.goal}</small>
        <small>{tr("서버 관측", "Server as of")} {new Date(view.as_of).toLocaleString()}</small>
      </button>
    </li>)}
  </ul></details>;
}
