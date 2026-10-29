import { useCallback, useEffect, useRef, useState } from "react";
import { getCampaigns, type Campaign, type CampaignLive, type CampaignNodeLive } from "../../api/campaigns";
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
import ServerCampaignForm, { createDraft, editDraft, type DefinitionDraft } from "./ServerCampaignForm";
import ServerCampaignList from "./ServerCampaignList";
import {
  campaignLocationQuery, commandAvailability, newIdempotencyKey, parseCampaignLocation, reconcileStop, rejectionText, stopRequest,
  type CampaignLocation, type StopIntent,
} from "./serverCampaignModel";
import "./campaigns.css";

const NO_LIVE: Record<string, CampaignNodeLive> = {};
const SERVER_OPTION = "server:";
const NEW_DRAFT = "server:new";

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
  const [outsideList, setOutsideList] = useState<ServerCampaignDocument | null>(null);
  const [events, setEvents] = useState<{ campaign: string; events: ServerCampaignEvent[] } | null>(null);
  const [serverDrafts, setServerDrafts] = useState<Record<string, DefinitionDraft>>({});
  const [stops, setStops] = useState<Record<string, StopIntent>>({});
  const [busy, setBusy] = useState(false);
  const [commandError, setCommandError] = useState<string | null>(null);
  const serverRequest = useRef(0);
  const detailRequest = useRef(0);
  const eventsRequest = useRef(0);
  const refreshServer = useCallback(async () => {
    const id = ++serverRequest.current;
    try {
      const documents = await getServerCampaigns();
      if (!mounted.current || id !== serverRequest.current) return;
      setServerDocs(documents); setServerError(null);
    } catch (cause) {
      if (mounted.current && id === serverRequest.current) setServerError(cause instanceof Error ? cause.message : "Unable to load server campaigns");
    } finally { if (mounted.current && id === serverRequest.current) setServerLoaded(true); }
  }, []);
  const refreshAll = useCallback(() => { void refresh(); void refreshServer(); }, [refresh, refreshServer]);
  useEffect(() => {
    mounted.current = true;
    refreshAll();
    const interval = window.setInterval(() => { if (document.visibilityState === "visible") refreshAll(); }, 15_000);
    return () => { mounted.current = false; requestId.current++; serverRequest.current++; window.clearInterval(interval); };
  }, [refreshAll]);
  useEffect(() => { writeLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, selectedId); }, [selectedId]);

  const serverDoc = serverDocs.find((doc) => doc.view.campaign_id === serverSelected)
    ?? (outsideList?.view.campaign_id === serverSelected ? outsideList : undefined);
  const connection = serverError ? "stale" : "live";
  // A linked campaign past the first list page is read on its own, after each list poll.
  useEffect(() => {
    if (mode !== "server" || !serverSelected || !serverLoaded || serverDocs.some((doc) => doc.view.campaign_id === serverSelected)) return;
    const id = ++detailRequest.current;
    getServerCampaign(serverSelected).then((doc) => { if (mounted.current && id === detailRequest.current) setOutsideList(doc); }).catch(() => undefined);
  }, [mode, serverSelected, serverLoaded, serverDocs]);
  const selectedCampaign = serverDoc?.view.campaign_id;
  const lastEvent = serverDoc?.view.last_event_sequence;
  useEffect(() => {
    if (!selectedCampaign || lastEvent === undefined) return;
    const id = ++eventsRequest.current;
    getServerCampaignEvents(selectedCampaign, { after: Math.max(0, lastEvent - 50), limit: 50 })
      .then((list) => { if (mounted.current && id === eventsRequest.current) setEvents({ campaign: selectedCampaign, events: list }); })
      .catch(() => undefined);
  }, [selectedCampaign, lastEvent]);
  useEffect(() => setStops((current) => {
    let changed = false;
    const next: Record<string, StopIntent> = {};
    for (const [campaign, intent] of Object.entries(current)) {
      const doc = serverDocs.find((candidate) => candidate.view.campaign_id === campaign) ?? (outsideList?.view.campaign_id === campaign ? outsideList : undefined);
      const settled = doc ? reconcileStop(intent, doc.view, events?.campaign === campaign ? events.events : []) : intent;
      if (settled) next[campaign] = settled; else changed = true;
    }
    return changed ? next : current;
  }), [serverDocs, outsideList, events]);

  const location: CampaignLocation | null = mode === "server"
    ? serverSelected ? { mode, campaign: serverSelected, node } : null
    : selectedId ? { mode, campaign: selectedId, node } : null;
  const query = location ? campaignLocationQuery(location) : null;
  useEffect(() => {
    if (query && window.location.search !== query) window.history.replaceState(window.history.state, "", `${window.location.pathname}${query}${window.location.hash}`);
  }, [query]);

  const selectServer = (id: string) => { setMode("server"); setServerSelected(id); setNode(null); setCommandError(null); };
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
    setBusy(true); setCommandError(null);
    try {
      await sendServerCampaignCommand(campaign, body, key);
      forget();
    } catch (cause) {
      const refusal = serverCampaignRejection(cause);
      if (intent && !isDefinitiveAnswer(refusal)) {
        setStops((current) => ({ ...current, [campaign]: { ...intent!, status: "unconfirmed" } }));
      } else {
        forget();
        setCommandError(!refusal ? tr("응답을 받지 못했습니다. 다음 갱신에서 결과를 확인하세요.", "No answer arrived. The next refresh shows the outcome.")
          : refusal.rejection ? rejectionText(refusal.rejection, tr) : refusal.status === 409 ? rejectionText("idempotency_conflict", tr) : `HTTP ${refusal.status}`);
      }
    } finally {
      setBusy(false);
      void refreshServer(); // The screen shows the server's view after a write, never one built from the reply.
    }
  };
  const setServerDraft = (key: string, draft: DefinitionDraft | null) => setServerDrafts((current) => {
    const next = { ...current };
    if (draft) next[key] = draft; else delete next[key];
    return next;
  });
  const openDraft = (key: string, draft: () => DefinitionDraft) => setServerDrafts((current) => current[key] ? current : { ...current, [key]: draft() });
  const renderForm = (key: string, disabled: boolean) => serverDrafts[key] && <ServerCampaignForm draft={serverDrafts[key]} disabled={disabled} tr={tr}
    onDraftChange={(draft) => setServerDraft(key, draft)} onCancel={() => setServerDraft(key, null)}
    onSaved={(id) => { setServerDraft(key, null); if (key === NEW_DRAFT) selectServer(id); void refreshServer(); }} />;

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
  </select></label>;
  return <section className="campaigns-panel">
    <header className="campaign-row"><div><h2>{tr("캠페인", "Campaigns")}</h2><p>{tr("그룹별 작업을 살펴보고, 선택한 작업의 다음 행동부터 이어가세요.", "Browse tasks by group and pick up the next action.")}</p></div><div className="campaign-actions"><button disabled={!!serverError} onClick={() => openDraft(NEW_DRAFT, () => createDraft(null))}>{tr("새 서버 캠페인", "New server campaign")}</button><button disabled={loading} onClick={refreshAll}>{loading ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")}</button></div></header>
    {error && <WidgetState kind={campaigns.length ? "stale" : "error"} title={tr("캠페인을 갱신하지 못했습니다.", "Could not refresh campaigns.")} description={error + (campaigns.length ? tr(" · 이전 내용을 표시 중입니다.", " · Showing the previous snapshot.") : "")} action={<button onClick={() => void refresh()}>{tr("다시 시도", "Retry")}</button>} />}
    {serverError && <WidgetState kind={serverDocs.length ? "stale" : "error"} title={tr("서버 캠페인을 갱신하지 못했습니다.", "Could not refresh server campaigns.")} description={serverError + (serverDocs.length ? tr(" · 마지막 서버 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", " · Showing the last server snapshot; only stop can be sent.") : "")} />}
    {!campaigns.length && !serverDocs.length && !error && <WidgetState kind={loading ? "loading" : "empty"} title={loading ? tr("캠페인을 불러오고 있습니다.", "Loading campaigns.") : tr("등록된 캠페인이 없습니다.", "No campaigns yet.")} description={loading ? undefined : tr("캠페인이 등록되면 작업 흐름과 진행 상황이 여기에 표시됩니다.", "Registered campaigns and their progress will appear here.")} />}
    {renderForm(NEW_DRAFT, !!serverError)}
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
      {serverDoc ? <>
        {renderForm(`server:${serverDoc.view.campaign_id}`, !commandAvailability(serverDoc.view, "edit_definition", connection).enabled)}
        <ServerCampaignDetail document={serverDoc} events={events?.campaign === serverDoc.view.campaign_id ? events.events : null}
          connection={connection} selectedNode={node} onSelectNode={setNode} stopIntent={stops[serverDoc.view.campaign_id] ?? null}
          busy={busy} commandError={commandError} onCommand={(name, nodeId) => void runCommand(serverDoc, name, nodeId)}
          onEdit={() => openDraft(`server:${serverDoc.view.campaign_id}`, () => editDraft(serverDoc))}
          onCopy={() => openDraft(NEW_DRAFT, () => createDraft(serverDoc.definition))}
          link={`${window.location.origin}${window.location.pathname}${query ?? ""}`} tr={tr} />
      </> : <WidgetState kind={serverLoaded ? "empty" : "loading"} title={serverLoaded ? tr("이 서버 캠페인을 찾을 수 없습니다.", "This server campaign was not found.") : tr("서버 캠페인을 불러오고 있습니다.", "Loading server campaigns.")} />}
    </div>}
  </section>;
}
