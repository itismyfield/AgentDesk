import {
  BarChart3,
  CalendarClock,
  GitBranch,
  Home,
  Settings,
  Users,
  Wrench,
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
    case "ops":
      return Wrench;
    case "stats":
      return BarChart3;
    case "agents":
      return Users;
    case "settings":
      return Settings;
  }
}
