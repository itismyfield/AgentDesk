import type { AppRouteId, AppSectionId } from "./routes";

// The fifth tab is "More", a sheet with the remaining menu routes.
export const MOBILE_PRIMARY_ROUTE_IDS: AppRouteId[] = [
  "home",
  "campaigns",
  "agents",
  "audit",
];

// Sections render without headings; the gap alone separates system pages.
export const SIDEBAR_SECTION_ORDER: AppSectionId[] = ["workspace", "system"];
