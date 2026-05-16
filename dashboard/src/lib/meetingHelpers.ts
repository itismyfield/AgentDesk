import type {
  IssueCreationResult,
  ProposedIssue,
  RoundTableMeeting,
} from "../types";

export type MeetingIssueState = "created" | "failed" | "discarded" | "pending";
export type MeetingIssueTone = "ok" | "warn" | "err" | "neutral";

export function getProposedIssueKey(issue: ProposedIssue): string {
  return JSON.stringify([
    issue.title.trim(),
    issue.body.trim(),
    issue.assignee.trim(),
  ]);
}

export function getMeetingIssueResult(
  meeting: RoundTableMeeting,
  issue: ProposedIssue,
): IssueCreationResult | null {
  const key = getProposedIssueKey(issue);
  return (
    meeting.issue_creation_results?.find((result) => result.key === key) ?? null
  );
}

export function getMeetingIssueState(
  result: IssueCreationResult | null,
): MeetingIssueState {
  if (!result) return "pending";
  if (result.discarded) return "discarded";
  return result.ok ? "created" : "failed";
}

export function getMeetingIssueTone(state: MeetingIssueState): MeetingIssueTone {
  if (state === "created") return "ok";
  if (state === "failed") return "err";
  if (state === "discarded") return "neutral";
  return "warn";
}
