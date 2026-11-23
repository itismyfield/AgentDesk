import { useCallback, useEffect, useMemo, useRef, useState, type RefObject } from "react";
import {
  getServerCampaignPipeline, pipelineRefusal, saveServerCampaignPipeline, validateServerCampaignPipeline,
  type CampaignPipeline as Detail, type PipelineDefinition, type PipelineDiagnostic,
} from "../../api/serverCampaignPipeline";
import type { Tr } from "./campaignPresentation";
import { diagnosticPlace, draftChanges } from "./pipelineEdit";
import { warnBeforeUnload } from "./relationsEditorModel";
import { newIdempotencyKey, rejectionText } from "./serverCampaignModel";

type Edit = { base: PipelineDefinition; revision: number; draft: PipelineDefinition };
type Sent = { body: string; key: string };
export type Checks = { errors: PipelineDiagnostic[]; warnings: PipelineDiagnostic[]; pending: boolean; failed: string | null };
export type Notice = { kind: "saved"; revision: number } | { kind: "lost" } | { kind: "error"; text: string } | { kind: "conflict"; latest: Detail };

/** Drafts left without a decision (another page, the back button) for this tab's session, by campaign. They live only
 * in memory, so while any is kept a reload or close asks first; its own listener keeps the editor's removal from dropping it. */
const kept = new Map<string, Edit & { sent: { current: Sent | null } }>();
const keptGuard = (event: BeforeUnloadEvent) => warnBeforeUnload(event);
const guardKept = () => kept.size ? window.addEventListener("beforeunload", keptGuard) : window.removeEventListener("beforeunload", keptGuard);

/** A draft of a server campaign's pipeline over the revision it was read at: the server checks it shortly after each
 * change and again, quietly, whenever the run moves (`runtime`), and a save replaces the pipeline only if the definition has not moved since. */
