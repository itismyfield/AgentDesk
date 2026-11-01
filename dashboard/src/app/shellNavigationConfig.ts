import type { AppRouteId, AppSectionId } from "./routes";

export const MOBILE_PRIMARY_ROUTE_IDS: AppRouteId[] = [
  "home",
  "campaigns",
  "routines",
  "ops",
];

// Sections render without headings; the gap alone separates system pages.
export const SIDEBAR_SECTION_ORDER: AppSectionId[] = ["workspace", "system"];
