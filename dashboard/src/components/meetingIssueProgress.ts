import type { I18nContextValue } from "../i18n";
import type { RoundTableMeeting } from "../types";

export interface MeetingIssueProgress {
  total: number;
  created: number;
  failed: number;
  discarded: number;
  pending: number;
  allCreated: boolean;
  allResolved: boolean;
}

export function getMeetingIssueProgress(
  meeting: RoundTableMeeting,
): MeetingIssueProgress {
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
}

export function getMeetingIssueProgressText(
  issueProgress: MeetingIssueProgress,
  t: I18nContextValue["t"],
): string {
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
}
