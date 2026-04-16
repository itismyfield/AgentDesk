import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import type {
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
  RoundTableMeetingExpertOption,
} from "../types";
import { I18nProvider } from "../i18n";
import {
  filterMeetingExpertsByQuery,
  getMeetingReferenceHashes,
  default as MeetingMinutesView,
  openMeetingDetailWithFallback,
  pruneFixedParticipantRoleIdsForLoadedChannel,
  submitMeetingStartRequest,
} from "./MeetingMinutesView";

function expert(roleId: string): RoundTableMeetingExpertOption {
  return {
    role_id: roleId,
    display_name: roleId.toUpperCase(),
    keywords: [],
    strengths: [],
    task_types: [],
    anti_signals: [],
    metadata_missing: false,
    metadata_confidence: "high",
  };
}

function channel(roleIds: string[]): RoundTableMeetingChannelOption {
  return {
    channel_id: "meeting",
    channel_name: "회의",
    owner_provider: "claude",
    available_experts: roleIds.map(expert),
  };
}

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

const t = ({ ko }: { ko: string; en: string }) => ko;

function installLocalStorage(values: Record<string, string> = {}) {
  const storage = {
    getItem: vi.fn((key: string) => values[key] ?? null),
    setItem: vi.fn((key: string, value: string) => {
      values[key] = value;
    }),
    removeItem: vi.fn((key: string) => {
      delete values[key];
    }),
    clear: vi.fn(() => {
      for (const key of Object.keys(values)) delete values[key];
    }),
  };
  vi.stubGlobal("localStorage", storage);
  return storage;
}

function renderMeetingStartFormMarkup() {
  installLocalStorage({
    "climpire.language": "ko",
    pcd_meeting_fixed_participants: "[]",
  });

  return renderToStaticMarkup(
    createElement(
      I18nProvider,
      { language: "ko" },
      createElement(MeetingMinutesView, {
        meetings: [],
        onRefresh: () => {},
        initialShowStartForm: true,
        initialChannelId: "meeting",
        initialMeetingChannels: [channel(["qwen", "gemini"])],
      }),
    ),
  );
}

function renderMeetingListMarkup(meetings: RoundTableMeeting[]) {
  installLocalStorage({
    "climpire.language": "ko",
    pcd_meeting_fixed_participants: "[]",
  });

  return renderToStaticMarkup(
    createElement(
      I18nProvider,
      { language: "ko" },
      createElement(MeetingMinutesView, {
        meetings,
        onRefresh: () => {},
      }),
    ),
  );
}

function renderEmbeddedMeetingListMarkup(meetings: RoundTableMeeting[]) {
  installLocalStorage({
    "climpire.language": "ko",
    pcd_meeting_fixed_participants: "[]",
  });

  return renderToStaticMarkup(
    createElement(
      I18nProvider,
      { language: "ko" },
      createElement(MeetingMinutesView, {
        meetings,
        onRefresh: () => {},
        embedded: true,
      }),
    ),
  );
}
describe("pruneFixedParticipantRoleIdsForLoadedChannel", () => {
  it("keeps stored fixed participants while meeting channels are loading", () => {
    const previous = ["td", "pd"];

    const next = pruneFixedParticipantRoleIdsForLoadedChannel(
      previous,
      true,
      null,
    );

    expect(next).toBe(previous);
    expect(next).toEqual(["td", "pd"]);
  });

  it("keeps stored fixed participants until a selected channel exists", () => {
    const previous = ["td", "pd"];

    const next = pruneFixedParticipantRoleIdsForLoadedChannel(
      previous,
      false,
      null,
    );

    expect(next).toBe(previous);
    expect(next).toEqual(["td", "pd"]);
  });

  it("prunes unavailable fixed participants only after a selected channel is loaded", () => {
    const previous = ["td", "unknown", "pd"];

    const next = pruneFixedParticipantRoleIdsForLoadedChannel(
      previous,
      false,
      channel(["td", "pd"]),
    );

    expect(next).toEqual(["td", "pd"]);
  });

  it("clears stale fixed participants when the selected channel has no available experts", () => {
    const previous = ["td", "pd"];
    const next = pruneFixedParticipantRoleIdsForLoadedChannel(
      previous,
      false,
      channel([]),
    );

    expect(next).toEqual([]);
  });
});

