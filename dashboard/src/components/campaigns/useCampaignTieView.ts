import { useCallback, useEffect, useRef, useState } from "react";
import { rawEdgeId, type GraphViewSettings, type RawEdgeKey, type ViewScope } from "./campaignGraphRouting";
import { UNDO_LIMIT, commitTie, pruneTies, undoTie, type TieCommand, type TieHistory } from "./campaignTieEditor";
import { campaignViewKey, loadCampaignView, pruneStoredView, readStoredRaw, saveCampaignView, subscribeCampaignView } from "./campaignViewStorage";

type TieViewState = {
  key: string;
  history: TieHistory;
  recovered: boolean;
  saveFailed: boolean;
  /** Another tab's view that arrived during a drag, a selection draft or a failed save; never applied over them. */
  pending: GraphViewSettings | null;
  /** The stored value this tab last read or wrote, to tell its own writes from another tab's. */
  lastRaw: string | null;
};

function initialState(scope: ViewScope): TieViewState {
  const stored = loadCampaignView(scope);
  return { key: campaignViewKey(scope), history: { view: stored.view, undo: [] }, recovered: stored.recovered, saveFailed: false, pending: null, lastRaw: stored.raw };
}

const sameView = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/**
 * Tie layout of one graph scope: commands, undo and a save that reports failure while keeping the shown view.
 * `present` is every raw edge of the full definition (not the filtered drawing); an empty definition is real.
 */
export function useCampaignTieView(scope: ViewScope, present: readonly RawEdgeKey[]) {
  const key = campaignViewKey(scope);
  const presentKey = present.map(rawEdgeId).sort().join("\n");
  const [state, setState] = useState(() => initialState(scope));
  const stateRef = useRef(state);
  const scopeRef = useRef(scope);
  const presentRef = useRef(present);
  const busyRef = useRef(false);
  scopeRef.current = scope;
  presentRef.current = present;
  if (stateRef.current.key !== key) stateRef.current = initialState(scope);
  const current = stateRef.current.key === state.key ? state : stateRef.current;

  const update = useCallback((next: TieViewState) => {
    stateRef.current = next;
    setState(next);
  }, []);

  /** Drops members that left the definition. */
  const clean = useCallback((view: GraphViewSettings) => pruneTies(view, presentRef.current), []);

  const persist = useCallback((history: TieHistory, pending: GraphViewSettings | null = null) => {
    const result = saveCampaignView(scopeRef.current, history.view);
    const base = { ...stateRef.current, history, pending };
    update(result.ok ? { ...base, saveFailed: false, recovered: false, lastRaw: result.raw } : { ...base, saveFailed: true });
  }, [update]);

  const adopt = useCallback((view: GraphViewSettings, raw: string | null) => {
    const s = stateRef.current;
    update({ ...s, history: { view, undo: [...s.history.undo, s.history.view].slice(-UNDO_LIMIT) }, pending: null, lastRaw: raw });
  }, [update]);

  // Resubscribes when the key names another scope; the listener reads the latest scope through the ref.
  useEffect(() => subscribeCampaignView(scopeRef.current, () => {
    const s = stateRef.current;
    const written = readStoredRaw(scopeRef.current);
    if (written === s.lastRaw) return;
    // A tab with an older definition may still list members deleted here; store the view without them.
    const raw = pruneStoredView(scopeRef.current, presentRef.current, written);
    const stored = loadCampaignView(scopeRef.current, raw);
    if (stored.recovered) return;
    if (busyRef.current || s.saveFailed) update({ ...s, pending: stored.view, lastRaw: raw });
    else adopt(stored.view, raw);
  }), [key, adopt, update]);

  // A changed edge set (not a status-only refresh) removes deleted members from the view, the undo steps and
  // a held view, and saves the cleaned view so a dependency added back later is not tied again.
  useEffect(() => {
    const s = stateRef.current;
    const view = clean(s.history.view);
    const undo = s.history.undo.map(clean);
    const pending = s.pending && clean(s.pending);
    if (sameView(view, s.history.view)) {
      if (!sameView(undo, s.history.undo) || !sameView(pending, s.pending)) update({ ...s, history: { view: s.history.view, undo }, pending });
    } else persist({ view, undo }, pending);
  }, [presentKey, key, clean, persist, update]);

  const commit = useCallback((command: TieCommand) => {
    const s = stateRef.current;
    const base = clean(s.pending ?? s.history.view);
    const history = commitTie(s.history, command, base);
    if (history === s.history) return null;
    persist(history);
    return history.view;
  }, [clean, persist]);

  const undo = useCallback(() => {
    const s = stateRef.current;
    if (s.history.undo.length === 0) return;
    const history = undoTie(s.history);
    persist({ ...history, view: clean(history.view) });
  }, [clean, persist]);

  const setShowOriginal = useCallback((showOriginal: boolean) => {
    const s = stateRef.current;
    persist({ ...s.history, view: { ...s.history.view, showOriginal } });
  }, [persist]);

  const retrySave = useCallback(() => persist(stateRef.current.history), [persist]);
  const adoptPending = useCallback(() => {
    const s = stateRef.current;
    if (s.pending) adopt(s.pending, s.lastRaw);
  }, [adopt]);

  /** A drag, a stroke being drawn or a selection draft holds back another tab's view until it ends. */
  const setBusy = useCallback((busy: boolean) => {
    busyRef.current = busy;
    const s = stateRef.current;
    if (!busy && s.pending && !s.saveFailed) adopt(s.pending, s.lastRaw);
  }, [adopt]);

  return {
    view: current.history.view,
    canUndo: current.history.undo.length > 0,
    saveFailed: current.saveFailed,
    recovered: current.recovered,
    pending: current.pending !== null,
    commit, undo, setShowOriginal, retrySave, adoptPending, setBusy,
  };
}
