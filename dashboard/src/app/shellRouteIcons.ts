import {
  Bell,
  Building2,
  CalendarClock,
  FolderKanban,
  Home,
  LayoutDashboard,
  Settings,
  Trophy,
  Users,
  Wrench,
} from "lucide-react";

import type { AppRouteId } from "./routes";

export function iconForRoute(routeId: AppRouteId) {
  switch (routeId) {
    case "home":
      return Home;
    case "office":
      return Building2;
    case "agents":
      return Users;
    case "kanban":
      return FolderKanban;
    case "routines":
      return CalendarClock;
    case "stats":
      return LayoutDashboard;
    case "ops":
      return Wrench;
    case "meetings":
      return Bell;
    case "achievements":
      return Trophy;
    case "settings":
      return Settings;
  }
}
