import { ChevronDown } from "lucide-react";
import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";
import {
  isDefinitiveAnswer, sendServerCampaignCommand, serverCampaignProfile, serverCampaignRejection,
  type ServerCampaignCommand, type ServerCampaignDocument,
} from "../../api/serverCampaigns";
import { PageFrame } from "../../app/PageFrame";
import { Badge as KitBadge } from "../../kit/free/badge-clean";
import { Button } from "../../kit/free/button-clean";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { readLocalStorageValue, writeLocalStorageValue } from "../../lib/useLocalStorage";
import { BottomSheet } from "../common/overlay/BottomSheet";
import { Modal } from "../common/overlay/Modal";
import { useIsMobile } from "../common/overlay/useBreakpoint";
import { WidgetState } from "../common/WidgetState";
import CampaignAutoQueueToggle, { useAutoQueueRequests } from "./CampaignAutoQueueToggle";
import CampaignBoard from "./CampaignBoard";
import { legacyBoard, serverBoard } from "./campaignBoardModel";
import CampaignDetail from "./CampaignDetail";
import CampaignList from "./CampaignList";
import { campaignIssueLabel } from "./campaignModel";
import CampaignNodeDetails, { type CampaignDraft } from "./CampaignNodeDetails";
import CampaignOverview, { type TaskView } from "./CampaignOverview";
import {
  LEDGER_STATUS_LABELS, entryKey, legacyEntry, legacyTask, recentServerEvents, recentTaskChanges, serverEntry, serverTasks, statusSentence, type CampaignEntry,
} from "./campaignOverviewModel";
import { Badge, CAMPAIGN_SCOPE, type Tr } from "./campaignPresentation";
import CampaignRelations from "./CampaignRelations";
import ServerCampaignControls, { type ControlCommand } from "./ServerCampaignControls";
import ServerCampaignSections, { ServerTaskDetails } from "./ServerCampaignDetail";
import ServerCampaignForm, { createDraft, editDraft, isUntouchedCreate, type DefinitionDraft } from "./ServerCampaignForm";
import {
  DISPLAY_LABELS, campaignLocationQuery, commandAvailability, newIdempotencyKey, parseCampaignLocation, reconcileStop, rejectionText, stopRequest,
  type CampaignLocation, type StopIntent,
} from "./serverCampaignModel";
import { queueLocationQuery, parseQueueLocation } from "./campaignQueueModel";
import { useLegacyCampaigns } from "./useLegacyCampaigns";
import { useServerCampaignHolds } from "./useServerCampaignHolds";
import { useServerCampaignReads } from "./useServerCampaignReads";
import "./campaigns.css";

const CampaignQueueTab = lazy(() => import("./CampaignQueueTab"));

