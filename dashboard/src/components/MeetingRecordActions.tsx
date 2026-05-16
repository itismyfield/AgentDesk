import type { CSSProperties } from "react";
import type { I18nContextValue } from "../i18n";
import type { GitHubRepoOption } from "../api/client";
import type { MeetingIssueProgress } from "./meetingIssueProgress";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceNotice,
} from "./common/SurfacePrimitives";

interface MeetingRecordActionsProps {
  t: I18nContextValue["t"];
  hasProposedIssues: boolean;
  issuesCreated: number;
  issueProgress: MeetingIssueProgress;
  selectedRepo: string;
  repoOptions: GitHubRepoOption[];
  githubRepos: GitHubRepoOption[];
  loadingRepos: boolean;
  isSavingRepo: boolean;
  repoSaveError?: string;
  repoError: string | null;
  repoOwner: string;
  creatingIssue: boolean;
  discardingAllIssues: boolean;
  inputStyle: CSSProperties;
  onOpenDetail: () => void;
  onCreateIssues: () => void;
  onDiscardAllIssues: () => void;
  onRepoChange: (repo: string) => void;
}

export default function MeetingRecordActions({
  t,
  hasProposedIssues,
  issuesCreated,
  issueProgress,
  selectedRepo,
  repoOptions,
  githubRepos,
  loadingRepos,
  isSavingRepo,
  repoSaveError,
  repoError,
  repoOwner,
  creatingIssue,
  discardingAllIssues,
  inputStyle,
  onOpenDetail,
  onCreateIssues,
  onDiscardAllIssues,
  onRepoChange,
}: MeetingRecordActionsProps) {
  const canRetryIssues =
    hasProposedIssues &&
    !issueProgress.allResolved &&
    !!selectedRepo &&
    !isSavingRepo;
  const createButtonTone =
    issueProgress.allCreated || issueProgress.allResolved
      ? "neutral"
      : issueProgress.failed > 0
        ? "warn"
        : "accent";

  return (
    <SurfaceCard
      className="rounded-2xl p-3"
      style={{
        background:
          "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
        borderColor:
          "color-mix(in srgb, var(--th-border) 68%, transparent)",
      }}
    >
      <div className="flex min-w-0 flex-col gap-3 sm:flex-row sm:flex-wrap sm:items-end sm:justify-between">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <SurfaceActionButton tone="neutral" onClick={onOpenDetail}>
            {t({ ko: "상세 보기", en: "Details" })}
          </SurfaceActionButton>
          {hasProposedIssues ? (
            <>
              <SurfaceActionButton
                tone={createButtonTone}
                onClick={onCreateIssues}
                disabled={!canRetryIssues || creatingIssue}
              >
                {issueProgress.allCreated
                  ? t({
                      ko: `일감 생성 완료 (${issueProgress.created}/${issueProgress.total})`,
                      en: `Issues created (${issueProgress.created}/${issueProgress.total})`,
                    })
                  : issueProgress.allResolved
                    ? t({
                        ko: `일감 처리 완료 (생성 ${issueProgress.created}, 폐기 ${issueProgress.discarded})`,
                        en: `Issues resolved (created ${issueProgress.created}, discarded ${issueProgress.discarded})`,
                      })
                    : creatingIssue
                      ? t({ ko: "생성 중...", en: "Creating..." })
                      : isSavingRepo
                        ? t({ ko: "Repo 저장 중...", en: "Saving repo..." })
                        : !selectedRepo
                          ? t({ ko: "Repo 선택 필요", en: "Select repo" })
                          : issueProgress.failed > 0
                            ? t({
                                ko: `실패분 재시도 (${issueProgress.created}/${issueProgress.total})`,
                                en: `Retry failed (${issueProgress.created}/${issueProgress.total})`,
                              })
                            : t({
                                ko: `일감 생성 (${issueProgress.total}건)`,
                                en: `Create issues (${issueProgress.total})`,
                              })}
              </SurfaceActionButton>
              {issueProgress.pending + issueProgress.failed > 0 && (
                <SurfaceActionButton
                  tone="neutral"
                  onClick={onDiscardAllIssues}
                  disabled={discardingAllIssues}
                >
                  {discardingAllIssues
                    ? t({ ko: "전체 폐기 중...", en: "Discarding all..." })
                    : t({
                        ko: `남은 일감 전체 폐기 (${issueProgress.pending + issueProgress.failed}건)`,
                        en: `Discard all remaining (${issueProgress.pending + issueProgress.failed})`,
                      })}
                </SurfaceActionButton>
              )}
            </>
          ) : issuesCreated ? (
            <SurfaceNotice compact tone="success">
              {t({ ko: "일감 생성 완료", en: "Issues created" })}
            </SurfaceNotice>
          ) : (
            <SurfaceNotice compact tone="neutral">
              {t({ ko: "추출된 일감 없음", en: "No issues extracted" })}
            </SurfaceNotice>
          )}
        </div>
        {hasProposedIssues && (
          <div className="flex min-w-0 flex-col gap-1 sm:min-w-[280px]">
            <div
              className="text-left text-xs font-semibold uppercase tracking-widest sm:text-right"
              style={{ color: "var(--th-text-muted)" }}
            >
              {t({ ko: "이 회의용 Repo", en: "Repo for this meeting" })}
            </div>
            <select
              value={selectedRepo}
              onChange={(event) => onRepoChange(event.target.value)}
              className="rounded-lg px-3 py-2 text-sm"
              style={inputStyle}
              disabled={loadingRepos || isSavingRepo || repoOptions.length === 0}
            >
              {!selectedRepo && (
                <option value="">
                  {t({ ko: "Repo 선택", en: "Select repo" })}
                </option>
              )}
              {repoOptions.map((repo) => (
                <option key={repo.nameWithOwner} value={repo.nameWithOwner}>
                  {githubRepos.some(
                    (item) => item.nameWithOwner === repo.nameWithOwner,
                  )
                    ? repo.nameWithOwner
                    : `${repo.nameWithOwner} ${t({
                        ko: "(현재 목록에 없음)",
                        en: "(not in current list)",
                      })}`}
                </option>
              ))}
            </select>
            <div
              className="text-left text-xs sm:text-right"
              style={{
                color: repoSaveError ? "#fbbf24" : "var(--th-text-muted)",
              }}
            >
              {repoSaveError ||
                (isSavingRepo
                  ? t({ ko: "repo 저장 중...", en: "Saving repo..." })
                  : null) ||
                repoError ||
                (loadingRepos
                  ? t({
                      ko: "repo 목록 불러오는 중...",
                      en: "Loading repos...",
                    })
                  : null) ||
                (repoOwner
                  ? t({
                      ko: `gh 계정 ${repoOwner}`,
                      en: `gh account ${repoOwner}`,
                    })
                  : "")}
            </div>
          </div>
        )}
      </div>
    </SurfaceCard>
  );
}
