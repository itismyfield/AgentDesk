import { useCallback, useEffect, useRef, useState } from "react";
import { getCampaigns, type Campaign, type CampaignLive, type CampaignNodeLive } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import {
  getServerCampaign, getServerCampaignEvents, getServerCampaigns, isDefinitiveAnswer, sendServerCampaignCommand, serverCampaignRejection,
  type ServerCampaignCommand, type ServerCampaignDocument, type ServerCampaignEvent,
} from "../../api/serverCampaigns";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue, writeLocalStorageValue } from "../../lib/useLocalStorage";
import { WidgetState } from "../common/WidgetState";
import CampaignAutoQueueToggle from "./CampaignAutoQueueToggle";
import { campaignProgress } from "./campaignModel";
import { Badge, type Tr } from "./campaignPresentation";
import CampaignExplorer from "./CampaignExplorer";
import type { CampaignDraft } from "./CampaignNodeDetails";
import type { ControlCommand } from "./ServerCampaignControls";
import ServerCampaignDetail from "./ServerCampaignDetail";
import ServerCampaignForm, { createDraft, editDraft, isUntouchedCreate, type DefinitionDraft } from "./ServerCampaignForm";
import ServerCampaignList from "./ServerCampaignList";
import {
  campaignLocationQuery, commandAvailability, newIdempotencyKey, parseCampaignLocation, reconcileStop, rejectionText, stopRequest,
  type CampaignLocation, type StopIntent,
} from "./serverCampaignModel";
import "./campaigns.css";

const NO_LIVE: Record<string, CampaignNodeLive> = {};
const SERVER_OPTION = "server:";
// Disjoint from edit keys, so a campaign named "new" never shares the create draft.
const NEW_DRAFT = "create";
const editKey = (id: string) => `edit:${id}`;
const POLL_MS = 15_000;
// Route paths match without regard to a trailing slash or case.
const samePage = (a: string, b: string) => a.replace(/\/+$/, "").toLowerCase() === b.replace(/\/+$/, "").toLowerCase();

