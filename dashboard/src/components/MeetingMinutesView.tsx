import { useState, useEffect } from "react";
import { useI18n } from "../i18n";
import {
  removeLocalStorageValue,
  writeLocalStorageValue,
} from "../lib/useLocalStorage";
import {
  getProposedIssueKey,
} from "../lib/meetingHelpers";
import type {
  ProposedIssue,
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
  RoundTableMeetingExpertOption,
} from "../types";
import {
  createRoundTableIssues,
  discardAllRoundTableIssues,
  discardRoundTableIssue,
  deleteRoundTableMeeting,
  getGitHubRepos,
  getRoundTableMeetingChannels,
  getRoundTableMeeting,
  startRoundTableMeeting,
  updateRoundTableMeetingIssueRepo,
  type GitHubRepoOption,
} from "../api/client";
import MeetingDetailModal from "./MeetingDetailModal";
import MeetingOverviewSection from "./MeetingOverviewSection";
import MeetingStartForm from "./MeetingStartForm";
import MeetingTimelineSection from "./MeetingTimelineSection";
import {
  getMeetingIssueProgress,
  getMeetingIssueProgressText,
} from "./meetingIssueProgress";
import {
  FIXED_PARTICIPANTS_STORAGE_KEY,
  MEETING_PROVIDERS,
  STORAGE_KEY,
  filterMeetingExpertsByQuery,
  filterReposForViewer,
  getDefaultIssueRepo,
  getDefaultReviewerProvider,
  openMeetingDetailWithFallback,
  parseStoredFixedParticipants,
  pruneFixedParticipantRoleIdsForLoadedChannel,
  readStoredMeetingChannelId,
  submitMeetingStartRequest,
} from "./meetingMinutesModel";

export {
  filterMeetingExpertsByQuery,
  getMeetingReferenceHashes,
  openMeetingDetailWithFallback,
  pruneFixedParticipantRoleIdsForLoadedChannel,
  submitMeetingStartRequest,
} from "./meetingMinutesModel";

interface Props {
  meetings: RoundTableMeeting[];
  onRefresh: () => void;
  embedded?: boolean;
  onNotify?: (
    message: string,
    type?: "info" | "success" | "warning" | "error",
  ) => string | void;
  onUpdateNotification?: (
    id: string,
    message: string,
    type?: "info" | "success" | "warning" | "error",
  ) => void;
  initialShowStartForm?: boolean;
  initialMeetingChannels?: RoundTableMeetingChannelOption[];
  initialChannelId?: string;
}

