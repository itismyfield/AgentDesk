import type { ServerCampaignDocument } from "../../api/serverCampaigns";
import type { Tr } from "./campaignPresentation";
import { displayText } from "./serverCampaignModel";

/** Server campaigns with what the server reported for each: no client-side status. */
export default function ServerCampaignList({ documents, selectedId, onSelect, tr }: {
  documents: ServerCampaignDocument[]; selectedId: string | null; onSelect: (id: string) => void; tr: Tr;
}) {
  return <ul className="campaign-server-list" aria-label={tr("서버 캠페인 목록", "Server campaigns")}>
    {documents.map(({ definition, view }) => <li key={view.campaign_id}>
      <button type="button" aria-pressed={view.campaign_id === selectedId} onClick={() => onSelect(view.campaign_id)}>
        <strong>{definition.title}</strong>
        <span className={`campaign-badge campaign-status-${view.display}`}>{displayText(view, tr)}</span>
        <small>{definition.repository === "adk" ? "AgentDesk" : "CookingHeart"} · {definition.goal}</small>
        <small>{tr("서버 관측", "Server as of")} {new Date(view.as_of).toLocaleString()}</small>
      </button>
    </li>)}
  </ul>;
}
