import { useEffect, useState } from "react";
import { getServerCampaign, type ServerCampaignSummary } from "../../api/serverCampaigns";

export type ServerCampaignHolds = { issueUrls: string[]; loaded: boolean; failed: number };

type LoadedHolds = { key: string | null; issueUrls: string[]; failed: number };
const NONE: LoadedHolds = { key: null, issueUrls: [], failed: 0 };

/** Issue links that server campaigns not yet succeeded hold. Summaries carry no nodes, so each such
 * campaign's document is read while `active`; repeat reads revalidate by ETag. */
export function useServerCampaignHolds(summaries: ServerCampaignSummary[], listLoaded: boolean, active: boolean): ServerCampaignHolds {
  const [holds, setHolds] = useState<LoadedHolds>(NONE);
  const openIds = listLoaded
    ? summaries.filter((summary) => summary.view.display !== "succeeded").map((summary) => summary.view.campaign_id)
    : null;
  const targetKey = openIds ? [...openIds].sort().join("\n") : null;
  useEffect(() => {
    if (!active || !openIds || openIds.length === 0 || targetKey === null) return;
    let current = true;
    void Promise.allSettled(openIds.map((id) => getServerCampaign(id))).then((results) => {
      if (!current) return;
      setHolds({
        key: targetKey,
        issueUrls: results.flatMap((result) => result.status === "fulfilled" ? result.value.definition.nodes.map((node) => node.issue_url) : []),
        failed: results.filter((result) => result.status === "rejected").length,
      });
    });
    return () => { current = false; };
  }, [active, targetKey]); // eslint-disable-line react-hooks/exhaustive-deps -- targetKey canonically represents openIds
  if (!active || targetKey === null) return { issueUrls: [], loaded: false, failed: 0 };
  if (targetKey === "") return { issueUrls: [], loaded: true, failed: 0 };
  const loaded = holds.key === targetKey;
  return { issueUrls: loaded ? holds.issueUrls : [], loaded, failed: loaded ? holds.failed : 0 };
}