export default function MeetingMinutesView({
  meetings,
  onRefresh,
  embedded = false,
  onNotify,
  onUpdateNotification,
  initialShowStartForm = false,
  initialMeetingChannels = [],
  initialChannelId,
}: Props) {
  const { t, locale } = useI18n();
  const [detailMeeting, setDetailMeeting] = useState<RoundTableMeeting | null>(
    null,
  );
  const [creatingIssue, setCreatingIssue] = useState<string | null>(null);
  const [discardingIssueIds, setDiscardingIssueIds] = useState<
    Record<string, boolean>
  >({});
  const [discardingMeetingIds, setDiscardingMeetingIds] = useState<
    Record<string, boolean>
  >({});
  const [deleting, setDeleting] = useState<string | null>(null);
  const [expandedIssues, setExpandedIssues] = useState<Set<string>>(new Set());
  const [showStartForm, setShowStartForm] = useState(initialShowStartForm);
  const [agenda, setAgenda] = useState("");
  const [channelId, setChannelId] = useState(
    () => initialChannelId ?? readStoredMeetingChannelId(),
  );
  const [showChannelEdit, setShowChannelEdit] = useState(false);
  const [primaryProvider, setPrimaryProvider] = useState<string>("claude");
  const [reviewerProvider, setReviewerProvider] = useState<string>(() => {
    const storedChannelId = initialChannelId ?? readStoredMeetingChannelId();
    const seededChannel =
      initialMeetingChannels.find(
        (channel) => channel.channel_id === storedChannelId,
      ) ?? null;
    return getDefaultReviewerProvider("claude", seededChannel?.owner_provider);
  });
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const [meetingChannels, setMeetingChannels] = useState<
    RoundTableMeetingChannelOption[]
  >(initialMeetingChannels);
  const [fixedParticipants, setFixedParticipants] = useState<string[]>(
    parseStoredFixedParticipants,
  );
  const [expertQuery, setExpertQuery] = useState("");
  const [loadingChannels, setLoadingChannels] = useState(false);
  const [githubRepos, setGithubRepos] = useState<GitHubRepoOption[]>([]);
  const [repoOwner, setRepoOwner] = useState<string>("");
  const [meetingRepoSelections, setMeetingRepoSelections] = useState<
    Record<string, string>
  >({});
  const [savingRepoIds, setSavingRepoIds] = useState<Record<string, boolean>>(
    {},
  );
  const [repoSaveErrors, setRepoSaveErrors] = useState<Record<string, string>>(
    {},
  );
  const [loadingRepos, setLoadingRepos] = useState(true);
  const [repoError, setRepoError] = useState<string | null>(null);

  useEffect(() => {
    if (channelId) {
      writeLocalStorageValue(STORAGE_KEY, channelId);
    }
  }, [channelId]);

  useEffect(() => {
    let cancelled = false;

    setLoadingChannels(true);
    getRoundTableMeetingChannels()
      .then((channels) => {
        if (cancelled) return;
        setMeetingChannels(channels);
        setLoadingChannels(false);
      })
      .catch((error) => {
        if (cancelled) return;
        setMeetingChannels([]);
        setLoadingChannels(false);
        console.error("Meeting channel list load failed:", error);
      });

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;

    getGitHubRepos()
      .then((result) => {
        if (cancelled) return;

        setGithubRepos(filterReposForViewer(result.repos, result.viewer_login));
        setRepoOwner(result.viewer_login);
        setLoadingRepos(false);
        setRepoError(null);
      })
      .catch((error) => {
        if (cancelled) return;
        setGithubRepos([]);
        setRepoOwner("");
        setLoadingRepos(false);
        setRepoError(
          error instanceof Error
            ? error.message
            : t({
                ko: "repo 목록을 불러오지 못했습니다",
                en: "Failed to load repo list",
              }),
        );
      });

    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    setMeetingRepoSelections((prev) => {
      const meetingIds = new Set(meetings.map((meeting) => meeting.id));
      let changed = false;
      const next: Record<string, string> = {};
      for (const [meetingId, repo] of Object.entries(prev)) {
        if (meetingIds.has(meetingId)) {
          next[meetingId] = repo;
        } else {
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [meetings]);

  const selectedChannel =
    meetingChannels.find((channel) => channel.channel_id === channelId) ?? null;
  const availableExperts = selectedChannel?.available_experts ?? [];
  const reviewerOptions = MEETING_PROVIDERS.filter(
    (provider) =>
      provider !== primaryProvider &&
      provider !== selectedChannel?.owner_provider,
  );
  const filteredExperts = filterMeetingExpertsByQuery(
    availableExperts,
    expertQuery,
  );

  useEffect(() => {
    setExpertQuery("");
  }, [selectedChannel?.channel_id]);

  useEffect(() => {
    setFixedParticipants((previous) =>
      pruneFixedParticipantRoleIdsForLoadedChannel(
        previous,
        loadingChannels,
        selectedChannel,
      ),
    );
  }, [loadingChannels, selectedChannel]);

  useEffect(() => {
    if (fixedParticipants.length === 0) {
      removeLocalStorageValue(FIXED_PARTICIPANTS_STORAGE_KEY);
      return;
    }
    writeLocalStorageValue(FIXED_PARTICIPANTS_STORAGE_KEY, fixedParticipants);
  }, [fixedParticipants]);

  useEffect(() => {
    if (reviewerOptions.length === 0) {
      if (reviewerProvider) setReviewerProvider("");
      return;
    }
    if (
      !reviewerOptions.includes(
        reviewerProvider as (typeof MEETING_PROVIDERS)[number],
      )
    ) {
      setReviewerProvider(reviewerOptions[0]);
    }
  }, [
    primaryProvider,
    reviewerProvider,
    reviewerOptions.join(","),
    selectedChannel?.owner_provider,
  ]);

  const handleOpenDetail = async (m: RoundTableMeeting) => {
    const full = await openMeetingDetailWithFallback(
      m,
      getRoundTableMeeting,
      (message, error) => console.error(message, error),
    );
    setDetailMeeting(full);
  };

  const getSelectedRepo = (meeting: RoundTableMeeting) => {
    if (
      Object.prototype.hasOwnProperty.call(meetingRepoSelections, meeting.id)
    ) {
      return meetingRepoSelections[meeting.id] ?? "";
    }
    return (
      (typeof meeting.issue_repo === "string" && meeting.issue_repo.trim()) ||
      getDefaultIssueRepo(githubRepos, repoOwner)
    );
  };

  const getRepoOptions = (selectedRepo: string) => {
    if (
      !selectedRepo ||
      githubRepos.some((repo) => repo.nameWithOwner === selectedRepo)
    ) {
      return githubRepos;
    }
    return [
      {
        nameWithOwner: selectedRepo,
        updatedAt: "",
        isPrivate: false,
      },
      ...githubRepos,
    ];
  };

  const handleCreateIssues = async (id: string, repo: string) => {
    if (!repo) return;
    setCreatingIssue(id);
    try {
      await createRoundTableIssues(id, repo);
      onRefresh();
    } catch (e) {
      console.error("Issue creation failed:", e);
    } finally {
      setCreatingIssue(null);
    }
  };

  const handleRepoChange = async (meetingId: string, repo: string) => {
    const hadPreviousSelection = Object.prototype.hasOwnProperty.call(
      meetingRepoSelections,
      meetingId,
    );
    const previousSelection = meetingRepoSelections[meetingId];

    setMeetingRepoSelections((prev) => ({
      ...prev,
      [meetingId]: repo,
    }));
    setRepoSaveErrors((prev) => {
      const next = { ...prev };
      delete next[meetingId];
      return next;
    });
    setSavingRepoIds((prev) => ({ ...prev, [meetingId]: true }));

    try {
      await updateRoundTableMeetingIssueRepo(meetingId, repo || null);
    } catch (e) {
      setMeetingRepoSelections((prev) => {
        const next = { ...prev };
        if (hadPreviousSelection) next[meetingId] = previousSelection;
        else delete next[meetingId];
        return next;
      });
      setRepoSaveErrors((prev) => ({
        ...prev,
        [meetingId]:
          e instanceof Error
            ? e.message
            : t({ ko: "repo 저장 실패", en: "Failed to save repo" }),
      }));
      console.error("Repo setting save failed:", e);
    } finally {
      setSavingRepoIds((prev) => {
        const next = { ...prev };
        delete next[meetingId];
        return next;
      });
    }
  };

  const handleDiscardIssue = async (
    meetingId: string,
    issue: ProposedIssue,
  ) => {
    const issueKey = getProposedIssueKey(issue);
    const actionKey = `${meetingId}:${issueKey}`;

    if (
      !window.confirm(
        t({
          ko: "이 일감은 생성하지 않기로 처리하시겠습니까?",
          en: "Discard this issue and skip creation?",
        }),
      )
    )
      return;

    setDiscardingIssueIds((prev) => ({ ...prev, [actionKey]: true }));
    try {
      await discardRoundTableIssue(meetingId, issueKey);
      onRefresh();
    } catch (e) {
      console.error("Issue discard failed:", e);
    } finally {
      setDiscardingIssueIds((prev) => {
        const next = { ...prev };
        delete next[actionKey];
        return next;
      });
    }
  };

  const handleDiscardAllIssues = async (meetingId: string) => {
    if (
      !window.confirm(
        t({
          ko: "이 회의록의 생성되지 않은 일감을 전부 폐기하시겠습니까?",
          en: "Discard all uncreated issues from this meeting?",
        }),
      )
    )
      return;

    setDiscardingMeetingIds((prev) => ({ ...prev, [meetingId]: true }));
    try {
      await discardAllRoundTableIssues(meetingId);
      onRefresh();
    } catch (e) {
      console.error("Discard all issues failed:", e);
    } finally {
      setDiscardingMeetingIds((prev) => {
        const next = { ...prev };
        delete next[meetingId];
        return next;
      });
    }
  };

  const handleDelete = async (id: string) => {
    if (
      !window.confirm(
        t({
          ko: "이 회의록을 삭제하시겠습니까?",
          en: "Delete this meeting record?",
        }),
      )
    )
      return;
    setDeleting(id);
    try {
      await deleteRoundTableMeeting(id);
      onRefresh();
    } catch (e) {
      console.error("Delete failed:", e);
    } finally {
      setDeleting(null);
    }
  };

  const toggleIssuePreview = (id: string) => {
    setExpandedIssues((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const handleStartMeeting = async () => {
    if (!agenda.trim() || !channelId.trim() || !reviewerProvider.trim()) return;
    setStarting(true);
    setStartError(null);
    try {
      await submitMeetingStartRequest({
        agenda: agenda.trim(),
        channelId: channelId.trim(),
        primaryProvider,
        reviewerProvider,
        fixedParticipants,
        startMeeting: startRoundTableMeeting,
        notify: onNotify,
        updateNotification: onUpdateNotification,
        t,
      });
      setAgenda("");
      setShowStartForm(false);
      onRefresh();
    } catch (e) {
      const message =
        e instanceof Error
          ? e.message
          : t({ ko: "회의 시작 실패", en: "Failed to start meeting" });
      setStartError(message);
      onNotify?.(message, "error");
    } finally {
      setStarting(false);
    }
  };

  const toggleFixedParticipant = (expert: RoundTableMeetingExpertOption) => {
    setFixedParticipants((previous) => {
      if (previous.includes(expert.role_id)) {
        return previous.filter((roleId) => roleId !== expert.role_id);
      }
      return [...previous, expert.role_id];
    });
  };

  const inputStyle = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };

  const activeMeetingCount = meetings.filter((meeting) => meeting.status === "in_progress").length;
  const completedMeetingCount = meetings.filter((meeting) => meeting.status === "completed").length;
  const unresolvedIssueCount = meetings.reduce((sum, meeting) => {
    const issueProgress = getMeetingIssueProgress(meeting);
    return sum + issueProgress.pending + issueProgress.failed;
  }, 0);

  return (
    <div
      className={
        embedded
          ? "w-full min-w-0 space-y-5 overflow-x-hidden"
          : "mx-auto w-full max-w-4xl min-w-0 space-y-6 overflow-x-hidden p-4 pb-40 sm:h-full sm:overflow-y-auto sm:p-6"
      }
      style={embedded ? undefined : { paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <MeetingOverviewSection
        t={t}
        embedded={embedded}
        meetingsCount={meetings.length}
        showStartForm={showStartForm}
        activeMeetingCount={activeMeetingCount}
        completedMeetingCount={completedMeetingCount}
        unresolvedIssueCount={unresolvedIssueCount}
        onToggleStartForm={() => setShowStartForm((value) => !value)}
      />

      {showStartForm && (
        <MeetingStartForm
          t={t}
          agenda={agenda}
          channelId={channelId}
          showChannelEdit={showChannelEdit}
          primaryProvider={primaryProvider}
          reviewerProvider={reviewerProvider}
          reviewerOptions={reviewerOptions}
          expertQuery={expertQuery}
          filteredExperts={filteredExperts}
          fixedParticipants={fixedParticipants}
          startError={startError}
          starting={starting}
          inputStyle={inputStyle}
          onAgendaChange={setAgenda}
          onChannelIdChange={setChannelId}
          onShowChannelEditChange={setShowChannelEdit}
          onPrimaryProviderChange={setPrimaryProvider}
          onReviewerProviderChange={setReviewerProvider}
          onExpertQueryChange={setExpertQuery}
          onToggleFixedParticipant={toggleFixedParticipant}
          onStartMeeting={handleStartMeeting}
          onCancel={() => setShowStartForm(false)}
        />
      )}

      <MeetingTimelineSection
        meetings={meetings}
        showStartForm={showStartForm}
        locale={locale}
        t={t}
        expandedIssues={expandedIssues}
        deleting={deleting}
        creatingIssue={creatingIssue}
        discardingIssueIds={discardingIssueIds}
        discardingMeetingIds={discardingMeetingIds}
        savingRepoIds={savingRepoIds}
        repoSaveErrors={repoSaveErrors}
        loadingRepos={loadingRepos}
        repoError={repoError}
        repoOwner={repoOwner}
        githubRepos={githubRepos}
        inputStyle={inputStyle}
        getIssueProgress={getMeetingIssueProgress}
        getIssueProgressText={(issueProgress) =>
          getMeetingIssueProgressText(issueProgress, t)
        }
        getSelectedRepo={getSelectedRepo}
        getRepoOptions={getRepoOptions}
        onToggleIssuePreview={toggleIssuePreview}
        onDelete={handleDelete}
        onOpenDetail={(meeting) => void handleOpenDetail(meeting)}
        onCreateIssues={handleCreateIssues}
        onDiscardIssue={(meetingId, issue) =>
          void handleDiscardIssue(meetingId, issue)
        }
        onDiscardAllIssues={(meetingId) => void handleDiscardAllIssues(meetingId)}
        onRepoChange={(meetingId, repo) => void handleRepoChange(meetingId, repo)}
      />

      {detailMeeting && (
        <MeetingDetailModal
          meeting={detailMeeting}
          onClose={() => setDetailMeeting(null)}
        />
      )}
    </div>
  );
}
