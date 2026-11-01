export type AppSectionId = "workspace" | "system";

export type AppRouteId =
  | "home"
  | "campaigns"
  | "routines"
  | "ops"
  | "stats"
  | "agents"
  | "settings";

export interface AppRouteEntry {
  id: AppRouteId;
  path: string;
  aliases?: string[];
  section: AppSectionId;
  labelKo: string;
  labelEn: string;
  descriptionKo: string;
  descriptionEn: string;
  paletteIcon: string;
  shortcutKey: string;
}

export const APP_ROUTES: AppRouteEntry[] = [
  {
    id: "home",
    path: "/home",
    aliases: ["/dashboard"],
    section: "workspace",
    labelKo: "홈",
    labelEn: "Home",
    descriptionKo: "지금 일하는 에이전트와 확인할 일을 한눈에 봐요.",
    descriptionEn: "See who is working and what needs your attention.",
    paletteIcon: "🏠",
    shortcutKey: "1",
  },
  {
    id: "campaigns",
    path: "/campaigns",
    section: "workspace",
    labelKo: "캠페인",
    labelEn: "Campaigns",
    descriptionKo: "작업 흐름과 대기열, 자동 진행을 관리해요.",
    descriptionEn: "Manage task flow, the queue and auto-run.",
    paletteIcon: "🧭",
    shortcutKey: "2",
  },
  {
    id: "routines",
    path: "/routines",
    section: "workspace",
    labelKo: "루틴",
    labelEn: "Routines",
    descriptionKo: "예약된 자동 작업과 최근 결과를 봐요.",
    descriptionEn: "Review scheduled jobs and their latest results.",
    paletteIcon: "🗓️",
    shortcutKey: "3",
  },
  {
    id: "ops",
    path: "/ops",
    aliases: ["/control"],
    section: "workspace",
    labelKo: "운영",
    labelEn: "Ops",
    descriptionKo: "시스템 상태와 전달 지연을 살펴봐요.",
    descriptionEn: "Monitor system health and delivery delays.",
    paletteIcon: "🛠️",
    shortcutKey: "4",
  },
  {
    id: "stats",
    path: "/stats",
    aliases: ["/pulse"],
    section: "workspace",
    labelKo: "통계",
    labelEn: "Stats",
    descriptionKo: "토큰 사용량과 비용을 기간별로 봐요.",
    descriptionEn: "Review token usage and cost by period.",
    paletteIcon: "📈",
    shortcutKey: "5",
  },
  {
    id: "agents",
    path: "/agents",
    section: "workspace",
    labelKo: "에이전트",
    labelEn: "Agents",
    descriptionKo: "에이전트, 부서, 스킬을 관리해요.",
    descriptionEn: "Manage agents, departments and skills.",
    paletteIcon: "👥",
    shortcutKey: "6",
  },
  {
    id: "settings",
    path: "/settings",
    section: "system",
    labelKo: "설정",
    labelEn: "Settings",
    descriptionKo: "개인 설정과 시스템 연결을 바꿔요.",
    descriptionEn: "Adjust personal settings and system connections.",
    paletteIcon: "⚙️",
    shortcutKey: "9",
  },
];

// Removed pages keep their old URLs working.
export const LEGACY_ROUTE_REDIRECTS: Array<{ path: string; to: string }> = [
  { path: "/kanban", to: "/campaigns?tab=queue" },
  { path: "/meetings", to: "/agents?tab=skills" },
  { path: "/achievements", to: "/stats" },
  { path: "/voice", to: "/home" },
];

export const PRIMARY_ROUTES = APP_ROUTES;

export const PALETTE_ROUTES = PRIMARY_ROUTES.map((route) => ({
  id: route.path,
  labelKo: route.labelKo,
  labelEn: route.labelEn,
  icon: route.paletteIcon,
}));

export const DEFAULT_ROUTE_PATH = "/home";

export function normalizeRoutePath(pathname: string): string {
  if (!pathname || pathname === "/") return "/";
  return pathname.replace(/\/+$/, "");
}

export function findRouteByPath(pathname: string): AppRouteEntry | null {
  const normalizedPath = normalizeRoutePath(pathname);
  return (
    PRIMARY_ROUTES.find(
      (route) =>
        route.path === normalizedPath ||
        normalizedPath.startsWith(`${route.path}/`) ||
        route.aliases?.includes(normalizedPath),
    ) ?? null
  );
}
