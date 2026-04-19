import type { ReactNode } from "react";
import { ToastProvider } from "./ToastProvider";

interface OverlayProviderProps {
  children: ReactNode;
}

// Root provider that wires the overlay subsystems (currently just toast).
// Drawer/Modal/BottomSheet are stateless components — they manage their own
// open state via props — so they don't need provider context.
export function OverlayProvider({ children }: OverlayProviderProps) {
  return <ToastProvider>{children}</ToastProvider>;
}
