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
export type Checks = { errors: PipelineDiagnostic[]; warnings: PipelineDiagnostic[]; pending: boolean; failed: string | null };
export type Notice = { kind: "saved"; revision: number } | { kind: "lost" } | { kind: "error"; text: string } | { kind: "conflict"; latest: Detail };

/** A draft of a server campaign's pipeline over the revision it was read at: the server checks it shortly after each
 * change, and a save replaces the pipeline only if the definition has not moved since. */
export function usePipelineEditor({ id, detail, reload, leave, tr }: {
  id: string | null; detail: Detail | null; reload: () => void; leave?: RefObject<((proceed: () => void) => void) | null>; tr: Tr;
}) {
  const [edit, setEdit] = useState<Edit | null>(null);
  const [checks, setChecks] = useState<Checks | null>(null);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [leaving, setLeaving] = useState<(() => void) | null>(null);
  // A key belongs to the exact request it was made for; one that got no answer is resent under it.
  const sent = useRef<{ body: string; key: string } | null>(null);
  const changes = useMemo(() => edit && draftChanges(edit.base, edit.draft), [edit]);
  const dirty = !!changes?.count;
  const draft = edit?.draft;

  useEffect(() => {
    if (!draft || !id) return;
    const abort = new AbortController();
    setChecks((current) => ({ errors: current?.errors ?? [], warnings: current?.warnings ?? [], pending: true, failed: null }));
    const timer = window.setTimeout(() => validateServerCampaignPipeline(id, draft, abort.signal).then(
      ({ errors, warnings }) => setChecks({ errors, warnings, pending: false, failed: null }),
      (cause: unknown) => { if (!abort.signal.aborted) setChecks((current) => ({ errors: [], warnings: [], ...current, pending: false, failed: cause instanceof Error ? cause.message : String(cause) })); }), 400);
    return () => { window.clearTimeout(timer); abort.abort(); };
  }, [id, draft]);

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

  const stop = () => { setEdit(null); setChecks(null); sent.current = null; };
  const begin = (from: Detail) => { setNotice(null); setEdit({ base: from.pipeline, revision: from.definition_revision, draft: from.pipeline }); };
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
    edit, changes, dirty, checks, saving, notice, leaving,
    begin, save,
    update: (change: (draft: PipelineDefinition) => PipelineDefinition) => setEdit((current) => current && { ...current, draft: change(current.draft) }),
    revert: () => setEdit((current) => current && { ...current, draft: current.base }),
    close: () => dirty ? setLeaving(() => stop) : stop(),
    decide: (discard: boolean) => { const proceed = leaving; setLeaving(null); if (discard) { stop(); proceed?.(); } },
  };
}
export type PipelineEditor = ReturnType<typeof usePipelineEditor>;
