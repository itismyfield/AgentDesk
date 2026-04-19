import { STORAGE_KEYS } from "../lib/storageKeys";
import {
  readLocalStorageValue,
  removeLocalStorageValue,
  writeLocalStorageValue,
} from "../lib/useLocalStorage";

export interface BotInfo {
  valid: boolean;
  bot_id?: string;
  bot_name?: string;
  error?: string;
}

export interface CommandBotEntry {
  provider: "claude" | "codex" | "gemini" | "qwen";
  token: string;
  botInfo: BotInfo | null;
}

export interface AgentDef {
  id: string;
  name: string;
  nameEn?: string;
  description: string;
  descriptionEn?: string;
  prompt: string;
  custom?: boolean;
}

export interface ChannelAssignment {
  agentId: string;
  agentName: string;
  recommendedName: string;
  channelId: string;
  channelName: string;
}

export interface ProviderStatus {
  installed: boolean;
  logged_in: boolean;
  version?: string;
}

export interface OnboardingCompletionState {
  stage?: string;
  partial_apply?: boolean;
  retry_recommended?: boolean;
  last_error?: string | null;
}

export type OnboardingSetupMode = "fresh" | "rerun";
export type OnboardingResumeState = "none" | "draft_available" | "partial_apply";

export interface OnboardingStatusResponse {
  owner_id?: string;
  guild_id?: string;
  bot_tokens?: {
    command?: string;
    announce?: string;
    notify?: string;
    command2?: string;
  };
  bot_providers?: {
    command?: CommandBotEntry["provider"];
    command2?: CommandBotEntry["provider"];
  };
  completion_state?: OnboardingCompletionState;
  partial_apply?: boolean;
  retry_recommended?: boolean;
  draft_available?: boolean;
  setup_mode?: OnboardingSetupMode;
  resume_state?: OnboardingResumeState;
}

export interface OnboardingDraft {
  version: 1;
  updatedAtMs: number;
  step: number;
  commandBots: CommandBotEntry[];
  announceToken: string;
  notifyToken: string;
  announceBotInfo: BotInfo | null;
  notifyBotInfo: BotInfo | null;
  providerStatuses: Record<string, ProviderStatus>;
  selectedTemplate: string | null;
  agents: AgentDef[];
  customName: string;
  customDesc: string;
  customNameEn: string;
  customDescEn: string;
  expandedAgent: string | null;
  selectedGuild: string;
  channelAssignments: ChannelAssignment[];
  ownerId: string;
  hasExistingSetup: boolean;
  confirmRerunOverwrite: boolean;
}

interface ServerOnboardingDraftBotInfo {
  valid?: boolean;
  bot_id?: string;
  bot_name?: string;
  error?: string;
}

interface ServerOnboardingDraftCommandBot {
  provider?: CommandBotEntry["provider"];
  token?: string;
  bot_info?: ServerOnboardingDraftBotInfo | null;
}

interface ServerOnboardingDraftAgent {
  id?: string;
  name?: string;
  name_en?: string;
  description?: string;
  description_en?: string;
  prompt?: string;
  custom?: boolean;
}

interface ServerOnboardingDraftChannelAssignment {
  agent_id?: string;
  agent_name?: string;
  recommended_name?: string;
  channel_id?: string;
  channel_name?: string;
}

interface ServerOnboardingDraft {
  version?: number;
  updated_at_ms?: number;
  step?: number;
  command_bots?: ServerOnboardingDraftCommandBot[];
  announce_token?: string;
  notify_token?: string;
  announce_bot_info?: ServerOnboardingDraftBotInfo | null;
  notify_bot_info?: ServerOnboardingDraftBotInfo | null;
  provider_statuses?: Record<string, ProviderStatus>;
  selected_template?: string | null;
  agents?: ServerOnboardingDraftAgent[];
  custom_name?: string;
  custom_desc?: string;
  custom_name_en?: string;
  custom_desc_en?: string;
  expanded_agent?: string | null;
  selected_guild?: string;
  channel_assignments?: ServerOnboardingDraftChannelAssignment[];
  owner_id?: string;
  has_existing_setup?: boolean;
  confirm_rerun_overwrite?: boolean;
}

