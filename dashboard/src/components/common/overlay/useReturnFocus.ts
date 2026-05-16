import { useCallback, useLayoutEffect, useRef } from "react";

export function useReturnFocus(open: boolean) {
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const previousOpenRef = useRef(false);

  // #2204 follow-up: capture in a layout-phase effect AND only on the
  // false→true transition. Radix's autofocus runs as a layout effect, so a
  // commit-phase useEffect would see `document.activeElement` already
  // moved INTO the dialog — we'd then "return focus" to a node inside the
  // dialog (which onCloseAutoFocus rejects via the document.contains
  // check, dropping focus to <body> entirely).
  useLayoutEffect(() => {
    if (typeof document === "undefined") return;
    const wasOpen = previousOpenRef.current;
    previousOpenRef.current = open;
    if (open && !wasOpen) {
      previousFocusRef.current = document.activeElement as HTMLElement | null;
    }
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
