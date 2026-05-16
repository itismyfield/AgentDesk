import { useState, useEffect } from "react";
import { useI18n } from "../i18n";
import {
  removeLocalStorageValue,
  writeLocalStorageValue,
} from "../lib/useLocalStorage";
import {
  getMeetingIssueResult,
  getMeetingIssueState,
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
import {
  Ban,
  CheckCircle2,
  CircleDashed,
  FileText,
  Plus,
  Trash2,
  ChevronDown,
  ChevronUp,
  Settings2,
  TriangleAlert,
} from "lucide-react";
import MeetingDetailModal from "./MeetingDetailModal";
import MeetingProviderFlow, {
  getProviderMeta,
} from "./MeetingProviderFlow";
import { formatMeetingReferenceHash } from "./meetingReferenceHash";
import MarkdownContent from "./common/MarkdownContent";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
} from "./common/SurfacePrimitives";
import {
  FIXED_PARTICIPANTS_STORAGE_KEY,
  MEETING_PROVIDERS,
  PROVIDER_LABELS,
  STORAGE_KEY,
  filterMeetingExpertsByQuery,
  filterReposForViewer,
  getDefaultIssueRepo,
  getDefaultReviewerProvider,
  normalizeSelectionReason,
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

function ownerProviderBadgeStyle(provider: string) {
  const meta = getProviderMeta(provider);
  return {
    background: meta.bg,
    color: meta.color,
    border: `1px solid ${meta.border}`,
  } as const;
}

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
  const [channelQuery, setChannelQuery] = useState("");
  const [loadingChannels, setLoadingChannels] = useState(false);
  const [channelError, setChannelError] = useState<string | null>(null);
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
        setChannelError(null);
      })
      .catch((error) => {
        if (cancelled) return;
        setMeetingChannels([]);
        setLoadingChannels(false);
        setChannelError(
          error instanceof Error
            ? error.message
            : t({
                ko: "회의 채널 목록을 불러오지 못했습니다",
                en: "Failed to load meeting channels",
              }),
        );
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
  const filteredChannels = meetingChannels.filter((channel) => {
    const query = channelQuery.trim().toLowerCase();
    if (!query) return true;
    return (
      channel.channel_name.toLowerCase().includes(query) ||
      channel.channel_id.includes(query) ||
      channel.owner_provider.toLowerCase().includes(query) ||
      `${channel.channel_name} (${channel.channel_id})`
        .toLowerCase()
        .includes(query)
    );
  });

  useEffect(() => {
    if (!selectedChannel) return;
    setChannelQuery(
      `${selectedChannel.channel_name} (${selectedChannel.channel_id})`,
    );
  }, [selectedChannel?.channel_id]);

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

  const statusBadge = (status: string) => {
    const map: Record<string, { bg: string; color: string; label: string }> = {
      completed: {
        bg: "rgba(16,185,129,0.15)",
        color: "#34d399",
        label: t({ ko: "완료", en: "Completed" }),
      },
      in_progress: {
        bg: "rgba(245,158,11,0.15)",
        color: "#fbbf24",
        label: t({ ko: "진행중", en: "In Progress" }),
      },
      cancelled: {
        bg: "rgba(239,68,68,0.15)",
        color: "#f87171",
        label: t({ ko: "취소", en: "Cancelled" }),
      },
    };
    const s = map[status] || map.completed;
    return (
      <span
        className="text-xs px-2 py-0.5 rounded-full font-medium"
        style={{ background: s.bg, color: s.color }}
      >
        {s.label}
      </span>
    );
  };

  const inputStyle = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };

  const getIssueProgress = (meeting: RoundTableMeeting) => {
    const total = meeting.proposed_issues?.length ?? 0;
    const results = meeting.issue_creation_results ?? [];
    const createdFromResults = results.filter(
      (result) => result.ok && result.discarded !== true,
    ).length;
    const created = Math.min(
      createdFromResults > 0 ? createdFromResults : meeting.issues_created || 0,
      total,
    );
    const failed = Math.min(
      results.filter((result) => !result.ok && result.discarded !== true)
        .length,
      Math.max(total - created, 0),
    );
    const discarded = Math.min(
      results.filter((result) => result.discarded === true).length,
      Math.max(total - created - failed, 0),
    );
    const pending = Math.max(total - created - failed - discarded, 0);
    return {
      total,
      created,
      failed,
      discarded,
      pending,
      allCreated: total > 0 && created === total,
      allResolved: total > 0 && pending === 0 && failed === 0,
    };
  };

  const getIssueProgressText = (
    issueProgress: ReturnType<typeof getIssueProgress>,
  ) => {
    if (issueProgress.allCreated) {
      return t({
        ko: `일감 생성 완료 ${issueProgress.created}/${issueProgress.total}`,
        en: `Issues created ${issueProgress.created}/${issueProgress.total}`,
      });
    }
    if (issueProgress.allResolved) {
      return t({
        ko: `일감 처리 완료 생성 ${issueProgress.created}/${issueProgress.total}, 폐기 ${issueProgress.discarded}건`,
        en: `Issues resolved: created ${issueProgress.created}/${issueProgress.total}, discarded ${issueProgress.discarded}`,
      });
    }
    if (issueProgress.failed > 0) {
      return t({
        ko: `생성 성공 ${issueProgress.created}/${issueProgress.total}, 실패 ${issueProgress.failed}건${issueProgress.discarded > 0 ? `, 폐기 ${issueProgress.discarded}건` : ""}`,
        en: `Created ${issueProgress.created}/${issueProgress.total}, failed ${issueProgress.failed}${issueProgress.discarded > 0 ? `, discarded ${issueProgress.discarded}` : ""}`,
      });
    }
    if (issueProgress.discarded > 0) {
      return issueProgress.pending > 0
        ? t({
            ko: `생성 대기 ${issueProgress.pending}건, 폐기 ${issueProgress.discarded}건`,
            en: `Pending ${issueProgress.pending}, discarded ${issueProgress.discarded}`,
          })
        : t({
            ko: `일감 처리 완료 생성 ${issueProgress.created}/${issueProgress.total}, 폐기 ${issueProgress.discarded}건`,
            en: `Issues resolved: created ${issueProgress.created}/${issueProgress.total}, discarded ${issueProgress.discarded}`,
          });
    }
    return t({
      ko: `생성 대기 ${issueProgress.pending}건`,
      en: `Pending ${issueProgress.pending}`,
    });
  };

  const activeMeetingCount = meetings.filter((meeting) => meeting.status === "in_progress").length;
  const completedMeetingCount = meetings.filter((meeting) => meeting.status === "completed").length;
  const unresolvedIssueCount = meetings.reduce((sum, meeting) => {
    const issueProgress = getIssueProgress(meeting);
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
      <SurfaceSection
        eyebrow={t({ ko: "Round Table", en: "Round Table" })}
        title={t({ ko: "회의 기록", en: "Meeting Records" })}
        description={
          embedded
            ? undefined
            : t({
                ko: "라운드 테이블 상세, 교차검증 흐름, 후속 일감 상태를 한 화면에서 관리합니다.",
                en: "Manage round-table detail, cross-review flow, and follow-up issue state in one place.",
              })
        }
        badge={t({ ko: `${meetings.length}개 기록`, en: `${meetings.length} records` })}
        actions={(
          <SurfaceActionButton
            tone={showStartForm ? "neutral" : "accent"}
            onClick={() => setShowStartForm((v) => !v)}
          >
            <span className="inline-flex items-center gap-1.5">
              <Plus size={14} />
              {showStartForm
                ? t({ ko: "입력 닫기", en: "Close Form" })
                : t({ ko: "새 회의", en: "New Meeting" })}
            </span>
          </SurfaceActionButton>
        )}
      >
        <div className="mt-4 flex flex-wrap gap-3">
          <SurfaceMetricPill
            label={t({ ko: "활성 회의", en: "Active Meetings" })}
            value={t({ ko: `${activeMeetingCount}건 진행 중`, en: `${activeMeetingCount} in progress` })}
            tone={activeMeetingCount > 0 ? "accent" : "neutral"}
          />
          <SurfaceMetricPill
            label={t({ ko: "완료 기록", en: "Completed" })}
            value={t({ ko: `${completedMeetingCount}건`, en: `${completedMeetingCount} records` })}
            tone="success"
          />
          <SurfaceMetricPill
            label={t({ ko: "후속 정리", en: "Follow-ups" })}
            value={t({ ko: `${unresolvedIssueCount}건 미해결`, en: `${unresolvedIssueCount} unresolved` })}
            tone={unresolvedIssueCount > 0 ? "warn" : "info"}
          />
        </div>

        {(!embedded || unresolvedIssueCount > 0) && (
          <SurfaceNotice
            className="mt-4"
            tone={unresolvedIssueCount > 0 ? "warn" : "info"}
          >
            <div className="text-sm leading-6">
              {unresolvedIssueCount > 0
                ? t({
                    ko: `생성 대기 또는 실패한 후속 일감 ${unresolvedIssueCount}건을 이 화면에서 바로 정리할 수 있습니다.`,
                    en: `You can resolve ${unresolvedIssueCount} pending or failed follow-up issues directly from this screen.`,
                  })
                : t({
                    ko: "현재 미해결 후속 일감이 없습니다. 새 라운드 테이블을 시작하거나 최근 회의 흐름을 검토하세요.",
                    en: "There are no unresolved follow-up issues. Start a new round table or review recent meeting flow.",
                  })}
            </div>
          </SurfaceNotice>
        )}
      </SurfaceSection>

      {/* Start meeting form */}
      {showStartForm && (
        <SurfaceSection
          eyebrow={t({ ko: "Compose", en: "Compose" })}
          title={t({ ko: "회의 시작", en: "Start Meeting" })}
          description={t({
            ko: "회의 채널, 안건, 진행 모델을 정하면 반대 모델 교차검증이 자동으로 따라옵니다.",
            en: "Set the channel, agenda, and primary model. Counter-model cross-review follows automatically.",
          })}
          actions={(
            <SurfaceActionButton tone="neutral" onClick={() => setShowStartForm(false)}>
              {t({ ko: "취소", en: "Cancel" })}
            </SurfaceActionButton>
          )}
        >
          <div className="mt-4 space-y-3">

            {/* Channel ID row */}
            <SurfaceCard className="rounded-2xl p-4" style={{ background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)", borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)" }}>
              <div className="flex flex-col gap-1 sm:flex-row sm:items-center sm:gap-3">
                <label className="text-xs font-semibold uppercase tracking-widest shrink-0 sm:w-24" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "채널 ID", en: "Channel ID" })}
                </label>
                {showChannelEdit || !channelId ? (
                  <input
                    type="text"
                    value={channelId}
                    onChange={(e) => setChannelId(e.target.value)}
                    placeholder={t({ ko: "Discord 채널 ID", en: "Discord Channel ID" })}
                    className="flex-1 px-3 py-1.5 rounded-lg text-xs font-mono"
                    style={inputStyle}
                    onBlur={() => { if (channelId) setShowChannelEdit(false); }}
                    autoFocus
                  />
                ) : (
                  <div className="flex items-center gap-2 flex-1">
                    <span className="text-xs font-mono" style={{ color: "var(--th-text-muted)" }}>
                      {channelId}
                    </span>
                    <SurfaceActionButton
                      onClick={() => setShowChannelEdit(true)}
                      tone="neutral"
                      compact
                      title={t({ ko: "채널 ID 변경", en: "Change Channel ID" })}
                    >
                      <Settings2 size={12} style={{ color: "var(--th-text-muted)" }} />
                    </SurfaceActionButton>
                  </div>
                )}
              </div>
            </SurfaceCard>

            {/* Agenda input */}
            <SurfaceCard className="rounded-2xl p-4" style={{ background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)", borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)" }}>
              <div className="flex flex-col gap-1 sm:flex-row sm:items-start sm:gap-3">
                <label className="text-xs font-semibold uppercase tracking-widest shrink-0 sm:w-24 sm:pt-2" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "안건", en: "Agenda" })}
                </label>
                <textarea
                  value={agenda}
                  onChange={(e) => setAgenda(e.target.value)}
                  placeholder={t({ ko: "회의 안건을 입력하세요", en: "Enter meeting agenda" })}
                  rows={3}
                  className="flex-1 resize-y px-3 py-2 rounded-lg text-sm"
                  style={inputStyle}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && !e.nativeEvent.isComposing) {
                      e.preventDefault();
                      handleStartMeeting();
                    }
                  }}
                />
              </div>
              <div className="mt-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
                {t({ ko: "시작: Ctrl/⌘ + Enter", en: "Start: Ctrl/⌘ + Enter" })}
              </div>
            </SurfaceCard>

            <SurfaceCard className="rounded-2xl p-4" style={{ background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)", borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)" }}>
              <div className="flex flex-col gap-4">
                <div className="grid gap-3 lg:grid-cols-2">
                  <div className="flex flex-col gap-1">
                    <label className="text-xs font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                      {t({ ko: "진행 프로바이더", en: "Primary Provider" })}
                    </label>
                    <select
                      value={primaryProvider}
                      onChange={(e) => setPrimaryProvider(e.target.value)}
                      className="px-3 py-2 rounded-lg text-xs"
                      style={inputStyle}
                    >
                      {MEETING_PROVIDERS.map((p) => (
                        <option key={p} value={p}>{PROVIDER_LABELS[p] ?? p.toUpperCase()}</option>
                      ))}
                    </select>
                  </div>
                  <div className="flex flex-col gap-1">
                    <label className="text-xs font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                      {t({ ko: "리뷰 프로바이더", en: "Reviewer Provider" })}
                    </label>
                    <select
                      value={reviewerProvider}
                      onChange={(e) => setReviewerProvider(e.target.value)}
                      className="px-3 py-2 rounded-lg text-xs"
                      style={inputStyle}
                    >
                      {reviewerOptions.map((provider) => (
                        <option key={provider} value={provider}>
                          {PROVIDER_LABELS[provider] ?? provider.toUpperCase()}
                        </option>
                      ))}
                    </select>
                  </div>
                </div>
                <SurfaceNotice tone="info" compact>
                  {t({ ko: "반대 모델이 자동 교차검증", en: "Counter model auto cross-review" })}
                </SurfaceNotice>
                <div className="flex flex-col gap-3 rounded-2xl border p-3" style={{ borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)", background: "color-mix(in srgb, var(--th-bg-surface) 70%, transparent)" }}>
                  <div className="flex flex-col gap-1">
                    <label className="text-xs font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                      {t({ ko: "고정 전문 에이전트", en: "Fixed Expert Agents" })}
                    </label>
                    <input
                      type="text"
                      value={expertQuery}
                      onChange={(event) => setExpertQuery(event.target.value)}
                      placeholder={t({ ko: "전문 에이전트 검색 후 여러 명 선택", en: "Search experts and select multiple" })}
                      className="px-3 py-2 rounded-lg text-xs"
                      style={inputStyle}
                    />
                  </div>
                  <div className="flex flex-wrap gap-2">
                    {filteredExperts.map((expert) => {
                      const selected = fixedParticipants.includes(expert.role_id);
                      return (
                        <button
                          key={expert.role_id}
                          type="button"
                          onClick={() => toggleFixedParticipant(expert)}
                          className="min-h-11 rounded-2xl border px-3 py-2.5 text-left text-sm transition-colors"
                          style={{
                            borderColor: selected
                              ? "color-mix(in srgb, var(--th-accent-primary) 40%, var(--th-border) 60%)"
                              : "color-mix(in srgb, var(--th-border) 72%, transparent)",
                            background: selected
                              ? "color-mix(in srgb, var(--th-accent-primary-soft) 72%, transparent)"
                              : "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                            color: "var(--th-text)",
                          }}
                        >
                          <span className="font-semibold">{expert.display_name}</span>
                          <span className="ml-1.5" style={{ color: "var(--th-text-muted)" }}>
                            #{expert.role_id}
                          </span>
                        </button>
                      );
                    })}
                    {filteredExperts.length === 0 && (
                      <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {t({ ko: "선택 가능한 전문 에이전트가 없습니다", en: "No available experts" })}
                      </span>
                    )}
                  </div>
                </div>
              </div>
            </SurfaceCard>

            {startError && (
              <SurfaceNotice tone="danger" compact>
                {startError}
              </SurfaceNotice>
            )}

            <div className="flex items-center gap-2 justify-end">
              <SurfaceActionButton tone="neutral" onClick={() => setShowStartForm(false)}>
                {t({ ko: "취소", en: "Cancel" })}
              </SurfaceActionButton>
              <SurfaceActionButton
                tone="accent"
                onClick={handleStartMeeting}
                disabled={starting || !agenda.trim() || !channelId.trim()}
              >
                {starting ? t({ ko: "시작 중...", en: "Starting..." }) : t({ ko: "회의 시작", en: "Start Meeting" })}
              </SurfaceActionButton>
            </div>
          </div>
        </SurfaceSection>
      )}

      {/* Empty state */}
      {meetings.length === 0 && !showStartForm && (
        <SurfaceSection
          eyebrow={t({ ko: "Archive", en: "Archive" })}
          title={t({ ko: "회의 타임라인", en: "Meeting Timeline" })}
          description={t({ ko: "최근 회의가 쌓이면 여기서 흐름과 후속 작업을 이어서 관리합니다.", en: "Recent meetings accumulate here for follow-up tracking." })}
        >
          <SurfaceEmptyState className="mt-4 py-16 text-center">
            <FileText size={48} className="mx-auto mb-4 opacity-30" />
            <p>{t({ ko: "회의 기록이 없습니다", en: "No meeting records" })}</p>
            <p className="text-sm mt-1">{t({ ko: "\"새 회의\" 버튼으로 라운드 테이블을 시작하세요", en: "Start a round table with the \"New Meeting\" button" })}</p>
          </SurfaceEmptyState>
        </SurfaceSection>
      )}

      {/* Meeting list */}
      {meetings.length > 0 && (
        <SurfaceSection
          eyebrow={t({ ko: "Archive", en: "Archive" })}
          title={t({ ko: "회의 타임라인", en: "Meeting Timeline" })}
          description={t({
            ko: "각 회의의 진행 상태, 참여자, 후속 일감 생성 흐름을 한 번에 확인합니다.",
            en: "Review meeting status, participants, and follow-up issue generation flow at a glance.",
          })}
          badge={t({ ko: `${meetings.length}개 회의`, en: `${meetings.length} meetings` })}
        >
          <div className="mt-4 space-y-4">
        {meetings.map((m) => {
          const hasProposedIssues =
            m.proposed_issues && m.proposed_issues.length > 0;
          const issuesExpanded = expandedIssues.has(m.id);
          const issueProgress = getIssueProgress(m);
          const selectedRepo = getSelectedRepo(m);
          const repoOptions = getRepoOptions(selectedRepo);
          const isSavingRepo = !!savingRepoIds[m.id];
          const canRetryIssues = hasProposedIssues && !issueProgress.allResolved && !!selectedRepo && !isSavingRepo;
          const meetingHashDisplay = formatMeetingReferenceHash(m.meeting_hash);
          const threadHashDisplay = formatMeetingReferenceHash(m.thread_hash);
          const selectionReason = normalizeSelectionReason(m.selection_reason);
          const progressTone = issueProgress.allCreated
            ? "success"
            : issueProgress.failed > 0
              ? "warn"
              : issueProgress.discarded > 0
                ? "neutral"
                : "info";
          const createButtonTone = issueProgress.allCreated || issueProgress.allResolved
            ? "neutral"
            : issueProgress.failed > 0
              ? "warn"
              : "accent";

          return (
            <SurfaceCard
              key={m.id}
              className="space-y-4 rounded-3xl p-4 sm:p-5"
              style={{
                background: "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
                borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
              }}
            >
              {/* Top row */}
              <div className="flex min-w-0 items-start justify-between gap-3">
                <div className="min-w-0 flex-1">
                  <h3
                    className="break-words text-base font-semibold [overflow-wrap:anywhere]"
                    style={{ color: "var(--th-text)" }}
                  >
                    {m.agenda}
                  </h3>
                  <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-2">
                    {statusBadge(m.status)}
                    {(m.primary_provider || m.reviewer_provider) && (
                      <MeetingProviderFlow
                        primaryProvider={m.primary_provider}
                        reviewerProvider={m.reviewer_provider}
                        compact
                      />
                    )}
                    <span
                      className="text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {new Date(m.started_at).toLocaleDateString(locale)}
                    </span>
                    {m.total_rounds > 0 && (
                      <span
                        className="text-xs"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {m.total_rounds}R
                      </span>
                    )}
                  </div>
                </div>
                <button
                  onClick={() => handleDelete(m.id)}
                  disabled={deleting === m.id}
                  className="p-1.5 rounded-lg transition-colors hover:bg-red-500/10 shrink-0"
                  title={t({ ko: "삭제", en: "Delete" })}
                >
                  <Trash2
                    size={14}
                    style={{
                      color:
                        deleting === m.id ? "var(--th-text-muted)" : "#f87171",
                    }}
                  />
                </button>
              </div>

              {(meetingHashDisplay || threadHashDisplay) && (
                <div
                  className="space-y-1 rounded-xl px-3 py-2 text-xs"
                  style={{
                    background: "rgba(148,163,184,0.08)",
                    border: "1px solid rgba(148,163,184,0.14)",
                  }}
                >
                  {meetingHashDisplay && (
                    <div className="flex min-w-0 items-center gap-2">
                      <span
                        className="shrink-0 font-medium"
                        style={{ color: "var(--th-text-secondary)" }}
                      >
                        {t({ ko: "회의 해시 :", en: "Meeting Hash:" })}
                      </span>
                      <span
                        className="min-w-0 break-all font-mono"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {meetingHashDisplay}
                      </span>
                    </div>
                  )}
                  {threadHashDisplay && (
                    <div className="flex min-w-0 items-center gap-2">
                      <span
                        className="shrink-0 font-medium"
                        style={{ color: "var(--th-text-secondary)" }}
                      >
                        {t({ ko: "스레드 해시 :", en: "Thread Hash:" })}
                      </span>
                      <span
                        className="min-w-0 break-all font-mono"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {threadHashDisplay}
                      </span>
                    </div>
                  )}
                </div>
              )}

              {/* Participants */}
              <div className="flex min-w-0 flex-wrap items-center gap-1.5">
                {m.participant_names.map((name) => (
                  <span
                    key={name}
                    className="text-xs px-2 py-0.5 rounded-full font-medium"
                    style={{
                      background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                      color: "var(--th-text-primary)",
                    }}
                  >
                    {name}
                  </span>
                ))}
              </div>

              {selectionReason && (
                <div
                  className="min-w-0 rounded-xl px-3 py-2 text-xs"
                  style={{
                    background: "rgba(148,163,184,0.08)",
                    border: "1px solid rgba(148,163,184,0.14)",
                  }}
                >
                  <span
                    className="font-medium"
                    style={{ color: "var(--th-text-secondary)" }}
                  >
                    {t({ ko: "선정 사유:", en: "Selection Reason:" })}
                  </span>{" "}
                  <span
                    className="break-words [overflow-wrap:anywhere]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {selectionReason}
                  </span>
                </div>
              )}

              {/* PMD Summary bubble */}
              {m.summary && (
                <SurfaceNotice
                  tone="accent"
                  className="items-start"
                  leading={(
                    <div className="mt-0.5 h-8 w-8 rounded-xl overflow-hidden shrink-0" style={{ background: "var(--th-bg-surface)" }}>
                      <img
                        src="/sprites/7-D-1.png"
                        alt="PMD"
                        className="w-full h-full object-cover"
                        style={{ imageRendering: "pixelated" }}
                      />
                    </div>
                  )}
                >
                  <div className="min-w-0">
                    <div className="flex items-center justify-between gap-2 mb-1 flex-wrap">
                      <div className="text-xs font-semibold" style={{ color: "var(--th-text-primary)" }}>{t({ ko: "PMD 요약", en: "PMD Summary" })}</div>
                    </div>
                    <div className="text-sm" style={{ color: "var(--th-text)" }}>
                      <MarkdownContent content={m.summary} />
                    </div>
                  </div>
                </SurfaceNotice>
              )}

              {/* Proposed issues preview */}
              {hasProposedIssues && !issueProgress.allCreated && (
                <SurfaceCard
                  className="space-y-2 rounded-2xl p-3"
                  style={{
                    background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
                    borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                  }}
                >
                  <button
                    onClick={() => toggleIssuePreview(m.id)}
                    className="flex min-w-0 items-center gap-1.5 break-words text-left text-xs font-medium transition-colors hover:opacity-80 [overflow-wrap:anywhere]"
                    style={{ color: "#34d399" }}
                  >
                    {issuesExpanded ? (
                      <ChevronUp size={14} />
                    ) : (
                      <ChevronDown size={14} />
                    )}
                    {t({
                      ko: `생성될 일감 미리보기 (${m.proposed_issues!.length}건)`,
                      en: `Preview issues to create (${m.proposed_issues!.length})`,
                    })}
                  </button>
                  {issuesExpanded && (
                    <div className="space-y-2">
                      {m.proposed_issues!.map((issue, i) => {
                        const issueResult = getMeetingIssueResult(m, issue);
                        const issueState = getMeetingIssueState(issueResult);
                        const issueKey = getProposedIssueKey(issue);
                        const actionKey = `${m.id}:${issueKey}`;
                        const isDiscardingIssue =
                          !!discardingIssueIds[actionKey];
                        const statusMeta =
                          issueState === "created"
                            ? {
                                label: t({ ko: "생성됨", en: "Created" }),
                                icon: CheckCircle2,
                                color: "#34d399",
                                bg: "rgba(16,185,129,0.12)",
                                border: "rgba(16,185,129,0.18)",
                              }
                            : issueState === "discarded"
                              ? {
                                  label: t({ ko: "폐기됨", en: "Discarded" }),
                                  icon: Ban,
                                  color: "#94a3b8",
                                  bg: "rgba(148,163,184,0.12)",
                                  border: "rgba(148,163,184,0.18)",
                                }
                              : issueState === "failed"
                                ? {
                                    label: t({ ko: "실패", en: "Failed" }),
                                    icon: TriangleAlert,
                                    color: "#fbbf24",
                                    bg: "rgba(245,158,11,0.12)",
                                    border: "rgba(245,158,11,0.18)",
                                  }
                                : {
                                    label: t({ ko: "대기", en: "Pending" }),
                                    icon: CircleDashed,
                                    color: "#60a5fa",
                                    bg: "rgba(96,165,250,0.12)",
                                    border: "rgba(96,165,250,0.18)",
                                  };
                        const StatusIcon = statusMeta.icon;
                        const issueTone = issueState === "created"
                          ? "success"
                          : issueState === "discarded"
                            ? "neutral"
                            : issueState === "failed"
                              ? "warn"
                              : "info";

                        return (
                          <SurfaceNotice
                            key={i}
                            tone={issueTone}
                            compact
                            className="items-start"
                            action={(
                              (issueState === "pending" || issueState === "failed") && (
                                <SurfaceActionButton
                                  tone="neutral"
                                  compact
                                  onClick={() => void handleDiscardIssue(m.id, issue)}
                                  disabled={isDiscardingIssue}
                                >
                                  <span className="inline-flex items-center gap-1">
                                    <Trash2 size={11} />
                                    {isDiscardingIssue ? t({ ko: "폐기 중...", en: "Discarding..." }) : t({ ko: "폐기", en: "Discard" })}
                                  </span>
                                </SurfaceActionButton>
                              )
                            )}
                          >
                            <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
                              <div className="min-w-0 flex-1">
                                <div
                                  className="break-words font-medium [overflow-wrap:anywhere]"
                                  style={{ color: "var(--th-text)" }}
                                >
                                  [RT] {issue.title}
                                </div>
                                <div
                                  className="mt-0.5 break-words [overflow-wrap:anywhere]"
                                  style={{ color: "var(--th-text-muted)" }}
                                >
                                  {t({
                                    ko: `담당: ${issue.assignee}`,
                                    en: `Assignee: ${issue.assignee}`,
                                  })}
                                </div>
                                {issueResult?.error &&
                                  issueState === "failed" && (
                                    <div
                                      className="mt-1 break-words [overflow-wrap:anywhere]"
                                      style={{ color: "#fbbf24" }}
                                    >
                                      {t({
                                        ko: `실패: ${issueResult.error}`,
                                        en: `Failed: ${issueResult.error}`,
                                      })}
                                    </div>
                                  )}
                                {issueResult?.issue_url &&
                                  issueState === "created" && (
                                    <a
                                      href={issueResult.issue_url}
                                      target="_blank"
                                      rel="noreferrer"
                                      className="mt-1 inline-flex max-w-full break-all hover:underline"
                                      style={{ color: "#34d399" }}
                                    >
                                      {t({
                                        ko: "생성된 이슈 열기",
                                        en: "Open created issue",
                                      })}
                                    </a>
                                  )}
                              </div>
                            </div>
                            <div className="mt-2">
                              <span
                                className="inline-flex items-center gap-1 rounded-full border px-2 py-0.5 text-xs font-semibold"
                                style={{
                                  background: statusMeta.bg,
                                  borderColor: statusMeta.border,
                                  color: statusMeta.color,
                                }}
                                aria-label={t({
                                  ko: `후속 일감 상태: ${statusMeta.label}`,
                                  en: `Follow-up issue status: ${statusMeta.label}`,
                                })}
                              >
                                <StatusIcon size={12} aria-hidden="true" />
                                {statusMeta.label}
                              </span>
                            </div>
                          </SurfaceNotice>
                        );
                      })}
                    </div>
                  )}
                </SurfaceCard>
              )}

              {hasProposedIssues && (
                <SurfaceNotice tone={progressTone} compact>
                  {getIssueProgressText(issueProgress)}
                </SurfaceNotice>
              )}

              {/* Actions */}
              <SurfaceCard
                className="rounded-2xl p-3"
                style={{
                  background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                }}
              >
                <div className="flex flex-col gap-3 sm:flex-row sm:flex-wrap sm:items-end sm:justify-between min-w-0">
                  <div className="flex items-center gap-2 flex-wrap min-w-0">
                    <SurfaceActionButton tone="neutral" onClick={() => void handleOpenDetail(m)}>
                      {t({ ko: "상세 보기", en: "Details" })}
                    </SurfaceActionButton>
                    {hasProposedIssues ? (
                      <>
                        <SurfaceActionButton
                          tone={createButtonTone}
                          onClick={() => handleCreateIssues(m.id, selectedRepo)}
                          disabled={!canRetryIssues || creatingIssue === m.id}
                        >
                          {issueProgress.allCreated
                            ? t({ ko: `일감 생성 완료 (${issueProgress.created}/${issueProgress.total})`, en: `Issues created (${issueProgress.created}/${issueProgress.total})` })
                            : issueProgress.allResolved
                              ? t({ ko: `일감 처리 완료 (생성 ${issueProgress.created}, 폐기 ${issueProgress.discarded})`, en: `Issues resolved (created ${issueProgress.created}, discarded ${issueProgress.discarded})` })
                              : creatingIssue === m.id
                                ? t({ ko: "생성 중...", en: "Creating..." })
                                : isSavingRepo
                                  ? t({ ko: "Repo 저장 중...", en: "Saving repo..." })
                                  : !selectedRepo
                                    ? t({ ko: "Repo 선택 필요", en: "Select repo" })
                                    : issueProgress.failed > 0
                                      ? t({ ko: `실패분 재시도 (${issueProgress.created}/${issueProgress.total})`, en: `Retry failed (${issueProgress.created}/${issueProgress.total})` })
                                      : t({ ko: `일감 생성 (${issueProgress.total}건)`, en: `Create issues (${issueProgress.total})` })}
                        </SurfaceActionButton>
                        {issueProgress.pending + issueProgress.failed > 0 && (
                          <SurfaceActionButton
                            tone="neutral"
                            onClick={() => void handleDiscardAllIssues(m.id)}
                            disabled={!!discardingMeetingIds[m.id]}
                          >
                            {!!discardingMeetingIds[m.id]
                              ? t({ ko: "전체 폐기 중...", en: "Discarding all..." })
                              : t({ ko: `남은 일감 전체 폐기 (${issueProgress.pending + issueProgress.failed}건)`, en: `Discard all remaining (${issueProgress.pending + issueProgress.failed})` })}
                          </SurfaceActionButton>
                        )}
                      </>
                    ) : (
                      m.issues_created ? (
                        <SurfaceNotice compact tone="success">
                          {t({ ko: "일감 생성 완료", en: "Issues created" })}
                        </SurfaceNotice>
                      ) : (
                        <SurfaceNotice compact tone="neutral">
                          {t({ ko: "추출된 일감 없음", en: "No issues extracted" })}
                        </SurfaceNotice>
                      )
                    )}
                  </div>
                  {hasProposedIssues && (
                    <div className="flex flex-col gap-1 min-w-0 sm:min-w-[280px]">
                      <div className="text-xs font-semibold uppercase tracking-widest text-left sm:text-right" style={{ color: "var(--th-text-muted)" }}>
                        {t({ ko: "이 회의용 Repo", en: "Repo for this meeting" })}
                      </div>
                      <select
                        value={selectedRepo}
                        onChange={(e) => void handleRepoChange(m.id, e.target.value)}
                        className="px-3 py-2 rounded-lg text-sm"
                        style={inputStyle}
                        disabled={loadingRepos || isSavingRepo || repoOptions.length === 0}
                      >
                        {!selectedRepo && <option value="">{t({ ko: "Repo 선택", en: "Select repo" })}</option>}
                        {repoOptions.map((repo) => (
                          <option key={repo.nameWithOwner} value={repo.nameWithOwner}>
                            {githubRepos.some((item) => item.nameWithOwner === repo.nameWithOwner)
                              ? repo.nameWithOwner
                              : `${repo.nameWithOwner} ${t({ ko: "(현재 목록에 없음)", en: "(not in current list)" })}`}
                          </option>
                        ))}
                      </select>
                      <div className="text-xs text-left sm:text-right" style={{ color: repoSaveErrors[m.id] ? "#fbbf24" : "var(--th-text-muted)" }}>
                        {repoSaveErrors[m.id]
                          || (isSavingRepo ? t({ ko: "repo 저장 중...", en: "Saving repo..." }) : null)
                          || repoError
                          || (loadingRepos ? t({ ko: "repo 목록 불러오는 중...", en: "Loading repos..." }) : null)
                          || (repoOwner ? t({ ko: `gh 계정 ${repoOwner}`, en: `gh account ${repoOwner}` }) : "")}
                      </div>
                    </div>
                  )}
                </div>
              </SurfaceCard>
            </SurfaceCard>
          );
        })}
          </div>
        </SurfaceSection>
      )}

      {detailMeeting && (
        <MeetingDetailModal
          meeting={detailMeeting}
          onClose={() => setDetailMeeting(null)}
        />
      )}
    </div>
  );
}