export interface ServerOnboardingDraftResponse {
  available?: boolean;
  draft?: ServerOnboardingDraft | null;
  setup_mode?: OnboardingSetupMode;
  resume_state?: OnboardingResumeState;
  completion_state?: OnboardingCompletionState;
  secret_policy?: {
    stores_raw_tokens?: boolean;
    returns_raw_tokens_in_draft?: boolean;
    masked_in_status_after_completion?: boolean;
    cleared_on_complete?: boolean;
    cleared_on_delete?: boolean;
  };
}

export const ONBOARDING_DRAFT_STORAGE_KEY = STORAGE_KEYS.onboardingDraft;

function normalizeBotInfo(parsed: ServerOnboardingDraftBotInfo | BotInfo | null | undefined): BotInfo | null {
  if (!parsed) return null;
  return {
    valid: Boolean(parsed.valid),
    bot_id: parsed.bot_id ?? undefined,
    bot_name: parsed.bot_name ?? undefined,
    error: parsed.error ?? undefined,
  };
}

function normalizeCommandBots(parsed: Partial<OnboardingDraft>["commandBots"]): CommandBotEntry[] {
  if (!Array.isArray(parsed) || parsed.length === 0) {
    return [{ provider: "claude", token: "", botInfo: null }];
  }
  return parsed.map((entry) => ({
    provider: entry?.provider ?? "claude",
    token: entry?.token ?? "",
    botInfo: normalizeBotInfo(entry?.botInfo),
  }));
}

export function normalizeOnboardingDraft(parsed: Partial<OnboardingDraft> | null | undefined): OnboardingDraft | null {
  if (!parsed || parsed.version !== 1) return null;
  return {
    version: 1,
    updatedAtMs: typeof parsed.updatedAtMs === "number" ? parsed.updatedAtMs : 0,
    step: typeof parsed.step === "number" ? parsed.step : 1,
    commandBots: normalizeCommandBots(parsed.commandBots),
    announceToken: parsed.announceToken ?? "",
    notifyToken: parsed.notifyToken ?? "",
    announceBotInfo: normalizeBotInfo(parsed.announceBotInfo),
    notifyBotInfo: normalizeBotInfo(parsed.notifyBotInfo),
    providerStatuses: parsed.providerStatuses ?? {},
    selectedTemplate: parsed.selectedTemplate ?? null,
    agents: parsed.agents ?? [],
    customName: parsed.customName ?? "",
    customDesc: parsed.customDesc ?? "",
    customNameEn: parsed.customNameEn ?? "",
    customDescEn: parsed.customDescEn ?? "",
    expandedAgent: parsed.expandedAgent ?? null,
    selectedGuild: parsed.selectedGuild ?? "",
    channelAssignments: parsed.channelAssignments ?? [],
    ownerId: parsed.ownerId ?? "",
    hasExistingSetup: Boolean(parsed.hasExistingSetup),
    confirmRerunOverwrite: Boolean(parsed.confirmRerunOverwrite),
  };
}

export function isMeaningfulOnboardingDraft(draft: OnboardingDraft | null | undefined): draft is OnboardingDraft {
  if (!draft) return false;
  return (
    draft.step > 1 ||
    draft.commandBots.some((bot) => Boolean(bot.token.trim())) ||
    Boolean(draft.announceToken.trim()) ||
    Boolean(draft.notifyToken.trim()) ||
    Object.keys(draft.providerStatuses).length > 0 ||
    Boolean(draft.selectedTemplate) ||
    draft.agents.length > 0 ||
    Boolean(draft.customName.trim()) ||
    Boolean(draft.customDesc.trim()) ||
    Boolean(draft.customNameEn.trim()) ||
    Boolean(draft.customDescEn.trim()) ||
    Boolean(draft.expandedAgent) ||
    draft.channelAssignments.length > 0 ||
    draft.confirmRerunOverwrite
  );
}

