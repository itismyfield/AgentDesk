import { useCallback, useEffect, useRef } from "react";

export function useReturnFocus(open: boolean) {
  const previousFocusRef = useRef<HTMLElement | null>(null);

  useEffect(() => {
    if (!open || typeof document === "undefined") return;
    previousFocusRef.current = document.activeElement as HTMLElement | null;
  }, [open]);

  return useCallback((event: Event) => {
    const previousFocus = previousFocusRef.current;
    if (!previousFocus || typeof document === "undefined") return;
    if (!document.contains(previousFocus)) return;

    event.preventDefault();
    window.setTimeout(() => {
      previousFocus.focus();
    }, 0);
  }, []);
}
