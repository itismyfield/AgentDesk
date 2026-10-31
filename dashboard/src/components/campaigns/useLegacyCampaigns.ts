import { useCallback, useEffect, useRef, useState } from "react";
import { getCampaignDetail, getCampaigns, type Campaign, type CampaignLive } from "../../api/campaigns";
import { campaignRawEdges } from "./campaignGraphRouting";
import { groupViewScope, pruneStoredView, storedGroupViewCampaigns } from "./campaignViewStorage";

/** Never let an older read replace a revision this screen already holds. */
function newest(current: Campaign[], values: Campaign[]): Campaign[] {
  return values.map((value) => {
    const previous = current.find((candidate) => candidate.id === value.id);
    return previous && previous.revision > value.revision ? previous : value;
  });
}

/** Ledger campaigns: the whole list once (and on refresh), then only the shown campaign on each poll. */
export function useLegacyCampaigns() {
  const [campaigns, setCampaigns] = useState<Campaign[]>([]);
  const [live, setLive] = useState<CampaignLive>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshedAt, setRefreshedAt] = useState<number | null>(null);
  const [checkedRead, setCheckedRead] = useState<number | null>(null);
  const mounted = useRef(true);
  const listRequest = useRef(0);
  const detailRequest = useRef(0);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; listRequest.current++; detailRequest.current++; };
  }, []);

  // Saved tie layouts follow every loaded definition, even while no graph is open, a campaign has no tasks or
  // the campaign is gone (an id created again must not bring old ties back). Only after a successful load.
  useEffect(() => {
    if (refreshedAt === null) return;
    const loaded = new Map(campaigns.map((value) => [value.id, value]));
    for (const id of new Set([...loaded.keys(), ...storedGroupViewCampaigns()])) {
      const shown = loaded.get(id);
      pruneStoredView(groupViewScope(id), shown ? campaignRawEdges(shown.nodes) : [], shown?.revision ?? null);
    }
  }, [campaigns, refreshedAt]);

  const refresh = useCallback(async () => {
    const id = ++listRequest.current;
    setLoading(true);
    try {
      const { campaigns: values, live: nextLive, read } = await getCampaigns();
      if (!mounted.current || id !== listRequest.current) return;
      setLive(nextLive);
      setCampaigns((current) => newest(current, values));
      setError(null); setRefreshedAt(Date.now()); setCheckedRead(read);
    } catch (cause) {
      if (mounted.current && id === listRequest.current) setError(cause instanceof Error ? cause.message : "Unable to load campaigns");
    } finally { if (mounted.current && id === listRequest.current) setLoading(false); }
  }, []);

  const refreshCampaign = useCallback(async (campaignId: string) => {
    const id = ++detailRequest.current;
    try {
      const { campaign, live: nodeLive, read } = await getCampaignDetail(campaignId);
      if (!mounted.current || id !== detailRequest.current) return;
      setLive((current) => ({ ...current, [campaignId]: nodeLive }));
      setCampaigns((current) => current.map((value) => value.id === campaignId ? newest([value], [campaign])[0] : value));
      setError(null); setRefreshedAt(Date.now()); setCheckedRead(read);
    } catch (cause) {
      if (mounted.current && id === detailRequest.current) setError(cause instanceof Error ? cause.message : "Unable to load campaigns");
    }
  }, []);

  const onSaved = useCallback((updated: Campaign) => {
    // A read started before this save must not replace its result.
    listRequest.current++; detailRequest.current++;
    setLoading(false); setError(null); setRefreshedAt(Date.now());
    setCampaigns((current) => current.map((value) => value.id === updated.id && updated.revision >= value.revision ? updated : value));
  }, []);

  return { campaigns, live, loading, error, refreshedAt, checkedRead, refresh, refreshCampaign, onSaved };
}
