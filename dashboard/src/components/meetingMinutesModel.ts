import { STORAGE_KEYS } from "../lib/storageKeys";
import { readLocalStorageValue } from "../lib/useLocalStorage";
import type {
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
  RoundTableMeetingExpertOption,
} from "../types";
import type { GitHubRepoOption } from "../api/client";
import { getDisplayMeetingReferenceHashes } from "./meetingReferenceHash";

export const STORAGE_KEY = STORAGE_KEYS.meetingChannelId;
export const FIXED_PARTICIPANTS_STORAGE_KEY = STORAGE_KEYS.meetingFixedParticipants;
export const MEETING_PROVIDERS = ["claude", "codex", "gemini", "opencode", "qwen"] as const;
export const PROVIDER_LABELS: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  gemini: "Gemini",
  qwen: "Qwen",
  opencode: "OpenCode",
  copilot: "Copilot",
  antigravity: "Antigravity",
  api: "API",
};

type MeetingNotificationType = "info" | "success" | "warning" | "error";
type MeetingNotifier = (
  message: string,
  type?: MeetingNotificationType,
) => string | void;
type MeetingNotificationUpdater = (
  id: string,
  message: string,
  type?: MeetingNotificationType,
) => void;
type MeetingTranslator = (messages: { ko: string; en: string }) => string;

export function getDefaultIssueRepo(
  repos: GitHubRepoOption[],
  viewerLogin: string,
): string {
  return (
    repos.find((repo) => repo.nameWithOwner.endsWith("/CookingHeart"))
      ?.nameWithOwner ||
    repos.find(
      (repo) => viewerLogin && repo.nameWithOwner.startsWith(`${viewerLogin}/`),
    )?.nameWithOwner ||
    repos[0]?.nameWithOwner ||
    ""
  );
}

export function filterReposForViewer(
  repos: GitHubRepoOption[],
  viewerLogin: string,
): GitHubRepoOption[] {
  if (!viewerLogin) return repos;
  return repos.filter((repo) =>
    repo.nameWithOwner.startsWith(`${viewerLogin}/`),
  );
}

export function normalizeSelectionReason(reason: string | null | undefined): string {
  const trimmed = (reason ?? "").trim();
  if (!trimmed) return "";
  return trimmed.replace(/^선정 사유:\s*/u, "").trim();
}

export function readStoredMeetingChannelId(): string {
  return (
    readLocalStorageValue<string | null>(STORAGE_KEY, null, {
      validate: (value): value is string => typeof value === "string",
      legacy: (raw) => raw,
      warnOnInvalid: false,
    }) ?? ""
  );
}

export function parseStoredFixedParticipants(): string[] {
  const parsed = readLocalStorageValue<unknown>(FIXED_PARTICIPANTS_STORAGE_KEY, []);
  if (!Array.isArray(parsed)) {
    return [];
  }
  return parsed.filter(
    (roleId): roleId is string =>
      typeof roleId === "string" && roleId.trim().length > 0,
  );
}

export function getDefaultReviewerProvider(
  primaryProvider: string,
  ownerProvider?: string | null,
): string {
  return (
    MEETING_PROVIDERS.find(
      (provider) =>
        provider !== primaryProvider && provider !== (ownerProvider ?? null),
    ) ?? ""
  );
}

export function filterMeetingExpertsByQuery(
  experts: RoundTableMeetingExpertOption[],
  query: string,
): RoundTableMeetingExpertOption[] {
  const normalizedQuery = query.trim().toLowerCase();
  if (!normalizedQuery) return experts;

  return experts.filter((expert) => {
    const haystacks = [
      expert.display_name,
      expert.role_id,
      ...expert.keywords,
      expert.domain_summary ?? "",
      ...expert.strengths,
      ...expert.task_types,
      ...expert.anti_signals,
      expert.provider_hint ?? "",
    ];
    return haystacks.some((value) =>
      value.toLowerCase().includes(normalizedQuery),
    );
  });
}

export function pruneFixedParticipantRoleIdsForLoadedChannel(
  previous: string[],
  loadingChannels: boolean,
  selectedChannel: RoundTableMeetingChannelOption | null,
): string[] {
  if (loadingChannels || !selectedChannel) return previous;
  const availableExperts = selectedChannel.available_experts ?? [];
  if (availableExperts.length === 0)
    return previous.length === 0 ? previous : [];

  const availableRoleIds = new Set(
    availableExperts.map((expert) => expert.role_id),
  );
  const next = previous.filter((roleId) => availableRoleIds.has(roleId));
  if (
    next.length === previous.length &&
    next.every((roleId, index) => roleId === previous[index])
  ) {
    return previous;
  }
  return next;
}

export function getMeetingReferenceHashes(
  meeting: Pick<RoundTableMeeting, "meeting_hash" | "thread_hash">,
): string[] {
  return getDisplayMeetingReferenceHashes(meeting);
}

export async function openMeetingDetailWithFallback(
  meeting: RoundTableMeeting,
  fetchMeeting: (meetingId: string) => Promise<RoundTableMeeting>,
  logError: (message: string, error: unknown) => void = console.error,
): Promise<RoundTableMeeting> {
  try {
    return await fetchMeeting(meeting.id);
  } catch (error) {
    logError(`Meeting detail load failed for ${meeting.id}`, error);
    return meeting;
  }
}

export async function submitMeetingStartRequest(options: {
  agenda: string;
  channelId: string;
  primaryProvider: string;
  reviewerProvider: string;
  fixedParticipants: string[];
  startMeeting: (
    agenda: string,
    channelId: string,
    primaryProvider: string,
    reviewerProvider: string,
    fixedParticipants?: string[],
  ) => Promise<{ ok: boolean; message?: string }>;
  notify?: MeetingNotifier;
  updateNotification?: MeetingNotificationUpdater;
  t: MeetingTranslator;
}): Promise<{ ok: boolean; message: string }> {
  const {
    agenda,
    channelId,
    primaryProvider,
    reviewerProvider,
    fixedParticipants,
    startMeeting,
    notify,
    updateNotification,
    t,
  } = options;
  const acceptedMessage = t({
    ko: "회의 시작 요청이 접수되었습니다",
    en: "Meeting start request accepted",
  });
  const pendingNotificationId = notify?.(acceptedMessage, "info");

  try {
    const result = await startMeeting(
      agenda,
      channelId,
      primaryProvider,
      reviewerProvider,
      fixedParticipants,
    );
    const successMessage =
      result.message ||
      t({
        ko: "회의 시작 요청을 보냈습니다",
        en: "Meeting start requested",
      });

    if (typeof pendingNotificationId === "string" && updateNotification) {
      updateNotification(pendingNotificationId, successMessage, "success");
    } else if (successMessage !== acceptedMessage) {
      notify?.(successMessage, "success");
    }

    return {
      ok: result.ok,
      message: successMessage,
    };
  } catch (error) {
    const errorMessage =
      error instanceof Error
        ? error.message
        : t({ ko: "회의 시작 실패", en: "Failed to start meeting" });

    if (typeof pendingNotificationId === "string" && updateNotification) {
      updateNotification(pendingNotificationId, errorMessage, "error");
    } else {
      notify?.(errorMessage, "error");
    }

    throw error instanceof Error ? error : new Error(errorMessage);
  }
}
