import { createContext, useContext, useLayoutEffect } from "react";

/** What a page asks the shell to drop: `window` the desktop rail and top bar, `topBar` the phone top bar only. */
export type ShellFocus = "window" | "topBar";

/** Set by the shell: while a page asks, the shell leaves out what the request names so the page fills that space. */
export const ShellFocusContext = createContext<(focus: ShellFocus | null) => void>(() => {});

/** True while a desktop full-window editor holds the window, so background page chrome can mark itself inert. */
export const ShellWindowFocusContext = createContext(false);

export function useShellWindowFocused(): boolean {
  return useContext(ShellWindowFocusContext);
}

/** Asks for `focus` while `focused` holds, before paint so the page is measured at its full size. */
export function useShellFocus(focused: boolean, focus: ShellFocus = "window") {
  const setFocus = useContext(ShellFocusContext);
  useLayoutEffect(() => {
    if (!focused) return;
    setFocus(focus);
    return () => setFocus(null);
  }, [focused, focus, setFocus]);
}
