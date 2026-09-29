import {
  Bell,
  CalendarClock,
  GitBranch,
  FolderKanban,
  Home,
  LayoutDashboard,
  Mic,
  Settings,
  Trophy,
  Users,
  Wrench,
} from "lucide-react";

import type { AppRouteId } from "./routes";

export function iconForRoute(routeId: AppRouteId) {
  switch (routeId) {
    case "voice":
      return Mic;
    case "home":
      return Home;
    case "agents":
      return Users;
    case "kanban":
      return FolderKanban;
    case "campaigns":
      return GitBranch;
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
