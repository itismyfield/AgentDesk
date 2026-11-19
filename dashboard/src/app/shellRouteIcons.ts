import {
  BarChart3,
  CalendarClock,
  GitBranch,
  Home,
  ScanText,
  SlidersHorizontal,
  Users,
} from "lucide-react";

import type { AppRouteId } from "./routes";

export function iconForRoute(routeId: AppRouteId) {
  switch (routeId) {
    case "home":
      return Home;
    case "campaigns":
      return GitBranch;
    case "routines":
      return CalendarClock;
    case "stats":
      return BarChart3;
    case "agents":
      return Users;
    case "audit":
      return ScanText;
    case "settings":
      return SlidersHorizontal;
  }
}
