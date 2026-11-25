import { useCallback, useEffect, useRef, useState } from "react";

export interface DraftLeaveGuard {
  confirmOpen: boolean;
  confirmLeave: (proceed: () => void) => void;
  acceptLeave: () => void;
  cancelLeave: () => void;
}

/** Guards unsaved drafts with `beforeunload` and provides state for an in-app leave confirmation dialog. */
export function useDraftLeaveGuard(dirty: boolean): DraftLeaveGuard {
  const [confirmOpen, setConfirmOpen] = useState(false);
  const dirtyRef = useRef(dirty);
  dirtyRef.current = dirty;
  const pendingProceedRef = useRef<(() => void) | null>(null);

  useEffect(() => {
    if (!dirty || typeof window === "undefined") return;
    const handleBeforeUnload = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", handleBeforeUnload);
    return () => window.removeEventListener("beforeunload", handleBeforeUnload);
  }, [dirty]);

  const confirmLeave = useCallback((proceed: () => void) => {
    if (!dirtyRef.current) {
      proceed();
      return;
    }
    pendingProceedRef.current = proceed;
    setConfirmOpen(true);
  }, []);

  const acceptLeave = useCallback(() => {
    const proceed = pendingProceedRef.current;
    pendingProceedRef.current = null;
    setConfirmOpen(false);
    proceed?.();
  }, []);

  const cancelLeave = useCallback(() => {
    pendingProceedRef.current = null;
    setConfirmOpen(false);
  }, []);

  return {
    confirmOpen,
    confirmLeave,
    acceptLeave,
    cancelLeave,
  };
}
