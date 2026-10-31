import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import type { CampaignNodeLive } from "../../api/campaigns";
import {
  isDefinitiveAnswer, sendServerCampaignCommand, serverCampaignRejection,
  type ServerCampaignCommand, type ServerCampaignDocument,
} from "../../api/serverCampaigns";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue, writeLocalStorageValue } from "../../lib/useLocalStorage";
import { Select } from "../common/Select";
import { WidgetState } from "../common/WidgetState";
import CampaignPageHeader, { type CampaignTab } from "./CampaignPageHeader";
import type { Tr } from "./campaignPresentation";
import type { CampaignDraft } from "./CampaignNodeDetails";
import LegacyCampaignView from "./LegacyCampaignView";
import type { ControlCommand } from "./ServerCampaignControls";
import ServerCampaignDetail from "./ServerCampaignDetail";
import ServerCampaignForm, { createDraft, editDraft, isUntouchedCreate, type DefinitionDraft } from "./ServerCampaignForm";
import ServerCampaignList from "./ServerCampaignList";
import {
  campaignLocationQuery, commandAvailability, newIdempotencyKey, parseCampaignLocation, reconcileStop, rejectionText, stopRequest,
  type CampaignLocation, type StopIntent,
} from "./serverCampaignModel";
import { queueLocationQuery, parseQueueLocation } from "./campaignQueueModel";
import { useLegacyCampaigns } from "./useLegacyCampaigns";
import { useServerCampaignReads } from "./useServerCampaignReads";
import "./campaigns.css";

