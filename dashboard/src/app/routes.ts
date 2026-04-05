export type ViewMode = "office" | "dashboard" | "kanban" | "agents" | "meetings" | "chat" | "skills" | "settings";

export interface RouteEntry {
  id: ViewMode;
  icon: string;
  labelKo: string;
  labelEn: string;
  /** Show in sidebar/bottom nav */
  nav: boolean;
  /** Show in command palette */
  palette: boolean;
  /** Lazy loading fallback labels */
  loadingKo: string;
  loadingEn: string;
}

export const VIEW_REGISTRY: RouteEntry[] = [
  { id: "office", icon: "🏢", labelKo: "오피스", labelEn: "Office", nav: true, palette: true, loadingKo: "오피스 로딩 중...", loadingEn: "Loading Office..." },
  { id: "dashboard", icon: "📊", labelKo: "대시보드", labelEn: "Dashboard", nav: true, palette: true, loadingKo: "대시보드 로딩 중...", loadingEn: "Loading Dashboard..." },
  { id: "kanban", icon: "📋", labelKo: "칸반", labelEn: "Kanban", nav: true, palette: true, loadingKo: "칸반 로딩 중...", loadingEn: "Loading Kanban..." },
  { id: "agents", icon: "👥", labelKo: "직원", labelEn: "Staff", nav: true, palette: true, loadingKo: "직원 로딩 중...", loadingEn: "Loading Agents..." },
  { id: "meetings", icon: "📝", labelKo: "회의", labelEn: "Meetings", nav: true, palette: true, loadingKo: "회의 로딩 중...", loadingEn: "Loading Meetings..." },
  { id: "chat", icon: "💬", labelKo: "채팅", labelEn: "Chat", nav: true, palette: true, loadingKo: "채팅 로딩 중...", loadingEn: "Loading Chat..." },
  { id: "skills", icon: "🧩", labelKo: "스킬", labelEn: "Skills", nav: true, palette: true, loadingKo: "스킬 로딩 중...", loadingEn: "Loading Skills..." },
  { id: "settings", icon: "⚙️", labelKo: "설정", labelEn: "Settings", nav: true, palette: true, loadingKo: "설정 로딩 중...", loadingEn: "Loading Settings..." },
];

export const NAV_ROUTES = VIEW_REGISTRY.filter((r) => r.nav);
export const PALETTE_ROUTES = VIEW_REGISTRY.filter((r) => r.palette);
