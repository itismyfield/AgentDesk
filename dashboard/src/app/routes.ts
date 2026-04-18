export type AppSectionId = "workspace" | "knowledge" | "me";

export type AppRouteId =
  | "home"
  | "office"
  | "agents"
  | "kanban"
  | "stats"
  | "ops"
  | "meetings"
  | "achievements"
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
  showOfficeSelector?: boolean;
}

export interface AppRouteSection {
  id: AppSectionId;
  labelKo: string;
  labelEn: string;
}

export const APP_ROUTE_SECTIONS: AppRouteSection[] = [
  { id: "workspace", labelKo: "워크스페이스", labelEn: "Workspace" },
  { id: "knowledge", labelKo: "지식", labelEn: "Knowledge" },
  { id: "me", labelKo: "나", labelEn: "Me" },
];

export const APP_ROUTES: AppRouteEntry[] = [
  {
    id: "home",
    path: "/home",
    aliases: ["/dashboard"],
    section: "workspace",
    labelKo: "홈",
    labelEn: "Home",
    descriptionKo: "오늘의 상태와 빠른 진입점을 확인합니다.",
    descriptionEn: "See today's overview and quick entry points.",
    paletteIcon: "🏠",
    shortcutKey: "1",
    showOfficeSelector: true,
  },
  {
    id: "office",
    path: "/office",
    section: "workspace",
    labelKo: "오피스",
    labelEn: "Office",
    descriptionKo: "실시간 오피스 씬과 배치를 확인합니다.",
    descriptionEn: "Inspect the live office scene and assignments.",
    paletteIcon: "🏢",
    shortcutKey: "2",
    showOfficeSelector: true,
  },
  {
    id: "agents",
    path: "/agents",
    section: "workspace",
    labelKo: "에이전트",
    labelEn: "Agents",
    descriptionKo: "에이전트, 부서, 파견 세션을 관리합니다.",
    descriptionEn: "Manage agents, departments, and dispatched sessions.",
    paletteIcon: "👥",
    shortcutKey: "3",
    showOfficeSelector: true,
  },
  {
    id: "kanban",
    path: "/kanban",
    section: "workspace",
    labelKo: "칸반",
    labelEn: "Kanban",
    descriptionKo: "작업 상태와 디스패치를 추적합니다.",
    descriptionEn: "Track work status and dispatches.",
    paletteIcon: "📋",
    shortcutKey: "4",
  },
  {
    id: "stats",
    path: "/stats",
    aliases: ["/pulse"],
    section: "workspace",
    labelKo: "통계",
    labelEn: "Stats",
    descriptionKo: "운영 지표와 대시보드 위젯을 봅니다.",
    descriptionEn: "Review operational metrics and dashboard widgets.",
    paletteIcon: "📈",
    shortcutKey: "5",
    showOfficeSelector: true,
  },
  {
    id: "ops",
    path: "/ops",
    aliases: ["/control"],
    section: "workspace",
    labelKo: "운영",
    labelEn: "Ops",
    descriptionKo: "오피스와 운영 표면을 관리합니다.",
    descriptionEn: "Manage offices and operational surfaces.",
    paletteIcon: "🛠️",
    shortcutKey: "6",
  },
  {
    id: "meetings",
    path: "/meetings",
    section: "knowledge",
    labelKo: "회의",
    labelEn: "Meetings",
    descriptionKo: "회의 기록과 후속 이슈를 정리합니다.",
    descriptionEn: "Review meeting records and follow-up issues.",
    paletteIcon: "📝",
    shortcutKey: "7",
  },
  {
    id: "achievements",
    path: "/achievements",
    section: "knowledge",
    labelKo: "업적",
    labelEn: "Achievements",
    descriptionKo: "성과와 랭킹 흐름을 확인합니다.",
    descriptionEn: "Inspect achievements and ranking flow.",
    paletteIcon: "🏆",
    shortcutKey: "8",
  },
  {
    id: "settings",
    path: "/settings",
    section: "me",
    labelKo: "설정",
    labelEn: "Settings",
    descriptionKo: "개인 및 시스템 설정을 조정합니다.",
    descriptionEn: "Adjust personal and system settings.",
    paletteIcon: "⚙️",
    shortcutKey: "9",
  },
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
        route.path === normalizedPath || route.aliases?.includes(normalizedPath),
    ) ?? null
  );
}

export function getSectionById(sectionId: AppSectionId): AppRouteSection {
  return (
    APP_ROUTE_SECTIONS.find((section) => section.id === sectionId) ??
    APP_ROUTE_SECTIONS[0]
  );
}