const CampaignQueueTab = lazy(() => import("./CampaignQueueTab"));

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
  // A link names the tab, or the mode, campaign and task; without one the legacy selection is restored.
  const [linked] = useState(() => parseCampaignLocation(window.location.search));
  const [queueLink] = useState(() => parseQueueLocation(window.location.search));
  const [tab, setTab] = useState<CampaignTab>(queueLink ? "queue" : "campaigns");
  const [queueCard, setQueueCard] = useState<string | null>(queueLink?.card ?? null);
  const legacy = useLegacyCampaigns();
  const { campaigns, live, loading, error, refreshedAt, checkedRead, onSaved } = legacy;
  const [drafts, setDrafts] = useState<Record<string, CampaignDraft>>({});
  const onDraftChange = (key: string, draft: CampaignDraft | null, expected?: CampaignDraft) => setDrafts((current) => {
    // A save may finish after this task was reopened and its draft changed.
    if (expected && current[key] !== expected) return current;
    const next = { ...current };
    if (draft) next[key] = draft; else delete next[key];
    return next;
  });
  const [selectedId, setSelectedId] = useState<string | null>(() => linked?.mode === "legacy" ? linked.campaign : readLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, null, { validate: (value): value is string | null => value === null || typeof value === "string" }));
  useEffect(() => {
    if (refreshedAt === null) return;
    setSelectedId((current) => campaigns.some((value) => value.id === current) ? current : campaigns.find((value) => value.status === "active")?.id ?? campaigns[0]?.id ?? null);
  }, [campaigns, refreshedAt]);
  useEffect(() => { writeLocalStorageValue(STORAGE_KEYS.dashboardActiveCampaign, selectedId); }, [selectedId]);

  const [mode, setMode] = useState<CampaignLocation["mode"]>(linked?.mode ?? "legacy");
  const [serverSelected, setServerSelected] = useState<string | null>(linked?.mode === "server" ? linked.campaign : null);
  const [node, setNode] = useState<string | null>(linked?.node ?? null);
  const server = useServerCampaignReads(mode === "server" ? serverSelected : null);
  const { summaries, listLoaded, listError, details, detail, events, refreshList, refreshDetail } = server;
  const serverDoc = server.doc;
  const serverDown = !!listError || server.silent;
  const connection = serverDown || detail?.error ? "stale" : "live";
  const [serverDrafts, setServerDrafts] = useState<Record<string, DefinitionDraft>>({});
  // The form the user just opened or filled takes focus once; showing it again later does not.
  const focusRequest = useRef<{ key: string; n: number } | null>(null);
  const focusCount = useRef(0);
  const [stops, setStops] = useState<Record<string, StopIntent>>({});
  // A command's progress and refusal belong to the campaign it was sent for.
  const [busy, setBusy] = useState<ReadonlySet<string>>(new Set());
  const [commandErrors, setCommandErrors] = useState<Record<string, string>>({});

  const refreshServer = useCallback((campaign: string | null) => {
    void refreshList();
    if (campaign) void refreshDetail(campaign);
  }, [refreshList, refreshDetail]);
  const refreshAll = () => { void legacy.refresh(); refreshServer(mode === "server" ? serverSelected : null); };

  // The list of every campaign is read once; each poll reads only the campaign on screen, and none while hidden.
  const poll = useRef<(visible: boolean) => void>(() => {});
  poll.current = (visible) => {
    const watching = visible && tab === "campaigns";
    server.tick(watching);
    if (watching && mode === "legacy" && selectedId) void legacy.refreshCampaign(selectedId);
  };
  const resume = useRef<() => void>(() => {});
  resume.current = () => {
    if (tab !== "campaigns") return;
    if (mode === "server") server.resume();
    else if (selectedId) void legacy.refreshCampaign(selectedId);
  };
  useEffect(() => {
    void legacy.refresh(); void refreshList(true);
    const interval = window.setInterval(() => poll.current(document.visibilityState === "visible"), POLL_MS);
    const onVisibility = () => { if (document.visibilityState === "visible") resume.current(); };
    document.addEventListener("visibilitychange", onVisibility);
    return () => { window.clearInterval(interval); document.removeEventListener("visibilitychange", onVisibility); };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps -- reads start once; polls read the latest state through refs

  useEffect(() => setStops((current) => {
    let changed = false;
    const next: Record<string, StopIntent> = {};
    for (const [campaign, intent] of Object.entries(current)) {
      const doc = details[campaign]?.doc ?? undefined;
      const settled = doc ? reconcileStop(intent, doc.view, events?.campaign === campaign ? events.events ?? [] : []) : intent;
      if (settled) next[campaign] = settled; else changed = true;
    }
    return changed ? next : current;
  }), [details, events]);

  const location: CampaignLocation | null = mode === "server"
    ? serverSelected ? { mode, campaign: serverSelected, node } : null
    : selectedId ? { mode, campaign: selectedId, node } : null;
  const query = tab === "queue" ? queueLocationQuery(queueCard) : location ? campaignLocationQuery(location) : null;
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
    setCommandErrors((current) => { const next = { ...current }; delete next[campaign]; return next; });
    try {
      await sendServerCampaignCommand(campaign, body, key);
      forget();
    } catch (cause) {
      const refusal = serverCampaignRejection(cause);
      if (intent && !isDefinitiveAnswer(refusal)) {
        setStops((current) => ({ ...current, [campaign]: { ...intent!, status: "unconfirmed" } }));
      } else {
        forget();
        const text = !refusal ? tr("응답을 받지 못했습니다. 다음 갱신에서 결과를 확인하세요.", "No answer arrived. The next refresh shows the outcome.")
          : refusal.rejection ? rejectionText(refusal.rejection, tr) : refusal.status === 409 ? rejectionText("idempotency_conflict", tr) : refusal.message;
        setCommandErrors((current) => ({ ...current, [campaign]: text }));
      }
    } finally {
      setBusy((current) => { const next = new Set(current); next.delete(campaign); return next; });
      refreshServer(campaign); // The screen shows the server's view after a write, never one built from the reply.
    }
  };
  const setServerDraft = (key: string, update: (current: DefinitionDraft) => DefinitionDraft | null) => setServerDrafts((current) => {
    // A save answering after the draft was closed must not reopen it.
    if (!current[key]) return current;
    const draft = update(current[key]);
    const next = { ...current };
    if (draft) next[key] = draft; else delete next[key];
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
    onDraftChange={(update) => setServerDraft(key, update)} onCancel={() => setServerDraft(key, () => null)} onConflict={() => refreshServer(key === NEW_DRAFT ? null : key.slice("edit:".length))}
    onSaved={(id) => {
      // The form keeps edits made while a create was in flight; they continue as an edit of the new campaign.
      if (key === NEW_DRAFT) {
        setServerDrafts((current) => {
          // Only the draft the create came from moves; one opened after it stays a create.
          if (current[NEW_DRAFT]?.mode !== "edit" || current[NEW_DRAFT].id !== id) return current;
          // An edit draft already open for the campaign keeps its key; both drafts stay to be saved or cancelled.
          if (current[editKey(id)]) return current;
          const { [NEW_DRAFT]: kept, ...rest } = current;
          return { ...rest, [editKey(id)]: kept };
        });
        selectServer(id);
      }
      refreshServer(id);
    }} />;

  const campaign = campaigns.find((value) => value.id === selectedId);
  const selectorTitle = tr("캠페인 선택", "Select campaign");
  const selector = <label className="campaign-selector"><span>{selectorTitle}</span><Select aria-label={selectorTitle} className="font-semibold" value={mode === "server" ? `${SERVER_OPTION}${serverSelected}` : campaign?.id ?? ""} onChange={(event) => {
    const { value } = event.target;
    if (value.startsWith(SERVER_OPTION)) selectServer(value.slice(SERVER_OPTION.length));
    else { setMode("legacy"); setSelectedId(value); setNode(null); }
  }}>
    {campaigns.map((value) => <option key={value.id} value={value.id}>{value.title}</option>)}
    {summaries.map(({ definition, view }) => <option key={view.campaign_id} value={`${SERVER_OPTION}${view.campaign_id}`}>{definition.title}</option>)}
    {/* The selection must match an option, or the browser shows another campaign as chosen and it cannot be picked. */}
    {mode === "server" && serverSelected && !summaries.some((summary) => summary.view.campaign_id === serverSelected) && <option value={`${SERVER_OPTION}${serverSelected}`}>{serverDoc?.definition.title ?? serverSelected}</option>}
  </Select></label>;

  const actions = tab === "campaigns" && <>
    <button type="button" disabled={serverDown} onClick={() => openDraft(NEW_DRAFT, () => createDraft(null))}>{tr("새 캠페인", "New campaign")}</button>
    <button type="button" disabled={loading} onClick={refreshAll}>{loading ? tr("불러오는 중…", "Refreshing…") : tr("새로고침", "Refresh")}</button>
  </>;
  return <section className="campaigns-panel [&_[data-widget-state]]:!rounded-lg min-w-0 gap-4 text-th-text-primary sm:gap-6 [&_button]:min-h-11 [&_button]:min-w-11 [&_input:not([type=checkbox])]:min-h-11 [&_select]:min-h-11 [&_summary]:min-h-11 [&_summary]:py-3 [&_label:has(input[type=checkbox])]:min-h-11 [&_h2]:text-2xl [&_h2]:font-semibold [&_h2]:tracking-tight [&_h3]:font-semibold [&_h4]:text-xs [&_h4]:font-medium [&_a]:inline-flex [&_a]:min-h-11 [&_a]:max-w-full [&_a]:items-center [&_a]:py-2 [&_a]:[overflow-wrap:anywhere]">
    <CampaignPageHeader tab={tab} onTab={setTab} actions={actions} tr={tr} />
    {tab === "queue" ? <div id="campaign-panel-queue" role="tabpanel" aria-labelledby="campaign-tab-queue" className="min-w-0">
      <Suspense fallback={<WidgetState kind="loading" title={tr("대기열을 불러오고 있습니다.", "Loading the queue.")} />}>
        <CampaignQueueTab language={language} tr={tr} campaigns={campaigns} campaignsLoaded={refreshedAt !== null} selectedCardId={queueCard} onSelectCard={setQueueCard} onCampaignSaved={onSaved} />
      </Suspense>
    </div> : <div id="campaign-panel-campaigns" role="tabpanel" aria-labelledby="campaign-tab-campaigns" className="grid min-w-0 gap-4 sm:gap-6">
      {error && <WidgetState kind={campaigns.length ? "stale" : "error"} title={tr("캠페인을 갱신하지 못했습니다.", "Could not refresh campaigns.")} description={error + (campaigns.length ? tr(" · 이전 내용을 표시 중입니다.", " · Showing the previous snapshot.") : "")} action={<button onClick={() => void legacy.refresh()}>{tr("다시 시도", "Retry")}</button>} />}
      {serverDown && <WidgetState kind={summaries.length || serverDoc ? "stale" : "error"} title={tr("서버 캠페인을 갱신하지 못했습니다.", "Could not refresh server campaigns.")} description={(listError ?? tr("서버가 응답하지 않습니다", "The server is not answering")) + (summaries.length || serverDoc ? tr(" · 마지막 서버 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", " · Showing the last server snapshot; only stop can be sent.") : "")} />}
      {/* Nothing is called empty until both lists have answered without an error. */}
      {!campaigns.length && !summaries.length && !error && !serverDown && mode === "legacy" && <WidgetState kind={loading || !listLoaded ? "loading" : "empty"} title={loading || !listLoaded ? tr("캠페인을 불러오고 있습니다.", "Loading campaigns.") : tr("등록된 캠페인이 없습니다.", "No campaigns yet.")} description={loading || !listLoaded ? undefined : tr("캠페인이 등록되면 작업 흐름과 진행 상황이 여기에 표시됩니다.", "Registered campaigns and their progress will appear here.")} />}
      {renderForm(NEW_DRAFT, serverDown)}
      {summaries.length > 0 && <ServerCampaignList summaries={summaries} selectedId={mode === "server" ? serverSelected : null} onSelect={selectServer} tr={tr} />}
      {mode === "legacy" && campaign && <LegacyCampaignView campaign={campaign} live={live[campaign.id] ?? NO_LIVE} selector={selector} refreshedAt={refreshedAt} checkedRead={checkedRead} language={language}
        drafts={drafts} onDraftChange={onDraftChange} node={node} onNode={setNode} onSaved={onSaved} tr={tr} />}
      {mode === "server" && serverSelected && <div className="campaign-main gap-4 rounded-lg p-3 sm:p-5">
        <div className="campaign-row campaign-summary-header">{selector}</div>
        {detail?.error && <WidgetState kind={serverDoc ? "stale" : "error"} title={tr("이 서버 캠페인을 갱신하지 못했습니다.", "Could not refresh this server campaign.")} description={detail.error + (serverDoc ? tr(" · 마지막 서버 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", " · Showing the last server snapshot; only stop can be sent.") : "")} />}
        {serverDoc ? <>
          {renderForm(editKey(serverDoc.view.campaign_id), !commandAvailability(serverDoc.view, "edit_definition", connection).enabled)}
          <ServerCampaignDetail document={serverDoc} events={events?.campaign === serverDoc.view.campaign_id ? events.events : null}
            eventsFailed={events?.campaign === serverDoc.view.campaign_id && events.failedAfter !== null}
            connection={connection} selectedNode={node} onSelectNode={setNode} stopIntent={stops[serverDoc.view.campaign_id] ?? null}
            busy={busy.has(serverDoc.view.campaign_id)} commandError={commandErrors[serverDoc.view.campaign_id] ?? null}
            onCommand={(name, nodeId) => void runCommand(serverDoc, name, nodeId)}
            onEdit={() => openDraft(editKey(serverDoc.view.campaign_id), () => editDraft(serverDoc))}
            onCopy={() => copyToCreate(serverDoc.definition)}
            link={`${window.location.origin}${window.location.pathname}${query ?? ""}`} tr={tr} />
        </> : detail?.error ? null : <WidgetState kind={detail && !detail.loading ? "empty" : "loading"} title={detail && !detail.loading ? tr("이 서버 캠페인을 찾을 수 없습니다.", "This server campaign was not found.") : tr("서버 캠페인을 불러오고 있습니다.", "Loading server campaigns.")} />}
      </div>}
    </div>}
  </section>;
}
