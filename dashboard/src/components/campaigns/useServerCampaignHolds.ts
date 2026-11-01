import { useEffect, useState } from "react";
import { getServerCampaign, type ServerCampaignSummary } from "../../api/serverCampaigns";

export type ServerCampaignHolds = { issueUrls: string[]; loaded: boolean; failed: number };

const NONE: ServerCampaignHolds = { issueUrls: [], loaded: false, failed: 0 };

/** Issue links that server campaigns not yet succeeded hold. Summaries carry no nodes, so each such
 * campaign's document is read while `active`; repeat reads revalidate by ETag. */
export function useServerCampaignHolds(summaries: ServerCampaignSummary[], listLoaded: boolean, active: boolean): ServerCampaignHolds {
  const [holds, setHolds] = useState<ServerCampaignHolds>(NONE);
  useEffect(() => {
    if (!active || !listLoaded) return;
    const open = summaries.filter((summary) => summary.view.display !== "succeeded");
    let current = true;
    void Promise.allSettled(open.map((summary) => getServerCampaign(summary.view.campaign_id))).then((results) => {
      if (!current) return;
      setHolds({
        issueUrls: results.flatMap((result) => result.status === "fulfilled" ? result.value.definition.nodes.map((node) => node.issue_url) : []),
        loaded: true,
        failed: results.filter((result) => result.status === "rejected").length,
      });
    });
    return () => { current = false; };
  }, [active, listLoaded, summaries]);
  return holds;
}
