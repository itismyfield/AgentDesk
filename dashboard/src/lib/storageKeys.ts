export type DashboardThemePreference = "dark" | "light";
export type DashboardAccentId = "indigo" | "violet" | "amber" | "rose" | "cyan" | "lime";

export const STORAGE_KEYS = {
  language: "climpire.language",
  languageUserSet: "climpire.language.user_set",
  theme: "agentdesk.theme",
  accent: "agentdesk.accent",
  sidebarCollapsed: "agentdesk.sidebar.collapsed",
  homeOrder: "agentdesk.home.order",
  homeEditing: "agentdesk.home.editing",
  fsmDraft: "agentdesk.fsm.v2",
  kanbanDrawerLastId: "agentdesk.kanban.drawer.lastId",
  dashboardBottleneckThresholds: "agentdesk:dashboard:bottleneck-thresholds",
  dashboardActiveTab: "agentdesk.dashboard.active-tab",
  settingsPanel: "agentdesk.settings.active-panel",
  settingsRuntimeCategory: "agentdesk.settings.runtime-category",
  onboardingDraft: "agentdesk.onboarding.draft.v1",
  meetingChannelId: "pcd_meeting_channel_id",
  meetingFixedParticipants: "pcd_meeting_fixed_participants",
} as const;
