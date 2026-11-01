import { useCallback, useEffect, useRef, useState } from "react";
import { ApiRequestError } from "../../api/httpClient";
import {
  getServerCampaign, getServerCampaignEvents, getServerCampaignSummaries,
  type ServerCampaignDocument, type ServerCampaignEvent, type ServerCampaignSummary,
} from "../../api/serverCampaigns";

export type ServerCampaignDetailState = { doc: ServerCampaignDocument | null; loading: boolean; error: string | null };
/** `read` names the campaign and last event a list answers. A list is null until one read succeeds;
 * a failed read keeps the last list and records the detail read it failed after. */
export type ServerCampaignEvents = { campaign: string; read: string; events: ServerCampaignEvent[] | null; failedAfter: number | null };
type ReadKind = "list" | "detail";

const message = (cause: unknown, fallback: string) => cause instanceof Error ? cause.message : fallback;

/** Server campaign reads: summaries for choosing a campaign, and the full document of the one on screen. */
export function useServerCampaignReads(selected: string | null) {
  const [summaries, setSummaries] = useState<ServerCampaignSummary[]>([]);
  const [listLoaded, setListLoaded] = useState(false);
  const [listError, setListError] = useState<string | null>(null);
  const [details, setDetails] = useState<Record<string, ServerCampaignDetailState>>({});
  // A read stayed unanswered through a whole poll slot.
  const [silent, setSilent] = useState(false);
  const [events, setEvents] = useState<ServerCampaignEvents | null>(null);
  const [detailReads, setDetailReads] = useState(0);
  const detailReadCount = useRef(0);
  const mounted = useRef(true);
  const readId = useRef(0);
  const listRequest = useRef(0);
  const detailRequest = useRef<Record<string, number>>({});
  const eventsRequest = useRef(0);
  const eventsPending = useRef<string | null>(null);
  // `ticks`: poll slots this read (or the ones it replaced) has waited through unanswered.
  const pending = useRef<Record<ReadKind, { id: number; ticks: number } | null>>({ list: null, detail: null });
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; listRequest.current++; detailRequest.current = {}; eventsRequest.current++; };
  }, []);

  const updateSilent = () => setSilent(Object.values(pending.current).some((read) => read !== null && read.ticks >= 2));
  // A read on the poll schedule starts a slot; one the user starts takes over the count of the read it replaces.
  const begin = (kind: ReadKind, onSchedule: boolean) => {
    const id = ++readId.current;
    pending.current[kind] = { id, ticks: onSchedule ? 1 : pending.current[kind]?.ticks ?? 0 };
    return id;
  };
  const end = (kind: ReadKind, id: number) => {
    if (pending.current[kind]?.id === id) pending.current[kind] = null;
    if (mounted.current) updateSilent();
  };

  const refreshList = useCallback(async (onSchedule = false) => {
    const id = ++listRequest.current;
    const read = begin("list", onSchedule);
    try {
      const values = await getServerCampaignSummaries();
      if (!mounted.current || id !== listRequest.current) return;
      setSummaries(values); setListError(null);
    } catch (cause) {
      if (mounted.current && id === listRequest.current) setListError(message(cause, "Unable to load server campaigns"));
    } finally {
      end("list", read);
      if (mounted.current && id === listRequest.current) setListLoaded(true);
    }
  }, []);

  const refreshDetail = useCallback(async (campaign: string, onSchedule = false) => {
    const id = (detailRequest.current[campaign] ?? 0) + 1;
    detailRequest.current[campaign] = id;
    const read = begin("detail", onSchedule);
    const settle = (next: { doc?: ServerCampaignDocument; error: string | null; notFound?: boolean }) => {
      if (!mounted.current || id !== detailRequest.current[campaign]) return;
      setDetails((current) => {
        const doc = next.doc ?? current[campaign]?.doc ?? null;
        // A 404 before any snapshot means the campaign does not exist; after one, the shown snapshot is stale.
        return { ...current, [campaign]: { doc, loading: false, error: next.notFound && !doc ? null : next.error } };
      });
      if (next.doc) setDetailReads(++detailReadCount.current);
    };
    setDetails((current) => ({ ...current, [campaign]: { doc: current[campaign]?.doc ?? null, loading: true, error: current[campaign]?.error ?? null } }));
    try {
      settle({ doc: await getServerCampaign(campaign), error: null });
    } catch (cause) {
      settle({ error: message(cause, "Unable to load the server campaign"), notFound: cause instanceof ApiRequestError && cause.status === 404 });
    } finally { end("detail", read); }
  }, []);

  useEffect(() => { if (selected) void refreshDetail(selected); }, [selected, refreshDetail]);

  /** One poll slot passed. Slots count while the page is hidden too, so a hung read still marks the snapshot stale. */
  const tick = useCallback((visible: boolean) => {
    const waiting = pending.current.detail;
    for (const read of Object.values(pending.current)) if (read) read.ticks++;
    updateSilent();
    // An unanswered read is waited out rather than superseded, so its failure still lands.
    if (visible && selected && !waiting) void refreshDetail(selected, true);
  }, [selected, refreshDetail]);
  /** The page became visible again: read the shown campaign now unless a read is still out. */
  const resume = useCallback(() => {
    if (selected && !pending.current.detail) void refreshDetail(selected);
  }, [selected, refreshDetail]);

  const detail = selected ? details[selected] : undefined;
  const doc = detail?.doc ?? undefined;
  const shownCampaign = doc?.view.campaign_id;
  const lastEvent = doc?.view.last_event_sequence;
  // Events are read when the campaign or its last event moves; after a failed read, each later detail read retries it.
  const eventsRetry = events !== null && events.campaign === shownCampaign && events.failedAfter !== null ? detailReads - events.failedAfter : 0;
  useEffect(() => {
    if (!shownCampaign || lastEvent === undefined) return;
    const read = `${shownCampaign}\n${lastEvent}`;
    if (eventsPending.current === read) return; // it will answer; request() always settles
    const id = ++eventsRequest.current; // a read for anything else is no longer wanted
    eventsPending.current = null;
    // A good read stands; a failed one waits for a detail read that lands after it.
    if (events?.read === read && (events.failedAfter === null || events.failedAfter >= detailReadCount.current)) return;
    eventsPending.current = read;
    const campaign = shownCampaign;
    getServerCampaignEvents(campaign, { after: Math.max(0, lastEvent - 50), limit: 50 }).then(
      (list) => {
        if (!mounted.current || id !== eventsRequest.current) return;
        eventsPending.current = null;
        setEvents({ campaign, read, events: list, failedAfter: null });
      },
      () => {
        if (!mounted.current || id !== eventsRequest.current) return;
        eventsPending.current = null;
        // Keep the last list, marked, rather than an old timeline that looks current.
        setEvents((current) => ({ campaign, read, events: current?.campaign === campaign ? current.events : null, failedAfter: detailReadCount.current }));
      },
    );
  }, [shownCampaign, lastEvent, eventsRetry]); // eslint-disable-line react-hooks/exhaustive-deps -- `events` is read, not watched

  return { summaries, listLoaded, listError, details, detail, doc, silent, events, refreshList, refreshDetail, tick, resume };
}
