import { useCallback, useEffect, useRef, useState } from "react";
import { rawEdgeId, type GraphViewSettings, type RawEdgeKey, type ViewScope } from "./campaignGraphRouting";
import { UNDO_LIMIT, commitTie, undoTie, type TieCommand, type TieHistory } from "./campaignTieEditor";
import { campaignViewKey, definitionCleaner, loadCampaignView, readStoredRaw, saveCampaignView, subscribeCampaignView } from "./campaignViewStorage";

type TieViewState = {
  key: string;
  history: TieHistory;
  recovered: boolean;
  saveFailed: boolean;
  /** Another tab's view that arrived during a drag, a selection draft or a failed save; never applied over them. */
  pending: GraphViewSettings | null;
  /** The stored value this tab last read or wrote, to tell its own writes from another tab's. */
  lastRaw: string | null;
  /** Definition revision recorded with that stored value; null when it has none. */
  revision: number | null;
};

function initialState(scope: ViewScope): TieViewState {
  const stored = loadCampaignView(scope);
  return { key: campaignViewKey(scope), history: { view: stored.view, undo: [] }, recovered: stored.recovered, saveFailed: false, pending: null, lastRaw: stored.raw, revision: stored.revision };
}

const sameView = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

/**
 * Tie layout of one graph scope: commands, undo and a save that reports failure while keeping the shown view.
 * `present` is every raw edge of the full definition at `revision` (not the filtered drawing); an empty definition
 * is real.
 */
export function useCampaignTieView(scope: ViewScope, present: readonly RawEdgeKey[], revision: number) {
  const key = campaignViewKey(scope);
  const presentKey = present.map(rawEdgeId).sort().join("\n");
  const [state, setState] = useState(() => initialState(scope));
  const stateRef = useRef(state);
  const scopeRef = useRef(scope);
  const presentRef = useRef(present);
  const revisionRef = useRef(revision);
  const busyRef = useRef(false);
  scopeRef.current = scope;
  presentRef.current = present;
  revisionRef.current = revision;
  if (stateRef.current.key !== key) stateRef.current = initialState(scope);
  const current = stateRef.current.key === state.key ? state : stateRef.current;

  const update = useCallback((next: TieViewState) => {
    stateRef.current = next;
    setState(next);
  }, []);

  /** Drops members that left the definition, or null while the stored view was judged by a newer definition. */
  const cleaner = useCallback(() => definitionCleaner(presentRef.current, revisionRef.current, stateRef.current.revision), []);

  /** Records the higher of this definition's revision and the stored one, so an older tab never lowers it. */
  const persist = useCallback((history: TieHistory, pending: GraphViewSettings | null = null) => {
    const s = stateRef.current;
    const recorded = Math.max(revisionRef.current, s.revision ?? revisionRef.current);
    const result = saveCampaignView(scopeRef.current, history.view, recorded);
    const base = { ...s, history, pending };
    update(result.ok ? { ...base, saveFailed: false, recovered: false, lastRaw: result.raw, revision: recorded } : { ...base, saveFailed: true });
  }, [update]);

  const adopt = useCallback((view: GraphViewSettings) => {
    const s = stateRef.current;
    update({ ...s, history: { view, undo: [...s.history.undo, s.history.view].slice(-UNDO_LIMIT) }, pending: null });
  }, [update]);

  // Resubscribes when the key names another scope; the listener reads the latest scope through the ref.
  useEffect(() => subscribeCampaignView(scopeRef.current, () => {
    const s = stateRef.current;
    const raw = readStoredRaw(scopeRef.current);
    if (raw === s.lastRaw) return;
    // Taken as written and never saved back: this tab's definition may be older than the writer's. Members it
    // does not know are left out of the drawing by routeEdges and kept for when its definition catches up.
    const stored = loadCampaignView(scopeRef.current, raw);
    if (stored.recovered) return;
    update({ ...s, pending: stored.view, lastRaw: raw, revision: stored.revision });
    if (!busyRef.current && !s.saveFailed) adopt(stored.view);
  }), [key, adopt, update]);

  // A changed edge set (not a status-only refresh) removes deleted members from the view, the undo steps and
  // a held view, and saves the cleaned view so a dependency added back later is not tied again.
  useEffect(() => {
    const s = stateRef.current;
    const clean = cleaner();
    if (!clean) return;
    const view = clean(s.history.view);
    const undo = s.history.undo.map(clean);
    const pending = s.pending && clean(s.pending);
    if (sameView(view, s.history.view)) {
      if (!sameView(undo, s.history.undo) || !sameView(pending, s.pending)) update({ ...s, history: { view: s.history.view, undo }, pending });
    } else persist({ view, undo }, pending);
  }, [presentKey, revision, key, cleaner, persist, update]);

  const commit = useCallback((command: TieCommand) => {
    const s = stateRef.current;
    const shown = s.pending ?? s.history.view;
    const clean = cleaner();
    const history = commitTie(s.history, command, clean ? clean(shown) : shown);
    if (history === s.history) return null;
    persist(history);
    return history.view;
  }, [cleaner, persist]);

  const undo = useCallback(() => {
    const s = stateRef.current;
    if (s.history.undo.length === 0) return;
    const history = undoTie(s.history);
    const clean = cleaner();
    persist(clean ? { ...history, view: clean(history.view) } : history);
  }, [cleaner, persist]);

  const setShowOriginal = useCallback((showOriginal: boolean) => {
    const s = stateRef.current;
    persist({ ...s.history, view: { ...s.history.view, showOriginal } });
  }, [persist]);

  const retrySave = useCallback(() => persist(stateRef.current.history), [persist]);
  const adoptPending = useCallback(() => {
    const s = stateRef.current;
    if (s.pending) adopt(s.pending);
  }, [adopt]);

  /** A drag, a stroke being drawn or a selection draft holds back another tab's view until it ends. */
  const setBusy = useCallback((busy: boolean) => {
    busyRef.current = busy;
    const s = stateRef.current;
    if (!busy && s.pending && !s.saveFailed) adopt(s.pending);
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
