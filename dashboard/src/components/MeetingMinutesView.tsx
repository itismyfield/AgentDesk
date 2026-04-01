import { useState, useEffect } from "react";
import { useI18n } from "../i18n";
import type { IssueCreationResult, ProposedIssue, RoundTableMeeting } from "../types";
import {
  createRoundTableIssues,
  discardAllRoundTableIssues,
  discardRoundTableIssue,
  deleteRoundTableMeeting,
  getGitHubRepos,
  getRoundTableMeeting,
  startRoundTableMeeting,
  updateRoundTableMeetingIssueRepo,
  type GitHubRepoOption,
} from "../api/client";
import { FileText, Plus, Settings2, Trash2, ChevronDown, ChevronUp } from "lucide-react";
import MeetingDetailModal from "./MeetingDetailModal";
import MeetingProviderFlow, { formatProviderFlow, providerFlowCaption } from "./MeetingProviderFlow";
import MarkdownContent from "./common/MarkdownContent";

const STORAGE_KEY = "pcd_meeting_channel_id";
const MEETING_PROVIDERS = ["claude", "codex", "gemini", "qwen"] as const;
const PROVIDER_LABELS: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  gemini: "Gemini",
  qwen: "Qwen",
  opencode: "OpenCode",
  copilot: "Copilot",
  antigravity: "Antigravity",
  api: "API",
};

interface Props {
  meetings: RoundTableMeeting[];
  onRefresh: () => void;
}

function getDefaultIssueRepo(repos: GitHubRepoOption[], viewerLogin: string): string {
  return (
    repos.find((repo) => repo.nameWithOwner.endsWith("/CookingHeart"))?.nameWithOwner
    || repos.find((repo) => viewerLogin && repo.nameWithOwner.startsWith(`${viewerLogin}/`))?.nameWithOwner
    || repos[0]?.nameWithOwner
    || ""
  );
}

function filterReposForViewer(repos: GitHubRepoOption[], viewerLogin: string): GitHubRepoOption[] {
  if (!viewerLogin) return repos;
  return repos.filter((repo) => repo.nameWithOwner.startsWith(`${viewerLogin}/`));
}

function getProposedIssueKey(issue: ProposedIssue): string {
  return JSON.stringify([
    issue.title.trim(),
    issue.body.trim(),
    issue.assignee.trim(),
  ]);
}

function getMeetingIssueResult(
  meeting: RoundTableMeeting,
  issue: ProposedIssue,
): IssueCreationResult | null {
  const key = getProposedIssueKey(issue);
  return meeting.issue_creation_results?.find((result) => result.key === key) ?? null;
}

function getMeetingIssueState(
  result: IssueCreationResult | null,
): "created" | "failed" | "discarded" | "pending" {
  if (!result) return "pending";
  if (result.discarded) return "discarded";
  return result.ok ? "created" : "failed";
}

