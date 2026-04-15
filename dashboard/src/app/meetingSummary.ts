import type { RoundTableMeeting } from "../types";

export function countOpenMeetingIssues(meeting: RoundTableMeeting): number {
  const totalIssues = meeting.proposed_issues?.length ?? 0;
  if (meeting.status !== "completed" || totalIssues === 0) return 0;

  const results = meeting.issue_creation_results ?? [];
  if (results.length === 0) {
    return Math.max(totalIssues - meeting.issues_created, 0);
  }

  const created = results.filter((result) => result.ok && result.discarded !== true).length;
  const discarded = results.filter((result) => result.discarded === true).length;
  return Math.max(totalIssues - created - discarded, 0);
}

export function summarizeMeetings(meetings: RoundTableMeeting[]) {
  return meetings.reduce(
    (summary, meeting) => {
      if (meeting.status === "in_progress") {
        summary.activeCount += 1;
      }
      summary.unresolvedCount += countOpenMeetingIssues(meeting);
      return summary;
    },
    { activeCount: 0, unresolvedCount: 0 },
  );
}
