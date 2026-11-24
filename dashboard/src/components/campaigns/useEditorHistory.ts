import { useCallback, useEffect, useRef } from "react";

/** Shared history state key for full-window campaign editors (relations & pipeline). */
export const EDITOR_HISTORY_KEY = "adkCampaignEditor";
const LEGACY_PIPELINE_KEY = "adkPipelineEditor";

let tokenSeq = 0;
const orphanedTokens = new Set<string>();

function stripEditorMarks(state: unknown): Record<string, unknown> {
  if (!state || typeof state !== "object") return {};
  const next = { ...(state as Record<string, unknown>) };
  delete next[EDITOR_HISTORY_KEY];
  delete next[LEGACY_PIPELINE_KEY];
  return next;
}

export function currentEditorToken(): string | null {
  const value = window.history.state?.[EDITOR_HISTORY_KEY];
  return typeof value === "string" ? value : null;
}

/** True when the current history entry is an orphaned editor entry left behind before navigating to another page. */
export function isOrphanedEditorEntry(): boolean {
  const token = currentEditorToken();
  return token !== null && orphanedTokens.has(token);
}

/** Shared browser history & back-navigation state machine for full-window campaign editors (solves #8369 P2-4). */
export function useEditorHistory({ active, enabled = true, editorUrl, onBack }: {
  active: boolean; enabled?: boolean; editorUrl?: string; onBack: () => void;
}) {
  const live = useRef(true);
  const tokenRef = useRef<string | null>(null);
  const pushedRef = useRef(false);
  const closedRef = useRef(false);
  const backRef = useRef(onBack); backRef.current = onBack;
  const urlRef = useRef(editorUrl); urlRef.current = editorUrl;

  useEffect(() => { live.current = true; return () => { live.current = false; }; }, []);

  useEffect(() => {
    if (active) return;
    const token = currentEditorToken();
    if (token && orphanedTokens.has(token)) {
      orphanedTokens.delete(token);
      window.history.replaceState(stripEditorMarks(window.history.state), "");
      window.history.back();
    }
  }, [active]);

  const wasActive = useRef(active);
  useEffect(() => {
    if (!active || !enabled) { wasActive.current = active; return; }
    closedRef.current = false;
    const existing = currentEditorToken();
    const token = existing && !orphanedTokens.has(existing) ? existing : `ed-${++tokenSeq}`;
    orphanedTokens.delete(token);
    tokenRef.current = token;
    const nextState = { ...stripEditorMarks(window.history.state), [EDITOR_HISTORY_KEY]: token };
    if (!wasActive.current && existing !== token) {
      pushedRef.current = true;
      window.history.pushState(nextState, "", urlRef.current ?? window.location.href);
    } else if (existing !== token) {
      pushedRef.current = false;
      window.history.replaceState(nextState, "", urlRef.current ?? window.location.href);
    }
    wasActive.current = true;
    return () => {
      if (!closedRef.current && pushedRef.current && tokenRef.current && !live.current) orphanedTokens.add(tokenRef.current);
    };
  }, [active, enabled]);

  const restoreEntry = useCallback(() => {
    if (!enabled) return;
    const token = tokenRef.current ?? `ed-${++tokenSeq}`;
    tokenRef.current = token; pushedRef.current = true;
    window.history.pushState({ ...stripEditorMarks(window.history.state), [EDITOR_HISTORY_KEY]: token }, "", urlRef.current ?? window.location.href);
  }, [enabled]);

  const releaseEntry = useCallback(() => {
    closedRef.current = true;
    const token = tokenRef.current; tokenRef.current = null;
    if (token) orphanedTokens.delete(token);
    const onMarked = token !== null && currentEditorToken() === token;
    if (pushedRef.current && live.current && onMarked) {
      pushedRef.current = false;
      window.history.replaceState(stripEditorMarks(window.history.state), "");
      window.history.back();
    } else {
      pushedRef.current = false;
      if (onMarked) window.history.replaceState(stripEditorMarks(window.history.state), "");
    }
  }, []);

  useEffect(() => {
    if (!active || !enabled) return;
    const handlePop = () => { if (currentEditorToken() !== tokenRef.current) backRef.current(); };
    window.addEventListener("popstate", handlePop, true);
    return () => window.removeEventListener("popstate", handlePop, true);
  }, [active, enabled]);

  return { restoreEntry, releaseEntry };
}

export function resetOrphanedEditorEntries() { orphanedTokens.clear(); }
