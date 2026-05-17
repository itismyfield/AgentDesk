import { useCallback, useInsertionEffect, useLayoutEffect, useRef } from "react";

export function useReturnFocus(open: boolean) {
  const previousFocusRef = useRef<HTMLElement | null>(null);
  const previousOpenRef = useRef(false);

  const restorePreviousFocus = useCallback(() => {
    const previousFocus = previousFocusRef.current;
    if (!previousFocus || typeof document === "undefined") return false;
    if (!document.contains(previousFocus)) return false;

    window.setTimeout(() => {
      if (document.contains(previousFocus)) {
        previousFocus.focus();
      }
    }, 0);
    return true;
  }, []);

  // Capture before Radix/Vaul layout autofocus can move activeElement into
  // the overlay. Capturing after that point can leave us trying to restore
  // focus to an unmounted dialog node, which drops focus to <body>.
  useInsertionEffect(() => {
    if (typeof document === "undefined") return;
    const wasOpen = previousOpenRef.current;
    if (open && !wasOpen) {
      previousFocusRef.current = document.activeElement as HTMLElement | null;
    }
  }, [open]);

  useLayoutEffect(() => {
    const wasOpen = previousOpenRef.current;
    previousOpenRef.current = open;
    if (!open && wasOpen) {
      restorePreviousFocus();
    }
  }, [open, restorePreviousFocus]);

  return useCallback((event: Event) => {
    if (restorePreviousFocus()) {
      event.preventDefault();
    }
  }, [restorePreviousFocus]);
}