export function usePipelineEditor({ id, runtime, detail, reload, leave, tr }: {
  id: string | null; runtime: string; detail: Detail | null; reload: () => void; leave?: RefObject<((proceed: () => void) => void) | null>; tr: Tr;
}) {
  const [edit, setEdit] = useState<Edit | null>(null);
  const [waiting, setWaiting] = useState(() => id ? kept.get(id) ?? null : null);
  const [checks, setChecks] = useState<Checks | null>(null);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [leaving, setLeaving] = useState<(() => void) | null>(null);
  // A key belongs to the exact request it was made for; one that got no answer is resent under it.
  const sent = useRef<Sent | null>(null);
  const changes = useMemo(() => edit && draftChanges(edit.base, edit.draft), [edit]);
  const dirty = !!changes?.count;
  const draft = edit?.draft;

  // A recheck of a draft already checked (only the run moved) keeps the last result on screen and the save open meanwhile.
  const checked = useRef<PipelineDefinition | null>(null);
  useEffect(() => {
    if (!draft || !id) return;
    const abort = new AbortController(), quiet = checked.current === draft;
    if (!quiet) setChecks((current) => ({ errors: current?.errors ?? [], warnings: current?.warnings ?? [], pending: true, failed: null }));
    const timer = window.setTimeout(() => validateServerCampaignPipeline(id, draft, abort.signal).then(
      ({ errors, warnings }) => { checked.current = draft; setChecks({ errors, warnings, pending: false, failed: null }); },
      (cause: unknown) => { if (!abort.signal.aborted) setChecks((current) => quiet ? current && { ...current, pending: false } : { errors: [], warnings: [], ...current, pending: false, failed: cause instanceof Error ? cause.message : String(cause) }); }), quiet ? 0 : 400);
    return () => { window.clearTimeout(timer); abort.abort(); };
  }, [id, draft, runtime]);

  // Leaving with unsaved changes: the page asks through `leave` (tabs, another campaign), a reload or close through beforeunload.
  const ask = useCallback((proceed: () => void) => setLeaving(() => proceed), []);
  useEffect(() => {
    if (!leave || !dirty) return;
    leave.current = ask;
    return () => { leave.current = null; };
  }, [leave, dirty, ask]);
  useEffect(() => {
    if (!dirty) return;
    window.addEventListener("beforeunload", warnBeforeUnload);
    return () => window.removeEventListener("beforeunload", warnBeforeUnload);
  }, [dirty]);

  // Leaving any other way keeps the draft to pick up later; a decision (save, discard, close) clears `keep` first.
  const keep = useRef<Edit | null>(null);
  keep.current = dirty ? edit : null;
  useEffect(() => () => { if (id && keep.current) { kept.set(id, { ...keep.current, sent }); guardKept(); } }, [id]);
  const forget = () => { if (id && kept.delete(id)) guardKept(); setWaiting(null); };

  const stop = () => { keep.current = null; checked.current = null; setEdit(null); setChecks(null); sent.current = null; };
  const open = (next: Edit) => { forget(); setNotice(null); setEdit(next); };
  const begin = (from: Detail) => open({ base: from.pipeline, revision: from.definition_revision, draft: from.pipeline });
  const save = async (over?: Detail) => {
    if (!edit || !id || saving) return;
    const revision = over?.definition_revision ?? edit.revision;
    if (over) setEdit({ ...edit, base: over.pipeline, revision });
    const body = JSON.stringify([revision, edit.draft]);
    const key = sent.current?.body === body ? sent.current.key : newIdempotencyKey();
    sent.current = { body, key };
    setSaving(true);
    setNotice(null);
    try {
      const result = await saveServerCampaignPipeline(id, revision, edit.draft, key);
      // The page may have gone while waiting and kept this draft; it is saved now.
      if (kept.delete(id)) guardKept();
      stop();
      setNotice({ kind: "saved", revision: result.definition_revision });
      reload();
    } catch (cause) {
      const refusal = pipelineRefusal(cause);
      if (!refusal || refusal.status >= 500) return setNotice({ kind: "lost" });
      sent.current = null;
      if (refusal.rejection === "stale_definition") {
        // The newer graph shows beside the draft; the draft itself is never overwritten.
        const latest = await getServerCampaignPipeline(id).catch(() => null);
        setNotice(latest ? { kind: "conflict", latest } : { kind: "error", text: tr("다른 곳에서 파이프라인이 바뀌었어요. 초안은 그대로 둬요.", "The pipeline changed elsewhere. Your draft is kept.") });
        return reload();
      }
      if (refusal.diagnostics.length) setChecks({ errors: refusal.diagnostics, warnings: [], pending: false, failed: null });
      const places = refusal.rejection === "state_in_use" ? `: ${refusal.diagnostics.map((value) => diagnosticPlace(value, [edit.draft, edit.base], tr)).join(", ")}` : "";
      setNotice({ kind: "error", text: (refusal.rejection ? rejectionText(refusal.rejection, tr) : refusal.status === 409 ? rejectionText("idempotency_conflict", tr)
        : refusal.status === 422 ? tr("서버 검사에서 오류가 나왔어요. 아래 목록을 확인해 주세요.", "The server's checks found errors. See the list below.") : refusal.message) + places });
    } finally {
      setSaving(false);
    }
  };

  return {
    edit, changes, dirty, checks, saving, notice, leaving, waiting,
    begin, save, forget,
    /** Picks up the kept draft over the revision it was made against; a save then reports any newer one as a conflict. */
    // The key travels with a kept draft (by reference, so an answer that came after leaving still counts).
    resume: () => { if (waiting) { const { sent: last, ...next } = waiting; open(next); sent.current = last.current; } },
    update: (change: (draft: PipelineDefinition) => PipelineDefinition) => setEdit((current) => current && { ...current, draft: change(current.draft) }),
    revert: () => setEdit((current) => current && { ...current, draft: current.base }),
    close: () => dirty ? setLeaving(() => stop) : stop(),
    decide: (discard: boolean) => { const proceed = leaving; setLeaving(null); if (discard) { stop(); proceed?.(); } },
  };
}
export type PipelineEditor = ReturnType<typeof usePipelineEditor>;