export default function CampaignsPanel({ language }: { language: string }) {
  const tr: Tr = useCallback((ko, en) => language === "ko" ? ko : en, [language]);
  // A link names the mode, campaign and task; without one the legacy selection is restored.
  const [linked] = useState(() => parseCampaignLocation(window.location.search));
  const [campaigns, setCampaigns] = useState<Campaign[]>([]);
  const [live, setLive] = useState<CampaignLive>({});
  const [drafts, setDrafts] = useState<Record<string, CampaignDraft>>({});
  const onDraftChange = (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => setDrafts((current) => {
    // A save may finish after this task was reopened and its draft changed.
    if (expected && current[key] !== expected) return current;
    const next = { ...current };
    if (draft) next[key] = draft; else delete next[key];
    return next;
  });
  const [selectedId, setSelectedId] = useState<string | null>(() => linked?.mode === "legacy" ? linked.campaign : readLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, null, { validate: (value): value is string | null => value === null || typeof value === "string" }));
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshedAt, setRefreshedAt] = useState<number | null>(null);
  const [checkedRead, setCheckedRead] = useState<number | null>(null);
  const mounted = useRef(false);
  const requestId = useRef(0);
  const refresh = useCallback(async () => {
    const id = ++requestId.current;
    setLoading(true);
    try {
      const { campaigns: values, live: nextLive, read } = await getCampaigns();
      if (!mounted.current || id !== requestId.current) return;
      setLive(nextLive);
      setCampaigns((current) => values.map((value) => {
        const previous = current.find((candidate) => candidate.id === value.id);
        return previous && previous.revision > value.revision ? previous : value;
      })); setError(null); setRefreshedAt(Date.now()); setCheckedRead(read);
      setSelectedId((current) => values.some((value) => value.id === current) ? current : values.find((value) => value.status === "active")?.id ?? values[0]?.id ?? null);
    } catch (cause) {
      if (mounted.current && id === requestId.current) setError(cause instanceof Error ? cause.message : "Unable to load campaigns");
    } finally { if (mounted.current && id === requestId.current) setLoading(false); }
  }, []);

  const [mode, setMode] = useState<CampaignLocation["mode"]>(linked?.mode ?? "legacy");
  const [serverSelected, setServerSelected] = useState<string | null>(linked?.mode === "server" ? linked.campaign : null);
  const [node, setNode] = useState<string | null>(linked?.node ?? null);
  // Server campaigns poll as whole documents; the selected detail is read from that list.
  const [serverDocs, setServerDocs] = useState<ServerCampaignDocument[]>([]);
  const [serverLoaded, setServerLoaded] = useState(false);
  const [serverError, setServerError] = useState<string | null>(null);
  // The previous poll was still unanswered when the next one was due.
  const [serverSilent, setServerSilent] = useState(false);
  // The selected campaign read on its own when the list does not hold it.
  const [outside, setOutside] = useState<{ id: string; doc: ServerCampaignDocument | null; failed: boolean; missing: boolean } | null>(null);
  // `read` names the campaign and last event a list answers. A list is null until one read succeeds;
  // a failed read keeps the last list and records the list read it failed after.
  const [events, setEvents] = useState<{ campaign: string; read: string; events: ServerCampaignEvent[] | null; failedAfter: number | null } | null>(null);
  const [serverPolls, setServerPolls] = useState(0);
  const serverPollCount = useRef(0);
  const eventsPending = useRef<string | null>(null);
  const [serverDrafts, setServerDrafts] = useState<Record<string, DefinitionDraft>>({});
  // The form the user just opened or filled takes focus once; showing it again later does not.
  const focusRequest = useRef<{ key: string; n: number } | null>(null);
  const focusCount = useRef(0);
  const [stops, setStops] = useState<Record<string, StopIntent>>({});
  // A command's progress and refusal belong to the campaign it was sent for.
  const [busy, setBusy] = useState<ReadonlySet<string>>(new Set());
  const [commandError, setCommandError] = useState<{ campaign: string; text: string } | null>(null);
  const serverRequest = useRef(0);
  // `ticks`: poll slots this read (or the ones it replaced) has waited through unanswered.
  const serverPending = useRef<{ id: number; ticks: number } | null>(null);
  const detailRequest = useRef(0);
  const eventsRequest = useRef(0);
  const refreshServer = useCallback(async (onSchedule = false) => {
    const id = ++serverRequest.current;
    // A read on the poll schedule starts a slot; a read the user starts takes over the count of the one it
    // replaces, so refreshing never hides a hung server.
    serverPending.current = { id, ticks: onSchedule ? 1 : serverPending.current?.ticks ?? 0 };
    try {
      const documents = await getServerCampaigns();
      if (!mounted.current || id !== serverRequest.current) return;
      setServerDocs(documents); setServerError(null); setServerPolls(++serverPollCount.current);
    } catch (cause) {
      if (mounted.current && id === serverRequest.current) setServerError(cause instanceof Error ? cause.message : "Unable to load server campaigns");
    } finally {
      if (serverPending.current?.id === id) serverPending.current = null;
      if (mounted.current && id === serverRequest.current) { setServerLoaded(true); setServerSilent(false); }
    }
  }, []);
  const refreshAll = useCallback(() => { void refresh(); void refreshServer(); }, [refresh, refreshServer]);
  useEffect(() => {
    mounted.current = true;
    void refresh(); void refreshServer(true);
    const interval = window.setInterval(() => {
      // Slots count while the tab is hidden too: a read unanswered past a whole slot marks the snapshot stale.
      const pending = serverPending.current;
      if (pending && ++pending.ticks >= 2) setServerSilent(true);
      if (document.visibilityState !== "visible") return;
      void refresh();
      // An unanswered read is waited out rather than superseded, so its failure still lands.
      if (!pending) void refreshServer(true);
    }, POLL_MS);
    return () => { mounted.current = false; requestId.current++; serverRequest.current++; window.clearInterval(interval); };
  }, [refresh, refreshServer]);
  useEffect(() => { writeLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, selectedId); }, [selectedId]);

  const listed = serverDocs.find((doc) => doc.view.campaign_id === serverSelected);
  const outsideRead = !listed && outside?.id === serverSelected ? outside : null;
  const serverDoc = listed ?? outsideRead?.doc ?? undefined;
  const serverDown = !!serverError || serverSilent;
  const connection = serverDown || outsideRead?.failed ? "stale" : "live";
  // A linked or just-created campaign the list does not hold is read on its own, after each list poll.
  useEffect(() => {
    if (mode !== "server" || !serverSelected || !serverLoaded) return;
    if (serverDocs.some((doc) => doc.view.campaign_id === serverSelected)) { setOutside(null); return; }
    const id = ++detailRequest.current;
    const campaign = serverSelected;
    getServerCampaign(campaign).then(
      (doc) => { if (mounted.current && id === detailRequest.current) setOutside({ id: campaign, doc, failed: false, missing: false }); },
      (cause) => {
        if (!mounted.current || id !== detailRequest.current) return;
        // Only the server saying 404 makes it missing; any other failure keeps the last copy, marked stale.
        const missing = cause instanceof ApiRequestError && cause.status === 404;
        setOutside((current) => ({ id: campaign, doc: !missing && current?.id === campaign ? current.doc : null, failed: !missing, missing }));
      },
    );
  }, [mode, serverSelected, serverLoaded, serverDocs]);
  const selectedCampaign = serverDoc?.view.campaign_id;
  const lastEvent = serverDoc?.view.last_event_sequence;
  // Events are read when the campaign or its last event moves; after a failed read, each later list read retries it.
  const eventsRetry = events !== null && events.campaign === selectedCampaign && events.failedAfter !== null ? serverPolls - events.failedAfter : 0;
  useEffect(() => {
    if (!selectedCampaign || lastEvent === undefined) return;
    const read = `${selectedCampaign}\n${lastEvent}`;
    if (eventsPending.current === read) return; // it will answer; request() always settles
    const id = ++eventsRequest.current; // a read for anything else is no longer wanted
    eventsPending.current = null;
    if (events?.read === read && events.failedAfter === null) return;
    eventsPending.current = read;
    const campaign = selectedCampaign;
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
        setEvents((current) => ({ campaign, read, events: current?.campaign === campaign ? current.events : null, failedAfter: serverPollCount.current }));
      },
    );
  }, [selectedCampaign, lastEvent, eventsRetry]); // eslint-disable-line react-hooks/exhaustive-deps -- `events` is read, not watched
  useEffect(() => setStops((current) => {
    let changed = false;
    const next: Record<string, StopIntent> = {};
    for (const [campaign, intent] of Object.entries(current)) {
      const doc = serverDocs.find((candidate) => candidate.view.campaign_id === campaign) ?? (outside?.id === campaign ? outside.doc : null);
      const settled = doc ? reconcileStop(intent, doc.view, events?.campaign === campaign ? events.events ?? [] : []) : intent;
      if (settled) next[campaign] = settled; else changed = true;
    }
    return changed ? next : current;
  }), [serverDocs, outside, events]);

  const location: CampaignLocation | null = mode === "server"
    ? serverSelected ? { mode, campaign: serverSelected, node } : null
    : selectedId ? { mode, campaign: selectedId, node } : null;
  const query = location ? campaignLocationQuery(location) : null;
  // Runs after every render, so a navigation that dropped the query while this panel stayed mounted is undone;
  // once the address shows another page, the query is not this panel's to write.
  const [home] = useState(() => window.location.pathname);
  useEffect(() => {
    const { pathname, search, hash } = window.location;
    if (query && samePage(pathname, home) && search !== query) window.history.replaceState(window.history.state, "", `${pathname}${query}${hash}`);
  });

  const selectServer = (id: string) => { setMode("server"); setServerSelected(id); setNode(null); };
  const runCommand = async (doc: ServerCampaignDocument, name: ControlCommand, nodeId?: string) => {
    const { view } = doc;
    const campaign = view.campaign_id;
    let key = newIdempotencyKey();
    let body: ServerCampaignCommand;
    let intent: StopIntent | null = null;
    if (name === "stop_immediate" || name === "stop_finish_stage") {
      intent = stopRequest(stops[campaign] ?? null, view, name === "stop_immediate" ? "immediate" : "finish_stage", newIdempotencyKey);
      ({ key, command: body } = intent);
      setStops((current) => ({ ...current, [campaign]: intent! }));
    } else if (name === "start") body = { command: "start", expected_definition_revision: view.definition_revision };
    else if (name === "resume") body = { command: "resume", expected_definition_revision: view.definition_revision, expected_run: view.run_id ?? "" };
    else body = { command: "retry", expected_definition_revision: view.definition_revision, expected_run: view.run_id ?? "", node_id: nodeId ?? "" };
    const forget = () => setStops((current) => { const next = { ...current }; if (next[campaign]?.key === key) delete next[campaign]; return next; });
    setBusy((current) => new Set(current).add(campaign));
    setCommandError((current) => current?.campaign === campaign ? null : current);
    try {
      await sendServerCampaignCommand(campaign, body, key);
      forget();
    } catch (cause) {
      const refusal = serverCampaignRejection(cause);
      if (intent && !isDefinitiveAnswer(refusal)) {
        setStops((current) => ({ ...current, [campaign]: { ...intent!, status: "unconfirmed" } }));
      } else {
        forget();
        setCommandError({
          campaign,
          text: !refusal ? tr("응답을 받지 못했습니다. 다음 갱신에서 결과를 확인하세요.", "No answer arrived. The next refresh shows the outcome.")
            : refusal.rejection ? rejectionText(refusal.rejection, tr) : refusal.status === 409 ? rejectionText("idempotency_conflict", tr) : refusal.message,
        });
      }
    } finally {
      setBusy((current) => { const next = new Set(current); next.delete(campaign); return next; });
      void refreshServer(); // The screen shows the server's view after a write, never one built from the reply.
    }
  };
  const setServerDraft = (key: string, draft: DefinitionDraft | null | ((current: DefinitionDraft) => DefinitionDraft)) => setServerDrafts((current) => {
    // An answer for a draft cancelled meanwhile has nothing left to update.
    const value = typeof draft === "function" ? current[key] && draft(current[key]) : draft;
    const next = { ...current };
    if (value) next[key] = value; else delete next[key];
    return next;
  });
  const requestFocus = (key: string) => { focusRequest.current = { key, n: ++focusCount.current }; };
  const openDraft = (key: string, draft: () => DefinitionDraft) => {
    requestFocus(key);
    setServerDrafts((current) => current[key] ? { ...current } : { ...current, [key]: draft() });
  };
  // A copy fills an untouched create form; one being written is kept and says the copy was not applied.
  const copyToCreate = (definition: ServerCampaignDocument["definition"]) => {
    requestFocus(NEW_DRAFT);
    setServerDrafts((current) => {
      const existing = current[NEW_DRAFT];
      if (!existing || isUntouchedCreate(existing)) return { ...current, [NEW_DRAFT]: createDraft(definition) };
      if (existing.pending) return { ...current };
      const text = tr("작성 중인 새 캠페인이 있어 복사하지 않았습니다. 생성하거나 취소한 뒤 다시 복사하세요.", "A new campaign is already being written, so nothing was copied. Create or cancel it, then copy again.");
      return { ...current, [NEW_DRAFT]: { ...existing, status: { kind: "error", text } } };
    });
  };
  const renderForm = (key: string, disabled: boolean) => serverDrafts[key] && <ServerCampaignForm key={key}
    draft={serverDrafts[key]} disabled={disabled} tr={tr}
    focus={focusRequest.current?.key === key ? focusRequest.current.n : 0} onFocused={() => { focusRequest.current = null; }}
    onDraftChange={(draft) => setServerDraft(key, draft)} onCancel={() => setServerDraft(key, null)} onConflict={() => void refreshServer()}
    onSaved={(id, _result, sentKey) => {
      // Only the draft that body came from closes; anything else stays the user's.
      setServerDrafts((current) => {
        if (current[key]?.sent?.key !== sentKey) return current;
        const next = { ...current };
        delete next[key];
        return next;
      });
      if (key === NEW_DRAFT) selectServer(id);
      void refreshServer();
    }} />;

  const campaign = campaigns.find((value) => value.id === selectedId);
  const progress = campaign ? campaignProgress(campaign.nodes) : null;
  const onSaved = (updated: Campaign) => {
    requestId.current++; // A poll started before this save must not replace its result.
    setLoading(false); setError(null); setRefreshedAt(Date.now());
    setCampaigns((current) => current.map((value) => value.id === updated.id && updated.revision >= value.revision ? updated : value));
  };
  const selector = <label className="campaign-selector"><span>{tr("캠페인 선택", "Select campaign")}</span><select aria-label={tr("캠페인 선택", "Select campaign")} value={mode === "server" ? `${SERVER_OPTION}${serverSelected}` : campaign?.id ?? ""} onChange={(event) => {
    const { value } = event.target;
    if (value.startsWith(SERVER_OPTION)) selectServer(value.slice(SERVER_OPTION.length));
    else { setMode("legacy"); setSelectedId(value); setNode(null); }
  }}>
    {campaigns.map((value) => <option key={value.id} value={value.id}>{value.title}</option>)}
    {[...serverDocs, ...(serverDoc && !serverDocs.includes(serverDoc) ? [serverDoc] : [])].map(({ definition, view }) => <option key={view.campaign_id} value={`${SERVER_OPTION}${view.campaign_id}`}>{definition.title}</option>)}
    {/* The selection must match an option, or the browser shows another campaign as chosen and it cannot be picked. */}
    {mode === "server" && serverSelected && !serverDoc && <option value={`${SERVER_OPTION}${serverSelected}`}>{serverSelected}</option>}
  </select></label>;
  return <section className="campaigns-panel">
    <header className="campaign-row"><div><h2>{tr("캠페인", "Campaigns")}</h2><p>{tr("그룹별 작업을 살펴보고, 선택한 작업의 다음 행동부터 이어가세요.", "Browse tasks by group and pick up the next action.")}</p></div><div className="campaign-actions"><button disabled={serverDown} onClick={() => openDraft(NEW_DRAFT, () => createDraft(null))}>{tr("새 서버 캠페인", "New server campaign")}</button><button disabled={loading} onClick={() => refreshAll()}>{loading ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")}</button></div></header>
    {error && <WidgetState kind={campaigns.length ? "stale" : "error"} title={tr("캠페인을 갱신하지 못했습니다.", "Could not refresh campaigns.")} description={error + (campaigns.length ? tr(" · 이전 내용을 표시 중입니다.", " · Showing the previous snapshot.") : "")} action={<button onClick={() => void refresh()}>{tr("다시 시도", "Retry")}</button>} />}
    {serverDown && <WidgetState kind={serverDocs.length ? "stale" : "error"} title={tr("서버 캠페인을 갱신하지 못했습니다.", "Could not refresh server campaigns.")} description={(serverError ?? tr("서버가 응답하지 않습니다", "The server is not answering")) + (serverDocs.length ? tr(" · 마지막 서버 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", " · Showing the last server snapshot; only stop can be sent.") : "")} />}
    {!campaigns.length && !serverDocs.length && !error && <WidgetState kind={loading ? "loading" : "empty"} title={loading ? tr("캠페인을 불러오고 있습니다.", "Loading campaigns.") : tr("등록된 캠페인이 없습니다.", "No campaigns yet.")} description={loading ? undefined : tr("캠페인이 등록되면 작업 흐름과 진행 상황이 여기에 표시됩니다.", "Registered campaigns and their progress will appear here.")} />}
    {renderForm(NEW_DRAFT, serverDown)}
    {serverDocs.length > 0 && <ServerCampaignList documents={serverDocs} selectedId={mode === "server" ? serverSelected : null} onSelect={selectServer} tr={tr} />}
    {mode === "legacy" && campaign && progress && <div className="campaign-main">
      <div className="campaign-row campaign-summary-header">
        {selector}
        <Badge status={campaign.status} tr={tr} />
        <span className="campaign-badge">{tr("기존 방식", "Legacy mode")}</span>
        <span className="campaign-summary-round">{tr("라운드", "Round")} {campaign.round}</span>
        <strong>{tr(`완료 ${progress.counts.completed}/${progress.total}`, `${progress.counts.completed}/${progress.total} done`)}</strong>
        {refreshedAt && <span className="campaign-freshness">{tr("확인", "Checked")} {new Date(refreshedAt).toLocaleTimeString(language)}</span>}
        <CampaignAutoQueueToggle key={campaign.id} campaign={campaign} tr={tr} onSaved={onSaved} checkedRead={checkedRead} />
      </div>
      {campaign.description && <p className="campaign-description" title={campaign.description}>{campaign.description}</p>}
      <progress className="campaign-progress" max={100} value={progress.percent} aria-label={tr("작업 완료율", "Task completion")} />
      {campaign.nodes.length ? <CampaignExplorer key={campaign.id} campaign={campaign} live={live[campaign.id] ?? NO_LIVE} tr={tr} onSaved={onSaved} drafts={drafts} onDraftChange={onDraftChange} initialSelectedId={node} onSelectedChange={setNode} /> : <WidgetState kind="empty" title={tr("아직 작업이 없습니다.", "No tasks yet.")} />}
    </div>}
    {mode === "server" && serverSelected && <div className="campaign-main">
      <div className="campaign-row campaign-summary-header">{selector}</div>
      {outsideRead?.failed && <WidgetState kind={serverDoc ? "stale" : "error"} title={tr("이 서버 캠페인을 갱신하지 못했습니다.", "Could not refresh this server campaign.")} description={serverDoc ? tr("마지막 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", "Showing the last snapshot; only stop can be sent.") : undefined} />}
      {serverDoc ? <>
        {renderForm(editKey(serverDoc.view.campaign_id), !commandAvailability(serverDoc.view, "edit_definition", connection).enabled)}
        <ServerCampaignDetail document={serverDoc} events={events?.campaign === serverDoc.view.campaign_id ? events.events : null}
          eventsFailed={events?.campaign === serverDoc.view.campaign_id && events.failedAfter !== null}
          connection={connection} selectedNode={node} onSelectNode={setNode} stopIntent={stops[serverDoc.view.campaign_id] ?? null}
          busy={busy.has(serverDoc.view.campaign_id)} commandError={commandError?.campaign === serverDoc.view.campaign_id ? commandError.text : null}
          onCommand={(name, nodeId) => void runCommand(serverDoc, name, nodeId)}
          onEdit={() => openDraft(editKey(serverDoc.view.campaign_id), () => editDraft(serverDoc))}
          onCopy={() => copyToCreate(serverDoc.definition)}
          link={`${window.location.origin}${window.location.pathname}${query ?? ""}`} tr={tr} />
      </> : outsideRead?.missing ? <WidgetState kind="empty" title={tr("이 서버 캠페인을 찾을 수 없습니다.", "This server campaign was not found.")} />
        : !outsideRead?.failed && <WidgetState kind="loading" title={tr("서버 캠페인을 불러오고 있습니다.", "Loading server campaigns.")} />}
    </div>}
  </section>;
}
