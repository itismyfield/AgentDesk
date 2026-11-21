import { createContext, useContext, useLayoutEffect } from "react";

/** Set by the shell: while a page asks, the desktop shell drops its rail and top bar so the page fills the window. */
export const ShellFocusContext = createContext<(focused: boolean) => void>(() => {});

/** Asks for the whole window while `focused` holds, before paint so the canvas is measured at its full size. */
export function useShellFocus(focused: boolean) {
  const setFocused = useContext(ShellFocusContext);
  useLayoutEffect(() => {
    if (!focused) return;
    setFocused(true);
    return () => setFocused(false);
  }, [focused, setFocused]);
}
