import { createContext, useContext } from "react";

/** A page pane that drawers inside it dock into (non-modal, beside the list) on wide viewports instead of overlaying. */
export interface DrawerDockTarget {
  /** The pane element; null until it has mounted, while docked drawers wait for it. */
  host: HTMLElement | null;
}

export const DrawerDockContext = createContext<DrawerDockTarget | null>(null);

export function useDrawerDock(): DrawerDockTarget | null {
  return useContext(DrawerDockContext);
}