export function readOnboardingDraft(): OnboardingDraft | null {
  const parsed = readLocalStorageValue<Partial<OnboardingDraft> | null>(ONBOARDING_DRAFT_STORAGE_KEY, null);
  const draft = normalizeOnboardingDraft(parsed ?? undefined);
  return isMeaningfulOnboardingDraft(draft) ? draft : null;
}

export function writeOnboardingDraft(draft: OnboardingDraft): void {
  writeLocalStorageValue(ONBOARDING_DRAFT_STORAGE_KEY, draft);
}

export function clearOnboardingDraft(): void {
  removeLocalStorageValue(ONBOARDING_DRAFT_STORAGE_KEY);
}

export function serverDraftToLocalDraft(serverDraft: ServerOnboardingDraft | null | undefined): OnboardingDraft | null {
  if (!serverDraft || serverDraft.version !== 1) return null;
  return normalizeOnboardingDraft({
    version: 1,
    updatedAtMs: typeof serverDraft.updated_at_ms === "number" ? serverDraft.updated_at_ms : 0,
    step: typeof serverDraft.step === "number" ? serverDraft.step : 1,
    commandBots:
      Array.isArray(serverDraft.command_bots) && serverDraft.command_bots.length > 0
        ? serverDraft.command_bots.map((entry) => ({
            provider: entry.provider ?? "claude",
            token: entry.token ?? "",
            botInfo: normalizeBotInfo(entry.bot_info),
          }))
        : undefined,
    announceToken: serverDraft.announce_token ?? "",
    notifyToken: serverDraft.notify_token ?? "",
    announceBotInfo: normalizeBotInfo(serverDraft.announce_bot_info),
    notifyBotInfo: normalizeBotInfo(serverDraft.notify_bot_info),
    providerStatuses: serverDraft.provider_statuses ?? {},
    selectedTemplate: serverDraft.selected_template ?? null,
    agents:
      serverDraft.agents?.map((agent) => ({
        id: agent.id ?? "",
        name: agent.name ?? "",
        nameEn: agent.name_en ?? undefined,
        description: agent.description ?? "",
        descriptionEn: agent.description_en ?? undefined,
        prompt: agent.prompt ?? "",
        custom: agent.custom ?? false,
      })) ?? [],
    customName: serverDraft.custom_name ?? "",
    customDesc: serverDraft.custom_desc ?? "",
    customNameEn: serverDraft.custom_name_en ?? "",
    customDescEn: serverDraft.custom_desc_en ?? "",
    expandedAgent: serverDraft.expanded_agent ?? null,
    selectedGuild: serverDraft.selected_guild ?? "",
    channelAssignments:
      serverDraft.channel_assignments?.map((assignment) => ({
        agentId: assignment.agent_id ?? "",
        agentName: assignment.agent_name ?? "",
        recommendedName: assignment.recommended_name ?? "",
        channelId: assignment.channel_id ?? "",
        channelName: assignment.channel_name ?? "",
      })) ?? [],
    ownerId: serverDraft.owner_id ?? "",
    hasExistingSetup: Boolean(serverDraft.has_existing_setup),
    confirmRerunOverwrite: Boolean(serverDraft.confirm_rerun_overwrite),
  });
}

export function pickPreferredOnboardingDraft(
  localDraft: OnboardingDraft | null,
  serverDraft: OnboardingDraft | null,
): OnboardingDraft | null {
  const meaningfulLocal = isMeaningfulOnboardingDraft(localDraft) ? localDraft : null;
  const meaningfulServer = isMeaningfulOnboardingDraft(serverDraft) ? serverDraft : null;
  if (!meaningfulLocal) return meaningfulServer;
  if (!meaningfulServer) return meaningfulLocal;
  return meaningfulServer.updatedAtMs > meaningfulLocal.updatedAtMs
    ? meaningfulServer
    : meaningfulLocal;
}