describe("filterMeetingExpertsByQuery", () => {
  it("returns every expert when the query is blank", () => {
    const experts = [expert("td"), expert("pd")];

    expect(filterMeetingExpertsByQuery(experts, "   ")).toEqual(experts);
  });

  it("matches expert metadata case-insensitively across role id and specialist fields", () => {
    const matched = expert("gemini-domain");
    matched.domain_summary = "Meeting orchestration and provider review";
    matched.provider_hint = "gemini";
    matched.strengths = ["Architecture"];
    matched.task_types = ["Spec Review"];
    matched.anti_signals = ["One-line status"];

    const unmatched = expert("codex-ui");
    unmatched.domain_summary = "Frontend polish";

    expect(
      filterMeetingExpertsByQuery([matched, unmatched], "gemini").map(
        (item) => item.role_id,
      ),
    ).toEqual(["gemini-domain"]);
    expect(
      filterMeetingExpertsByQuery([matched, unmatched], "architecture").map(
        (item) => item.role_id,
      ),
    ).toEqual(["gemini-domain"]);
    expect(
      filterMeetingExpertsByQuery([matched, unmatched], "spec review").map(
        (item) => item.role_id,
      ),
    ).toEqual(["gemini-domain"]);
  });
});

describe("MeetingMinutesView rendered form contract", () => {
  it("renders the current meeting start form contract", () => {
    const markup = renderMeetingStartFormMarkup();

    expect(markup).toContain("진행 프로바이더");
    expect(markup).toContain("반대 모델이 자동 교차검증");
    expect(markup).toContain('placeholder="회의 안건을 입력하세요"');
    expect(markup).toContain("리뷰 프로바이더");
    expect(markup).toContain("고정 전문 에이전트");
  });

  it("renders the current provider options in the primary model selector", () => {
    const markup = renderMeetingStartFormMarkup();

    expect(markup).toContain("Claude");
    expect(markup).toContain("Codex");
    expect(markup).toContain("Gemini");
    expect(markup).toContain("Qwen");
  });
});

