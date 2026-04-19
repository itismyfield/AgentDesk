import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { CompanySettings, DashboardStats, WSEvent } from "../types";
import type { UiLanguage } from "../i18n";
import * as api from "../api/client";
import { useOffice } from "./OfficeContext";

// ── Context value ──

interface SettingsContextValue {
  settings: CompanySettings;
  setSettings: React.Dispatch<React.SetStateAction<CompanySettings>>;
  stats: DashboardStats | null;
  refreshStats: () => void;
  /** True while stats refresh is in flight */
  refreshingStats: boolean;
  isKo: boolean;
  locale: UiLanguage;
  tr: (ko: string, en: string) => string;
}

const SettingsContext = createContext<SettingsContextValue | null>(null);

// ── Provider (must be nested inside OfficeProvider) ──

interface SettingsProviderProps {
  initialSettings: CompanySettings;
  initialStats: DashboardStats | null;
  children: ReactNode;
}

export function SettingsProvider({ initialSettings, initialStats, children }: SettingsProviderProps) {
  const { selectedOfficeId } = useOffice();

  const [settings, setSettings] = useState<CompanySettings>(initialSettings);
  const [stats, setStats] = useState<DashboardStats | null>(initialStats);

  const [refreshingStats, setRefreshingStats] = useState(false);
  const refreshStats = useCallback(() => {
    setRefreshingStats(true);
    api.getStats(selectedOfficeId ?? undefined)
      .then(setStats)
      .catch(() => {})
      .finally(() => setRefreshingStats(false));
  }, [selectedOfficeId]);

  // Reload stats when office selection changes (skip mount — bootstrap data is fresh)
  const mountedRef = useRef(false);
  useEffect(() => {
    if (!mountedRef.current) {
      mountedRef.current = true;
      return;
    }
    refreshStats();
  }, [refreshStats]);

  // WS events that affect stats
  useEffect(() => {
    function handleWs(e: Event) {
      const event = (e as CustomEvent<WSEvent>).detail;
      switch (event.type) {
        case "kanban_card_created":
        case "kanban_card_updated":
        case "kanban_card_deleted":
          refreshStats();
          break;
      }
    }
    window.addEventListener("pcd-ws-event", handleWs);
    return () => window.removeEventListener("pcd-ws-event", handleWs);
  }, [refreshStats]);

  const isKo = settings.language === "ko";
  const locale = settings.language;
  const tr = useCallback((ko: string, en: string) => (settings.language === "ko" ? ko : en), [settings.language]);

  return (
    <SettingsContext.Provider value={{ settings, setSettings, stats, refreshStats, refreshingStats, isKo, locale, tr }}>
      {children}
    </SettingsContext.Provider>
  );
}

// ── Hook ──

export function useSettings(): SettingsContextValue {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings must be used within SettingsProvider");
  return ctx;
}
