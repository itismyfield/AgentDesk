import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import type { RoundTableMeeting } from "../types";
import { I18nProvider } from "../i18n";
import MeetingDetailModal from "./MeetingDetailModal";

function meeting(
  overrides: Partial<RoundTableMeeting> = {},
): RoundTableMeeting {
  return {
    id: "meeting-1",
    channel_id: "meeting",
    meeting_hash: "#meeting-aaaaaa",
    thread_hash: "#thread-bbbbbb",
    agenda: "전문가 회의",
    summary: "요약",
    selection_reason:
      "안건 대응에 필요한 핵심 전문성을 가장 넓게 커버하는 조합으로 선정",
    status: "completed",
    primary_provider: "claude",
    reviewer_provider: "qwen",
    participant_names: ["Alice", "Bob"],
    total_rounds: 2,
    issues_created: 0,
    proposed_issues: null,
    issue_creation_results: null,
    issue_repo: null,
    started_at: 1710000000000,
    completed_at: 1710000300000,
    created_at: 1710000000000,
    entries: [],
    ...overrides,
  };
}

describe("MeetingDetailModal provider flow contract", () => {
  it("renders the provider flow panel with both chips and a caption summary", () => {
    const markup = renderToStaticMarkup(
      createElement(
        I18nProvider,
        {
          language: "ko",
          children: createElement(MeetingDetailModal, {
            meeting: meeting(),
            onClose: () => {},
          }),
        },
      ),
    );

    expect(markup).toContain("Claude");
    expect(markup).toContain("Qwen");
    expect(markup).toContain("Provider Flow");
    expect(markup).toContain("초안/최종");
    expect(markup).toContain("비판 검토");
    expect(markup).toContain("초안/최종: Claude · 비판 검토: Qwen");
  });
});
