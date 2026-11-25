import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import type { CompanySettings, DashboardStats, WSEvent } from "../types";
import type { UiLanguage } from "../i18n";
import * as api from "../api/client";

// ── Context value ──

interface SettingsContextValue {
  settings: CompanySettings;
  setSettings: React.Dispatch<React.SetStateAction<CompanySettings>>;
  stats: DashboardStats | null;
  initialStatsPending?: boolean;
  refreshStats: () => void;
  /** True while stats refresh is in flight */
  refreshingStats: boolean;
  isKo: boolean;
  locale: UiLanguage;
  tr: (ko: string, en: string) => string;
}

const SettingsContext = createContext<SettingsContextValue | null>(null);

// ── Provider ──

interface SettingsProviderProps {
  initialSettings: CompanySettings;
  initialStats: DashboardStats | null;
  initialStatsPending?: boolean;
  children: ReactNode;
}

export function SettingsProvider({ initialSettings, initialStats, initialStatsPending = false, children }: SettingsProviderProps) {
  const [settings, setSettings] = useState<CompanySettings>(initialSettings);
  const [stats, setStats] = useState<DashboardStats | null>(initialStats);

  const [refreshingStats, setRefreshingStats] = useState(false);
  const statsRead = useRef(0);

  useEffect(() => {
    if (initialStats !== null && statsRead.current === 0) {
      setStats((prev) => prev ?? initialStats);
    }
  }, [initialStats]);
  const refreshStats = useCallback(() => {
    const read = ++statsRead.current;
    setRefreshingStats(true);
    api.getStats()
      .then((next) => { if (read === statsRead.current) setStats(next); })
      .catch(() => {})
      .finally(() => { if (read === statsRead.current) setRefreshingStats(false); });
  }, []);

  // #2050 P2 finding 6 — debounce kanban_card_* refresh.
  const debounceTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    function handleWs(e: Event) {
      const event = (e as CustomEvent<WSEvent>).detail;
      switch (event.type) {
        case "kanban_card_created":
        case "kanban_card_updated":
        case "kanban_card_deleted":
          if (debounceTimerRef.current) clearTimeout(debounceTimerRef.current);
          debounceTimerRef.current = setTimeout(() => {
            debounceTimerRef.current = null;
            refreshStats();
          }, 350);
          break;
      }
    }
    window.addEventListener("pcd-ws-event", handleWs);
    return () => {
      window.removeEventListener("pcd-ws-event", handleWs);
      if (debounceTimerRef.current) {
        clearTimeout(debounceTimerRef.current);
        debounceTimerRef.current = null;
      }
    };
  }, [refreshStats]);

  const isKo = settings.language === "ko";
  const locale = settings.language;
  const tr = useCallback((ko: string, en: string) => (settings.language === "ko" ? ko : en), [settings.language]);

  return (
    <SettingsContext.Provider value={{ settings, setSettings, stats, initialStatsPending, refreshStats, refreshingStats, isKo, locale, tr }}>
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