type CampaignTab = "campaigns" | "queue";
// Disjoint from edit keys, so a campaign named "new" never shares the create draft.
const NEW_DRAFT = "create";
const editKey = (id: string) => `edit:${id}`;
const POLL_MS = 15_000;
// Both campaign lists are re-read on this slower beat; a failed list read is retried on the next poll.
const LIST_POLL_MS = 60_000;
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
  // The list carries node summaries; the campaign on screen is read in full as soon as it shows.
  const selectedPartial = tab === "campaigns" && mode === "legacy" && campaigns.some((value) => value.id === selectedId && value.nodes_partial);
  const { refreshCampaign } = legacy;
  useEffect(() => { if (selectedPartial && selectedId) void refreshCampaign(selectedId); }, [selectedPartial, selectedId, refreshCampaign]);
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

  // Each poll reads the campaign on screen; the lists come on a slower beat. Nothing is read while hidden.
  const listsReadAt = useRef(0);
  const readListsIfDue = () => {
    if (Date.now() - listsReadAt.current < LIST_POLL_MS && !legacy.error && !listError) return false;
    listsReadAt.current = Date.now();
    void legacy.refresh(true); void refreshList(true);
    return true;
  };
  const poll = useRef<(visible: boolean) => void>(() => {});
  poll.current = (visible) => {
    const watching = visible && tab === "campaigns";
    server.tick(watching);
    if (visible && readListsIfDue()) return;
    if (watching && mode === "legacy" && selectedId) void legacy.refreshCampaign(selectedId);
  };
  const resume = useRef<() => void>(() => {});
  resume.current = () => {
    if (readListsIfDue() || tab !== "campaigns") return;
    if (mode === "server") server.resume();
    else if (selectedId) void legacy.refreshCampaign(selectedId);
  };
  useEffect(() => {
    listsReadAt.current = Date.now();
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

  const holds = useServerCampaignHolds(summaries, listLoaded, tab === "queue");
  const phone = useIsMobile();
  const [picking, setPicking] = useState(false);
  const autoRuns = useAutoQueueRequests();
  const campaign = mode === "legacy" ? campaigns.find((value) => value.id === selectedId) : undefined;
  const entries: CampaignEntry[] = [
    ...campaigns.map((value) => legacyEntry(value, live[value.id], tr)),
    ...summaries.map((summary) => serverEntry(summary, details[summary.view.campaign_id]?.doc ?? null, tr)),
  ];
  // A linked campaign past the list's pages stays listed once its own read answers, so it can be picked again.
  if (serverDoc && !summaries.some((summary) => summary.view.campaign_id === serverDoc.view.campaign_id)) entries.push(serverEntry({ definition: serverDoc.definition, view: serverDoc.view }, serverDoc, tr));
  const selectedKey = mode === "server" ? serverSelected && entryKey("server", serverSelected) : campaign ? entryKey("legacy", campaign.id) : null;
  const selectedEntry = entries.find((entry) => entry.key === selectedKey);
  const select = (entry: CampaignEntry) => {
    setPicking(false);
    if (entry.kind === "server") selectServer(entry.id);
    else { setMode("legacy"); setSelectedId(entry.id); setNode(null); }
  };
  const createNew = () => { setPicking(false); openDraft(NEW_DRAFT, () => createDraft(null)); };
  const list = <CampaignList entries={entries} selectedKey={selectedKey} onSelect={select} onCreate={createNew} createDisabled={serverDown}
    onRefresh={refreshAll} refreshing={loading} tr={tr} />;

  const legacyTasks: TaskView[] = campaign ? campaign.nodes.map((value) => ({ ...legacyTask(value, live[campaign.id]?.[value.id]), live: live[campaign.id]?.[value.id], hold: value.post_merge_hold })) : [];
  const serverEvents = server.events?.campaign === serverDoc?.view.campaign_id ? server.events?.events ?? [] : [];
  const serverTaskViews = serverDoc ? serverTasks(serverDoc, serverEvents, tr) : [];
  const openNode = node && (campaign?.nodes.find((value) => value.id === node) ?? (mode === "server" ? serverDoc?.definition.nodes.find((value) => value.id === node) : undefined));
  const legacyDetail = campaign && <CampaignDetail key={`legacy:${campaign.id}`} title={campaign.title} showTitle={!phone} tr={tr}
    chips={!phone && <span className="flex flex-wrap items-center gap-2 text-xs text-[var(--fg-muted)]">
      <KitBadge title={tr("작업 기록부로 관리하는 캠페인", "Campaign kept as a task ledger")}>{tr("원장", "Ledger")}</KitBadge>
      <span className="tabular-nums">{tr("라운드", "Round")} {campaign.round}</span>
    </span>}
    sentence={statusSentence(tr(...LEDGER_STATUS_LABELS[campaign.status]), legacyTasks, tr)}
    description={campaign.description && <details className="group min-w-0 text-xs text-[var(--fg-muted)]">
      <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-[var(--r-sm)] px-1 hover:text-[var(--fg)] [&::-webkit-details-marker]:hidden">
        <ChevronDown aria-hidden className="size-3.5 -rotate-90 transition-transform group-open:rotate-0 motion-reduce:transition-none" />{tr("캠페인 설명", "Campaign description")}
      </summary>
      <p className="campaign-description mt-1 max-w-[72ch] px-1 text-[var(--fg-dim)]">{campaign.description}</p>
    </details>}
    overview={(controls) => <CampaignOverview tasks={legacyTasks} limit={null} showPr asOf={refreshedAt ? new Date(refreshedAt).toISOString() : null}
      summaryExtra={<CampaignAutoQueueToggle key={campaign.id} campaign={campaign} tr={tr} onSaved={onSaved} checkedRead={checkedRead} held={autoRuns(campaign.id)} />}
      recent={{ title: tr("최근 바뀐 작업", "Recently changed tasks"), entries: recentTaskChanges(legacyTasks) }}
      selectedId={node} controls={controls} onOpen={setNode} language={language} tr={tr} />}
    relations={campaign.nodes.length > 0 && <CampaignRelations campaign={campaign} selectedId={node} onOpen={setNode} tr={tr} />}
    board={(controls) => <CampaignBoard columns={legacyBoard(campaign.nodes, live[campaign.id])} tasks={legacyTasks} selectedId={node} controls={controls} onOpen={setNode} tr={tr} />}
    task={openNode && "status" in openNode ? { id: openNode.id, label: campaignIssueLabel(openNode), body: <CampaignNodeDetails key={openNode.id} campaign={campaign} node={openNode} live={live[campaign.id]?.[openNode.id]} tr={tr} onSaved={onSaved}
      editing={drafts[`${campaign.id}:${openNode.id}`] ?? null} onDraftChange={(draft, expected) => onDraftChange(`${campaign.id}:${openNode.id}`, draft, expected)} /> } : null}
    onCloseTask={() => setNode(null)} />;

  const serverDetail = serverDoc && (() => {
    const { definition, view } = serverDoc;
    const id = view.campaign_id;
    const edit = commandAvailability(view, "edit_definition", connection);
    const shown = server.events?.campaign === id ? server.events : null;
    const profile = serverCampaignProfile(view);
    const retry = commandAvailability(view, "prepare_retry", connection);
    const commands = <ServerCampaignControls view={view} connection={connection} stopIntent={stops[id] ?? null} busy={busy.has(id)} error={commandErrors[id] ?? null} onCommand={(name) => void runCommand(serverDoc, name)} tr={tr} />;
    return <CampaignDetail key={`server:${id}`} title={definition.title} showTitle={!phone} tr={tr}
      chips={(!phone || view.unknown) && <span className="flex flex-wrap items-center gap-2 text-xs">
        {!phone && <><KitBadge>{tr("서버", "Server")}</KitBadge><KitBadge className="font-mono">{definition.repository}</KitBadge></>}
        {view.unknown && <Badge status="blocked" label={tr("확인 불가", "Unknown")} tr={tr} />}
      </span>}
      sentence={statusSentence(tr(...DISPLAY_LABELS[view.display]), serverTaskViews, tr)}
      actions={<>
        <Button variant="secondary" size="sm" disabled={!edit.enabled} title={edit.reason ? rejectionText(edit.reason, tr) : undefined} onClick={() => openDraft(editKey(id), () => editDraft(serverDoc))}>{tr("정의 편집", "Edit definition")}</Button>
        {view.control === "succeeded" && <Button variant="secondary" size="sm" onClick={() => copyToCreate(definition)}>{tr("정의를 복사해 새 캠페인", "Copy definition to a new campaign")}</Button>}
      </>}
      notes={<>
        {detail?.error && <WidgetState kind="stale" title={tr("이 서버 캠페인을 갱신하지 못했습니다.", "Could not refresh this server campaign.")} description={detail.error + tr(" · 마지막 서버 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", " · Showing the last server snapshot; only stop can be sent.")} />}
        {renderForm(editKey(id), !edit.enabled)}
        {!phone && commands}
      </>}
      overview={(controls) => <CampaignOverview tasks={serverTaskViews} limit={profile?.limits.concurrent_tasks ?? null} showPr={false} asOf={view.as_of}
        recent={{ title: tr("최근 이벤트", "Recent events"), entries: shown?.events ? recentServerEvents(shown.events) : null, note: shown && shown.failedAfter !== null ? tr("이벤트를 갱신하지 못했습니다", "Could not refresh events") : null }}
        footer={<>{phone && commands}<ServerCampaignSections document={serverDoc} link={`${window.location.origin}${window.location.pathname}${query ?? ""}`} tr={tr} /></>}
        selectedId={node} controls={controls} onOpen={setNode} language={language} tr={tr} />}
      board={(controls) => <CampaignBoard columns={serverBoard(serverDoc, serverTaskViews, tr)} tasks={serverTaskViews} selectedId={node} controls={controls} onOpen={setNode} tr={tr}
        retry={{ enabled: retry.enabled && !busy.has(id), reason: retry.reason, onRetry: (nodeId) => runCommand(serverDoc, "prepare_retry", nodeId) }} />}
      task={openNode && !("status" in openNode) ? { id: openNode.id, label: openNode.id, body: <ServerTaskDetails document={serverDoc} nodeId={openNode.id}
        gate={serverTaskViews.find((task) => task.id === openNode.id)?.gate ?? null} connection={connection} busy={busy.has(id)} onCommand={(name, nodeId) => void runCommand(serverDoc, name, nodeId)} tr={tr} /> } : null}
      onCloseTask={() => setNode(null)} />;
  })();

  // Below 1280px an open task pane leaves too little width beside the list, so the list folds into the picker.
  const folded = !phone && !!openNode;
  const pickerTitle = selectedEntry?.title ?? (mode === "server" ? serverDoc?.definition.title ?? serverSelected : null);
  return <PageFrame title={tr("캠페인", "Campaigns")} testId="campaigns-page" actions={<div className="w-full sm:w-56">
    <SegmentedControl<CampaignTab> kind="tabs" variant="tray" fullWidth size="sm" aria-label={tr("캠페인 화면", "Campaign views")} value={tab} onChange={setTab}
      options={[{ value: "campaigns", label: tr("캠페인", "Campaigns"), controls: "campaign-panel-campaigns" }, { value: "queue", label: tr("대기열", "Queue"), controls: "campaign-panel-queue" }]} />
  </div>}>
  <section className={`${CAMPAIGN_SCOPE} min-w-0 text-sm`}>
    {tab === "queue" ? <div id="campaign-panel-queue" role="tabpanel" aria-label={tr("대기열", "Queue")} className="min-w-0">
      <Suspense fallback={<WidgetState kind="loading" title={tr("대기열을 불러오고 있습니다.", "Loading the queue.")} />}>
        <CampaignQueueTab language={language} tr={tr} campaigns={campaigns} campaignsLoaded={refreshedAt !== null && holds.loaded}
          campaignsError={refreshedAt === null ? error : null} onRetryCampaigns={() => { void legacy.refresh(); void refreshList(); }}
          heldIssueUrls={holds.issueUrls} serverHoldsIncomplete={!!listError || holds.failed > 0}
          selectedCardId={queueCard} onSelectCard={setQueueCard} onCampaignSaved={onSaved} />
      </Suspense>
    </div> : <div id="campaign-panel-campaigns" role="tabpanel" aria-label={tr("캠페인", "Campaigns")} className="grid min-w-0 gap-4">
      {error && <WidgetState kind={campaigns.length ? "stale" : "error"} title={tr("캠페인을 갱신하지 못했습니다.", "Could not refresh campaigns.")} description={error + (campaigns.length ? tr(" · 이전 내용을 표시 중입니다.", " · Showing the previous snapshot.") : "")} action={<button onClick={() => void legacy.refresh()}>{tr("다시 시도", "Retry")}</button>} />}
      {serverDown && <WidgetState kind={summaries.length || serverDoc ? "stale" : "error"} title={tr("서버 캠페인을 갱신하지 못했습니다.", "Could not refresh server campaigns.")} description={(listError ?? tr("서버가 응답하지 않습니다", "The server is not answering")) + (summaries.length || serverDoc ? tr(" · 마지막 서버 스냅샷을 표시 중이며 중단만 보낼 수 있습니다.", " · Showing the last server snapshot; only stop can be sent.") : "")} />}
      <div className={`grid min-w-0 gap-4 min-[900px]:gap-6 ${folded ? "min-[1280px]:grid-cols-[17.5rem_minmax(0,1fr)]" : "min-[900px]:grid-cols-[17.5rem_minmax(0,1fr)]"}`}>
        {(phone || folded) && <button type="button" className={`campaign-picker !m-0 flex w-full min-w-0 items-center gap-3 !rounded-xl !border !border-[var(--line)] !bg-[var(--th-card-bg)] !px-4 !py-3 text-left ${folded ? "min-[1280px]:!hidden" : ""}`} aria-haspopup="dialog" onClick={() => setPicking(true)}>
            <span className="grid min-w-0 flex-1 gap-0.5">
              <span className="flex min-w-0 items-center gap-2"><strong className="truncate text-sm font-semibold text-[var(--fg)]">{pickerTitle ?? tr("캠페인 고르기", "Choose a campaign")}</strong>{selectedEntry && <Badge status={selectedEntry.status} label={selectedEntry.statusLabel} tr={tr} />}</span>
              <span className="truncate text-xs text-[var(--fg-muted)] tabular-nums">{[selectedEntry && (selectedEntry.kind === "legacy" ? tr("원장", "Ledger") : selectedEntry.repo),
                selectedEntry?.total != null && tr(`작업 ${selectedEntry.done}/${selectedEntry.total} 완료`, `${selectedEntry.done}/${selectedEntry.total} done`), tr(`캠페인 ${entries.length}개 중`, `of ${entries.length} campaigns`)].filter(Boolean).join(" · ")}</span>
            </span>
            <ChevronDown aria-hidden className="size-4 shrink-0 text-[var(--fg-muted)]" />
          </button>}
        {phone ? <BottomSheet open={picking} onClose={() => setPicking(false)} title={tr("캠페인 고르기", "Choose a campaign")}><div className={CAMPAIGN_SCOPE}>{list}</div></BottomSheet>
          : <aside className={`min-w-0 ${folded ? "max-[1279px]:hidden" : ""}`}>{list}</aside>}
        {folded && <Modal open={picking} onClose={() => setPicking(false)} title={tr("캠페인 고르기", "Choose a campaign")} size="sm"><div className={CAMPAIGN_SCOPE}>{list}</div></Modal>}
        <div className="grid min-w-0 content-start gap-4">
          {/* Nothing is called empty until both lists have answered without an error. */}
          {!campaigns.length && !summaries.length && !error && !serverDown && mode === "legacy" && <WidgetState kind={loading || !listLoaded ? "loading" : "empty"} title={loading || !listLoaded ? tr("캠페인을 불러오고 있습니다.", "Loading campaigns.") : tr("등록된 캠페인이 없습니다.", "No campaigns yet.")} description={loading || !listLoaded ? undefined : tr("캠페인이 등록되면 작업 흐름과 진행 상황이 여기에 표시됩니다.", "Registered campaigns and their progress will appear here.")} />}
          {renderForm(NEW_DRAFT, serverDown)}
          {mode === "legacy" ? legacyDetail : serverSelected && (serverDetail
            ?? (detail?.error ? <WidgetState kind="error" title={tr("이 서버 캠페인을 갱신하지 못했습니다.", "Could not refresh this server campaign.")} description={detail.error} />
              : <WidgetState kind={detail && !detail.loading ? "empty" : "loading"} title={detail && !detail.loading ? tr("이 서버 캠페인을 찾을 수 없습니다.", "This server campaign was not found.") : tr("서버 캠페인을 불러오고 있습니다.", "Loading server campaigns.")} />))}
        </div>
      </div>
    </div>}
  </section>
  </PageFrame>;
}
