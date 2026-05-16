import type { AppRouteId } from "./routes";

export const MOBILE_PRIMARY_ROUTE_IDS: AppRouteId[] = [
  "home",
  "office",
  "kanban",
  "stats",
];

export const SIDEBAR_SECTION_ORDER: Array<{
  id: "workspace" | "extensions" | "me";
  labelKo: string;
  labelEn: string;
}> = [
  {
    id: "workspace",
    labelKo: "워크스페이스",
    labelEn: "Workspace",
  },
  {
    id: "extensions",
    labelKo: "확장",
    labelEn: "Extensions",
  },
  {
    id: "me",
    labelKo: "나",
    labelEn: "Me",
  },
];
