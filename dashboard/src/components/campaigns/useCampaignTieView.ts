import { useCallback, useEffect, useRef, useState } from "react";
import type { GraphViewSettings, RawEdgeKey, ViewScope } from "./campaignGraphRouting";
import { UNDO_LIMIT, commitTie, pruneTies, undoTie, type TieCommand, type TieHistory } from "./campaignTieEditor";
import { campaignViewKey, loadCampaignView, readStoredRaw, saveCampaignView, subscribeCampaignView } from "./campaignViewStorage";

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

/** Tie layout of one graph scope: commands, undo and a save that reports failure while keeping the shown view. */
export function useCampaignTieView(scope: ViewScope, present: readonly RawEdgeKey[]) {
  const key = campaignViewKey(scope);
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

  const persist = useCallback((history: TieHistory) => {
    const result = saveCampaignView(scopeRef.current, history.view);
    const base = { ...stateRef.current, history, pending: null };
    update(result.ok ? { ...base, saveFailed: false, recovered: false, lastRaw: result.raw } : { ...base, saveFailed: true });
  }, [update]);

  const adopt = useCallback((view: GraphViewSettings, raw: string | null) => {
    const s = stateRef.current;
    update({ ...s, history: { view, undo: [...s.history.undo, s.history.view].slice(-UNDO_LIMIT) }, pending: null, lastRaw: raw });
  }, [update]);

  // Resubscribes when the key names another scope; the listener reads the latest scope through the ref.
  useEffect(() => subscribeCampaignView(scopeRef.current, () => {
    const s = stateRef.current;
    const raw = readStoredRaw(scopeRef.current);
    if (raw === s.lastRaw) return;
    const stored = loadCampaignView(scopeRef.current, raw);
    if (stored.recovered) return;
    if (busyRef.current || s.saveFailed) update({ ...s, pending: stored.view, lastRaw: raw });
    else adopt(stored.view, raw);
  }), [key, adopt, update]);

  const commit = useCallback((command: TieCommand) => {
    const s = stateRef.current;
    const base = pruneTies(s.pending ?? s.history.view, presentRef.current);
    const history = commitTie(s.history, command, base);
    if (history === s.history) return null;
    persist(history);
    return history.view;
  }, [persist]);

  const undo = useCallback(() => {
    const s = stateRef.current;
    if (s.history.undo.length > 0) persist(undoTie(s.history));
  }, [persist]);

  const setShowOriginal = useCallback((showOriginal: boolean) => {
    const s = stateRef.current;
    persist({ ...s.history, view: { ...s.history.view, showOriginal } });
  }, [persist]);

  const retrySave = useCallback(() => persist(stateRef.current.history), [persist]);
  const adoptPending = useCallback(() => {
    const s = stateRef.current;
    if (s.pending) adopt(s.pending, s.lastRaw);
  }, [adopt]);

  /** A drag or selection draft in progress holds back another tab's view until it ends. */
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