export default function MeetingMinutesView({ meetings, onRefresh }: Props) {
  const { t, locale } = useI18n();
  const [detailMeeting, setDetailMeeting] = useState<RoundTableMeeting | null>(null);
  const [creatingIssue, setCreatingIssue] = useState<string | null>(null);
  const [discardingIssueIds, setDiscardingIssueIds] = useState<Record<string, boolean>>({});
  const [discardingMeetingIds, setDiscardingMeetingIds] = useState<Record<string, boolean>>({});
  const [deleting, setDeleting] = useState<string | null>(null);
  const [expandedIssues, setExpandedIssues] = useState<Set<string>>(new Set());
  const [showStartForm, setShowStartForm] = useState(false);
  const [agenda, setAgenda] = useState("");
  const [channelId, setChannelId] = useState(() => localStorage.getItem(STORAGE_KEY) || "");
  const [primaryProvider, setPrimaryProvider] = useState<string>("claude");
  const [showChannelEdit, setShowChannelEdit] = useState(false);
  const [starting, setStarting] = useState(false);
  const [startError, setStartError] = useState<string | null>(null);
  const [githubRepos, setGithubRepos] = useState<GitHubRepoOption[]>([]);
  const [repoOwner, setRepoOwner] = useState<string>("");
  const [meetingRepoSelections, setMeetingRepoSelections] = useState<Record<string, string>>({});
  const [savingRepoIds, setSavingRepoIds] = useState<Record<string, boolean>>({});
  const [repoSaveErrors, setRepoSaveErrors] = useState<Record<string, string>>({});
  const [loadingRepos, setLoadingRepos] = useState(true);
  const [repoError, setRepoError] = useState<string | null>(null);

  useEffect(() => {
    if (channelId) localStorage.setItem(STORAGE_KEY, channelId);
  }, [channelId]);

  useEffect(() => {
    let cancelled = false;

    getGitHubRepos().then((result) => {
      if (cancelled) return;

      setGithubRepos(filterReposForViewer(result.repos, result.viewer_login));
      setRepoOwner(result.viewer_login);
      setLoadingRepos(false);
      setRepoError(null);
    }).catch((error) => {
      if (cancelled) return;
      setGithubRepos([]);
      setRepoOwner("");
      setLoadingRepos(false);
      setRepoError(error instanceof Error ? error.message : t({ ko: "repo 목록을 불러오지 못했습니다", en: "Failed to load repo list" }));
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

  const handleOpenDetail = async (m: RoundTableMeeting) => {
    try {
      const full = await getRoundTableMeeting(m.id);
      setDetailMeeting(full);
    } catch {
      setDetailMeeting(m);
    }
  };

  const getSelectedRepo = (meeting: RoundTableMeeting) => {
    if (Object.prototype.hasOwnProperty.call(meetingRepoSelections, meeting.id)) {
      return meetingRepoSelections[meeting.id] ?? "";
    }
    return (
      (typeof meeting.issue_repo === "string" && meeting.issue_repo.trim())
      || getDefaultIssueRepo(githubRepos, repoOwner)
    );
  };

  const getRepoOptions = (selectedRepo: string) => {
    if (!selectedRepo || githubRepos.some((repo) => repo.nameWithOwner === selectedRepo)) {
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
    const hadPreviousSelection = Object.prototype.hasOwnProperty.call(meetingRepoSelections, meetingId);
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
        [meetingId]: e instanceof Error ? e.message : t({ ko: "repo 저장 실패", en: "Failed to save repo" }),
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

  const handleDiscardIssue = async (meetingId: string, issue: ProposedIssue) => {
    const issueKey = getProposedIssueKey(issue);
    const actionKey = `${meetingId}:${issueKey}`;

    if (!window.confirm(t({ ko: "이 일감은 생성하지 않기로 처리하시겠습니까?", en: "Discard this issue and skip creation?" }))) return;

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
    if (!window.confirm(t({ ko: "이 회의록의 생성되지 않은 일감을 전부 폐기하시겠습니까?", en: "Discard all uncreated issues from this meeting?" }))) return;

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
    if (!window.confirm(t({ ko: "이 회의록을 삭제하시겠습니까?", en: "Delete this meeting record?" }))) return;
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
    if (!agenda.trim() || !channelId.trim()) return;
    setStarting(true);
    setStartError(null);
    try {
      await startRoundTableMeeting(agenda.trim(), channelId.trim(), primaryProvider);
      setAgenda("");
      setShowStartForm(false);
    } catch (e) {
      setStartError(e instanceof Error ? e.message : t({ ko: "회의 시작 실패", en: "Failed to start meeting" }));
    } finally {
      setStarting(false);
    }
  };

  const statusBadge = (status: string) => {
    const map: Record<string, { bg: string; color: string; label: string }> = {
      completed: { bg: "rgba(16,185,129,0.15)", color: "#34d399", label: t({ ko: "완료", en: "Completed" }) },
      in_progress: { bg: "rgba(245,158,11,0.15)", color: "#fbbf24", label: t({ ko: "진행중", en: "In Progress" }) },
      cancelled: { bg: "rgba(239,68,68,0.15)", color: "#f87171", label: t({ ko: "취소", en: "Cancelled" }) },
    };
    const s = map[status] || map.completed;
    return (
      <span
        className="text-[10px] px-2 py-0.5 rounded-full font-medium"
        style={{ background: s.bg, color: s.color }}
      >
        {s.label}
      </span>
    );
  };

  const inputStyle = { background: "var(--th-bg-surface)", border: "1px solid var(--th-border)", color: "var(--th-text)" };

  const getIssueProgress = (meeting: RoundTableMeeting) => {
    const total = meeting.proposed_issues?.length ?? 0;
    const results = meeting.issue_creation_results ?? [];
    const createdFromResults = results.filter((result) => result.ok && result.discarded !== true).length;
    const created = Math.min(createdFromResults > 0 ? createdFromResults : meeting.issues_created || 0, total);
    const failed = Math.min(
      results.filter((result) => !result.ok && result.discarded !== true).length,
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

  const getIssueProgressText = (issueProgress: ReturnType<typeof getIssueProgress>) => {
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

  return (
    <div
      className="p-4 sm:p-6 max-w-4xl mx-auto overflow-y-auto overflow-x-hidden h-full pb-40"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center gap-3">
          <FileText className="text-amber-400" size={24} />
          <h1 className="text-xl font-bold" style={{ color: "var(--th-text-heading)" }}>
            {t({ ko: "라운드 테이블 회의", en: "Round Table Meeting" })}
          </h1>
          <span className="text-xs px-2 py-0.5 rounded-full" style={{ background: "rgba(245,158,11,0.15)", color: "#fbbf24" }}>
            {meetings.length}
          </span>
        </div>
        <button
          onClick={() => setShowStartForm((v) => !v)}
          className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-amber-600 hover:bg-amber-500 text-white transition-colors"
        >
          <Plus size={14} />
          {t({ ko: "새 회의", en: "New Meeting" })}
        </button>
      </div>

      {/* Start meeting form */}
      {showStartForm && (
        <div
          className="rounded-2xl border p-4 sm:p-5 mb-6 space-y-3"
          style={{ background: "var(--th-surface)", borderColor: "var(--th-border)" }}
        >
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "회의 시작", en: "Start Meeting" })}
          </h3>

          {/* Channel ID row */}
          <div className="flex flex-col gap-1 sm:flex-row sm:items-center sm:gap-2">
            <label className="text-[10px] font-semibold uppercase tracking-widest shrink-0 sm:w-20" style={{ color: "var(--th-text-muted)" }}>
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
                <button
                  onClick={() => setShowChannelEdit(true)}
                  className="p-1 rounded hover:bg-white/10 transition-colors"
                  title={t({ ko: "채널 ID 변경", en: "Change Channel ID" })}
                >
                  <Settings2 size={12} style={{ color: "var(--th-text-muted)" }} />
                </button>
              </div>
            )}
          </div>

          {/* Agenda input */}
          <div className="flex flex-col gap-1 sm:flex-row sm:items-start sm:gap-2">
            <label className="text-[10px] font-semibold uppercase tracking-widest shrink-0 sm:w-20 sm:pt-2" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "안건", en: "Agenda" })}
            </label>
            <input
              type="text"
              value={agenda}
              onChange={(e) => setAgenda(e.target.value)}
              placeholder={t({ ko: "회의 안건을 입력하세요", en: "Enter meeting agenda" })}
              className="flex-1 px-3 py-1.5 rounded-lg text-sm"
              style={inputStyle}
              onKeyDown={(e) => { if (e.key === "Enter" && !e.nativeEvent.isComposing) handleStartMeeting(); }}
            />
          </div>

          <div className="flex flex-col gap-1 sm:flex-row sm:items-center sm:gap-2">
            <label className="text-[10px] font-semibold uppercase tracking-widest shrink-0 sm:w-20" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "진행 모델", en: "Model" })}
            </label>
            <select
              value={primaryProvider}
              onChange={(e) => setPrimaryProvider(e.target.value)}
              className="px-3 py-1.5 rounded-lg text-xs"
              style={inputStyle}
            >
              {MEETING_PROVIDERS.map((p) => (
                <option key={p} value={p}>{PROVIDER_LABELS[p] ?? p.toUpperCase()}</option>
              ))}
            </select>
            <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "반대 모델이 자동 교차검증", en: "Counter model auto cross-review" })}
            </span>
          </div>

          {startError && (
            <div className="text-xs px-3 py-1.5 rounded-lg" style={{ background: "rgba(239,68,68,0.1)", color: "#f87171" }}>
              {startError}
            </div>
          )}

          <div className="flex items-center gap-2 justify-end">
            <button
              onClick={() => setShowStartForm(false)}
              className="px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors hover:bg-white/5"
              style={{ borderColor: "var(--th-border)", color: "var(--th-text-muted)" }}
            >
              {t({ ko: "취소", en: "Cancel" })}
            </button>
            <button
              onClick={handleStartMeeting}
              disabled={starting || !agenda.trim() || !channelId.trim()}
              className="px-4 py-1.5 rounded-lg text-xs font-medium bg-amber-600 hover:bg-amber-500 text-white transition-colors disabled:opacity-40"
            >
              {starting ? t({ ko: "시작 중...", en: "Starting..." }) : t({ ko: "회의 시작", en: "Start Meeting" })}
            </button>
          </div>
        </div>
      )}

      {/* Empty state */}
      {meetings.length === 0 && !showStartForm && (
        <div className="text-center py-16" style={{ color: "var(--th-text-muted)" }}>
          <FileText size={48} className="mx-auto mb-4 opacity-30" />
          <p>{t({ ko: "회의 기록이 없습니다", en: "No meeting records" })}</p>
          <p className="text-sm mt-1">{t({ ko: "\"새 회의\" 버튼으로 라운드 테이블을 시작하세요", en: "Start a round table with the \"New Meeting\" button" })}</p>
        </div>
      )}

      {/* Meeting list */}
      <div className="space-y-4">
        {meetings.map((m) => {
          const hasProposedIssues = m.proposed_issues && m.proposed_issues.length > 0;
          const issuesExpanded = expandedIssues.has(m.id);
          const issueProgress = getIssueProgress(m);
          const selectedRepo = getSelectedRepo(m);
          const repoOptions = getRepoOptions(selectedRepo);
          const isSavingRepo = !!savingRepoIds[m.id];
          const canRetryIssues = hasProposedIssues && !issueProgress.allResolved && !!selectedRepo && !isSavingRepo;

          return (
            <div
              key={m.id}
              className="rounded-2xl border p-4 sm:p-5 space-y-3"
              style={{ background: "var(--th-surface)", borderColor: "var(--th-border)" }}
            >
              {/* Top row */}
              <div className="flex items-start justify-between gap-3">
                <div className="min-w-0 flex-1">
                  <h3 className="font-semibold text-base" style={{ color: "var(--th-text)" }}>
                    {m.agenda}
                  </h3>
                  <div className="flex items-center gap-2 mt-1.5 flex-wrap">
                    {statusBadge(m.status)}
                    {(m.primary_provider || m.reviewer_provider) && (
                      <span className="text-[10px] px-2 py-0.5 rounded-full font-medium" style={{ background: "rgba(59,130,246,0.12)", color: "#93c5fd" }}>
                        {formatProviderFlow(m.primary_provider, m.reviewer_provider)}
                      </span>
                    )}
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {new Date(m.started_at).toLocaleDateString(locale)}
                    </span>
                    {m.total_rounds > 0 && (
                      <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
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
                  <Trash2 size={14} style={{ color: deleting === m.id ? "var(--th-text-muted)" : "#f87171" }} />
                </button>
              </div>

              {/* Participants */}
              <div className="flex items-center gap-1.5 flex-wrap">
                {m.participant_names.map((name) => (
                  <span
                    key={name}
                    className="text-[10px] px-2 py-0.5 rounded-full font-medium"
                    style={{ background: "rgba(99,102,241,0.15)", color: "#818cf8" }}
                  >
                    {name}
                  </span>
                ))}
              </div>

              {(m.primary_provider || m.reviewer_provider) && (
                <div className="space-y-1.5">
                  <MeetingProviderFlow
                    primaryProvider={m.primary_provider}
                    reviewerProvider={m.reviewer_provider}
                  />
                  <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {providerFlowCaption(m.primary_provider, m.reviewer_provider, t)}
                  </div>
                </div>
              )}

              {/* PMD Summary bubble */}
              {m.summary && (
                <div className="flex items-start gap-2.5">
                  <div className="w-7 h-7 rounded-lg overflow-hidden shrink-0" style={{ background: "var(--th-bg-surface)" }}>
                    <img
                      src="/sprites/7-D-1.png"
                      alt="PMD"
                      className="w-full h-full object-cover"
                      style={{ imageRendering: "pixelated" }}
                    />
                  </div>
                  <div
                    className="rounded-xl rounded-tl-sm px-3 py-2 text-sm flex-1"
                    style={{
                      background: "rgba(99,102,241,0.08)",
                      border: "1px solid rgba(99,102,241,0.15)",
                      color: "var(--th-text)",
                    }}
                  >
                    <div className="flex items-center justify-between gap-2 mb-1 flex-wrap">
                      <div className="text-[10px] font-semibold" style={{ color: "#818cf8" }}>{t({ ko: "PMD 요약", en: "PMD Summary" })}</div>
                      {(m.primary_provider || m.reviewer_provider) && (
                        <div className="text-[10px]" style={{ color: "var(--th-text-muted)" }}>
                          {providerFlowCaption(m.primary_provider, m.reviewer_provider, t)}
                        </div>
                      )}
                    </div>
                    <MarkdownContent content={m.summary} />
                  </div>
                </div>
              )}

              {/* Proposed issues preview */}
              {hasProposedIssues && !issueProgress.allCreated && (
                <div>
                  <button
                    onClick={() => toggleIssuePreview(m.id)}
                    className="flex items-center gap-1.5 text-xs font-medium transition-colors hover:opacity-80"
                    style={{ color: "#34d399" }}
                  >
                    {issuesExpanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
                    {t({ ko: `생성될 일감 미리보기 (${m.proposed_issues!.length}건)`, en: `Preview issues to create (${m.proposed_issues!.length})` })}
                  </button>
                  {issuesExpanded && (
                    <div className="mt-2 space-y-1.5">
                      {m.proposed_issues!.map((issue, i) => {
                        const issueResult = getMeetingIssueResult(m, issue);
                        const issueState = getMeetingIssueState(issueResult);
                        const issueKey = getProposedIssueKey(issue);
                        const actionKey = `${m.id}:${issueKey}`;
                        const isDiscardingIssue = !!discardingIssueIds[actionKey];
                        const statusMeta = issueState === "created"
                          ? {
                              label: t({ ko: "생성됨", en: "Created" }),
                              color: "#34d399",
                              bg: "rgba(16,185,129,0.12)",
                              border: "rgba(16,185,129,0.18)",
                            }
                          : issueState === "discarded"
                            ? {
                                label: t({ ko: "폐기됨", en: "Discarded" }),
                                color: "#94a3b8",
                                bg: "rgba(148,163,184,0.12)",
                                border: "rgba(148,163,184,0.18)",
                              }
                            : issueState === "failed"
                              ? {
                                  label: t({ ko: "실패", en: "Failed" }),
                                  color: "#fbbf24",
                                  bg: "rgba(245,158,11,0.12)",
                                  border: "rgba(245,158,11,0.18)",
                                }
                              : {
                                  label: t({ ko: "대기", en: "Pending" }),
                                  color: "#60a5fa",
                                  bg: "rgba(96,165,250,0.12)",
                                  border: "rgba(96,165,250,0.18)",
                                };

                        return (
                          <div
                            key={i}
                            className="rounded-lg px-3 py-2 text-xs"
                            style={{
                              background: statusMeta.bg,
                              border: `1px solid ${statusMeta.border}`,
                            }}
                          >
                            <div className="flex items-start justify-between gap-2">
                              <div className="min-w-0 flex-1">
                                <div className="font-medium" style={{ color: "var(--th-text)" }}>
                                  [RT] {issue.title}
                                </div>
                                <div className="mt-0.5" style={{ color: "var(--th-text-muted)" }}>
                                  {t({ ko: `담당: ${issue.assignee}`, en: `Assignee: ${issue.assignee}` })}
                                </div>
                                {issueResult?.error && issueState === "failed" && (
                                  <div className="mt-1" style={{ color: "#fbbf24" }}>
                                    {t({ ko: `실패: ${issueResult.error}`, en: `Failed: ${issueResult.error}` })}
                                  </div>
                                )}
                                {issueResult?.issue_url && issueState === "created" && (
                                  <a
                                    href={issueResult.issue_url}
                                    target="_blank"
                                    rel="noreferrer"
                                    className="mt-1 inline-flex hover:underline"
                                    style={{ color: "#34d399" }}
                                  >
                                    {t({ ko: "생성된 이슈 열기", en: "Open created issue" })}
                                  </a>
                                )}
                              </div>
                              <div className="flex shrink-0 items-center gap-1.5">
                                <span
                                  className="rounded-full px-2 py-0.5 text-[10px] font-semibold"
                                  style={{ background: statusMeta.bg, color: statusMeta.color }}
                                >
                                  {statusMeta.label}
                                </span>
                                {(issueState === "pending" || issueState === "failed") && (
                                  <button
                                    onClick={() => void handleDiscardIssue(m.id, issue)}
                                    disabled={isDiscardingIssue}
                                    className="inline-flex items-center gap-1 rounded-full px-2 py-1 text-[10px] font-semibold transition-colors disabled:opacity-50"
                                    style={{
                                      background: "rgba(148,163,184,0.12)",
                                      color: "#cbd5e1",
                                      border: "1px solid rgba(148,163,184,0.2)",
                                    }}
                                  >
                                    <Trash2 size={11} />
                                    {isDiscardingIssue ? t({ ko: "폐기 중...", en: "Discarding..." }) : t({ ko: "폐기", en: "Discard" })}
                                  </button>
                                )}
                              </div>
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>
              )}

              {hasProposedIssues && (
                <div className="text-xs" style={{ color: issueProgress.failed > 0 ? "#fbbf24" : issueProgress.discarded > 0 ? "#cbd5e1" : "var(--th-text-muted)" }}>
                  {getIssueProgressText(issueProgress)}
                </div>
              )}

              {/* Actions */}
              <div className="flex flex-col gap-2 pt-1 sm:flex-row sm:flex-wrap sm:items-end sm:justify-between min-w-0">
                <div className="flex items-center gap-2 flex-wrap min-w-0">
                  <button
                    onClick={() => handleOpenDetail(m)}
                    className="px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors hover:bg-white/5"
                    style={{ borderColor: "var(--th-border)", color: "var(--th-text-secondary)" }}
                  >
                    {t({ ko: "상세 보기", en: "Details" })}
                  </button>
                  {hasProposedIssues ? (
                    <>
                      <button
                        onClick={() => handleCreateIssues(m.id, selectedRepo)}
                        disabled={!canRetryIssues || creatingIssue === m.id}
                        className="px-3 py-1.5 rounded-lg text-xs font-medium transition-colors disabled:opacity-40"
                        style={{
                          background: issueProgress.allCreated || issueProgress.allResolved
                            ? "transparent"
                            : issueProgress.failed > 0
                              ? "rgba(245,158,11,0.15)"
                              : "rgba(16,185,129,0.15)",
                          color: issueProgress.allCreated || issueProgress.allResolved
                            ? "var(--th-text-muted)"
                            : issueProgress.failed > 0
                              ? "#fbbf24"
                              : "#34d399",
                          border: `1px solid ${issueProgress.allCreated || issueProgress.allResolved
                            ? "var(--th-border)"
                            : issueProgress.failed > 0
                              ? "rgba(245,158,11,0.3)"
                              : "rgba(16,185,129,0.3)"}`,
                        }}
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
                      </button>
                      {issueProgress.pending + issueProgress.failed > 0 && (
                        <button
                          onClick={() => void handleDiscardAllIssues(m.id)}
                          disabled={!!discardingMeetingIds[m.id]}
                          className="px-3 py-1.5 rounded-lg text-xs font-medium transition-colors disabled:opacity-40"
                          style={{
                            background: "rgba(148,163,184,0.12)",
                            color: "#cbd5e1",
                            border: "1px solid rgba(148,163,184,0.2)",
                          }}
                        >
                          {!!discardingMeetingIds[m.id]
                            ? t({ ko: "전체 폐기 중...", en: "Discarding all..." })
                            : t({ ko: `남은 일감 전체 폐기 (${issueProgress.pending + issueProgress.failed}건)`, en: `Discard all remaining (${issueProgress.pending + issueProgress.failed})` })}
                        </button>
                      )}
                    </>
                  ) : (
                    m.issues_created ? (
                      <span className="px-3 py-1.5 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                        {t({ ko: "일감 생성 완료", en: "Issues created" })}
                      </span>
                    ) : (
                      <span className="px-3 py-1.5 text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>
                        {t({ ko: "추출된 일감 없음", en: "No issues extracted" })}
                      </span>
                    )
                  )}
                </div>
                {hasProposedIssues && (
                  <div className="flex flex-col gap-1 min-w-0 sm:min-w-[280px]">
                    <div className="text-[10px] font-semibold uppercase tracking-widest text-left sm:text-right" style={{ color: "var(--th-text-muted)" }}>
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
                    <div className="text-[11px] text-left sm:text-right" style={{ color: repoSaveErrors[m.id] ? "#fbbf24" : "var(--th-text-muted)" }}>
                      {repoSaveErrors[m.id]
                        || (isSavingRepo ? t({ ko: "repo 저장 중...", en: "Saving repo..." }) : null)
                        || repoError
                        || (loadingRepos ? t({ ko: "repo 목록 불러오는 중...", en: "Loading repos..." }) : null)
                        || (repoOwner ? t({ ko: `gh 계정 ${repoOwner}`, en: `gh account ${repoOwner}` }) : "")}
                    </div>
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>

      {detailMeeting && (
        <MeetingDetailModal
          meeting={detailMeeting}
          onClose={() => setDetailMeeting(null)}
        />
      )}
    </div>
  );
}