describe("MeetingMinutesView rendered meeting cards", () => {
  it("renders labeled meeting and thread hashes on dashboard cards", () => {
    const markup = renderMeetingListMarkup([meeting()]);

    expect(markup).toContain("회의 해시 :");
    expect(markup).toContain("스레드 해시 :");
    expect(markup).toContain("#aaaaaa");
    expect(markup).toContain("#bbbbbb");
    expect(markup.match(/#aaaaaa/g) ?? []).toHaveLength(1);
    expect(markup.match(/#bbbbbb/g) ?? []).toHaveLength(1);
  });

  it("renders the compact participant selection reason on dashboard cards", () => {
    const markup = renderMeetingListMarkup([
      meeting({
        selection_reason:
          "선정 사유: 안건 대응에 필요한 핵심 전문성을 가장 넓게 커버하는 조합으로 선정",
      }),
    ]);

    expect(markup).toContain("선정 사유:");
    expect(markup).toContain(
      "안건 대응에 필요한 핵심 전문성을 가장 넓게 커버하는 조합으로 선정",
    );
    expect(markup.match(/선정 사유:/g) ?? []).toHaveLength(1);
  });

  it("keeps provider flow labels only in the top row of meeting cards", () => {
    const markup = renderMeetingListMarkup([meeting({ summary: "" })]);

    expect(markup.match(/초안\/최종/g) ?? []).toHaveLength(1);
    expect(markup.match(/비판 검토/g) ?? []).toHaveLength(1);
    expect(markup).not.toContain("초안/최종:");
    expect(markup).not.toContain("비판 검토:");
  });

  it("omits the page header copy in embedded mode", () => {
    const markup = renderEmbeddedMeetingListMarkup([meeting()]);

    expect(markup).not.toContain(
      "라운드 테이블 상세와 후속 일감 상태를 함께 관리합니다.",
    );
    expect(markup).toContain("새 회의");
  });
});

describe("submitMeetingStartRequest", () => {
  it("shows accepted notification immediately and upgrades the same notification on success", async () => {
    const notifications: Array<{
      kind: "notify" | "update";
      id: string;
      message: string;
      type: string | undefined;
    }> = [];
    const notify = vi.fn(
      (message: string, type?: "info" | "success" | "warning" | "error") => {
        const id = `n-${notifications.length + 1}`;
        notifications.push({ kind: "notify", id, message, type });
        return id;
      },
    );
    const updateNotification = vi.fn(
      (
        id: string,
        message: string,
        type?: "info" | "success" | "warning" | "error",
      ) => {
        notifications.push({ kind: "update", id, message, type });
      },
    );

    const result = await submitMeetingStartRequest({
      agenda: "안건",
      channelId: "meeting",
      primaryProvider: "claude",
      reviewerProvider: "qwen",
      fixedParticipants: ["ch-td"],
      startMeeting: vi.fn(async () => ({
        ok: true,
        message: "회의가 큐에 등록되었습니다",
      })),
      notify,
      updateNotification,
      t,
    });

    expect(result).toEqual({ ok: true, message: "회의가 큐에 등록되었습니다" });
    expect(notify).toHaveBeenCalledTimes(1);
    expect(notifications[0]).toEqual({
      kind: "notify",
      id: "n-1",
      message: "회의 시작 요청이 접수되었습니다",
      type: "info",
    });
    expect(updateNotification).toHaveBeenCalledWith(
      "n-1",
      "회의가 큐에 등록되었습니다",
      "success",
    );
  });

  it("reuses the same notification lifecycle on failure without emitting a separate success toast", async () => {
    const notify = vi.fn(() => "n-1");
    const updateNotification = vi.fn();

    await expect(
      submitMeetingStartRequest({
        agenda: "안건",
        channelId: "meeting",
        primaryProvider: "claude",
        reviewerProvider: "qwen",
        fixedParticipants: [],
        startMeeting: vi.fn(async () => {
          throw new Error("start failed");
        }),
        notify,
        updateNotification,
        t,
      }),
    ).rejects.toThrow("start failed");

    expect(notify).toHaveBeenCalledTimes(1);
    expect(updateNotification).toHaveBeenCalledTimes(1);
    expect(updateNotification).toHaveBeenCalledWith(
      "n-1",
      "start failed",
      "error",
    );
  });
});

describe("openMeetingDetailWithFallback", () => {
  it("requests the clicked meeting id and falls back to the cached meeting on fetch failure", async () => {
    const fallbackMeeting = meeting({
      id: "meeting-clicked",
      summary: "fallback",
    });
    const fetchMeeting = vi.fn(async (_id: string) => {
      throw new Error("boom");
    });
    const logError = vi.fn();

    const result = await openMeetingDetailWithFallback(
      fallbackMeeting,
      fetchMeeting,
      logError,
    );

    expect(fetchMeeting).toHaveBeenCalledWith("meeting-clicked");
    expect(result).toBe(fallbackMeeting);
    expect(logError).toHaveBeenCalledTimes(1);
    expect(logError.mock.calls[0]?.[0]).toContain("meeting-clicked");
  });
});

describe("getMeetingReferenceHashes", () => {
  it("returns stable meeting and thread hashes in display order", () => {
    expect(
      getMeetingReferenceHashes(
        meeting({
          meeting_hash: "#meeting-123abc",
          thread_hash: "#thread-456def",
        }),
      ),
    ).toEqual(["#123abc", "#456def"]);
  });

  it("filters out missing hashes without changing the remaining stable hash", () => {
    expect(
      getMeetingReferenceHashes(
        meeting({
          meeting_hash: "#meeting-123abc",
          thread_hash: null,
        }),
      ),
    ).toEqual(["#123abc"]);
  });
});
