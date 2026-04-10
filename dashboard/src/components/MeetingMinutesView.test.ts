import { describe, expect, it } from "vitest";
import type { RoundTableMeetingChannelOption, RoundTableMeetingExpertOption } from "../types";
import { pruneFixedParticipantRoleIdsForLoadedChannel } from "./MeetingMinutesView";

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

describe("pruneFixedParticipantRoleIdsForLoadedChannel", () => {
  it("keeps stored fixed participants while meeting channels are loading", () => {
    const previous = ["td", "pd"];

    const next = pruneFixedParticipantRoleIdsForLoadedChannel(previous, true, null);

    expect(next).toBe(previous);
    expect(next).toEqual(["td", "pd"]);
  });

  it("keeps stored fixed participants until a selected channel exists", () => {
    const previous = ["td", "pd"];

    const next = pruneFixedParticipantRoleIdsForLoadedChannel(previous, false, null);

    expect(next).toBe(previous);
    expect(next).toEqual(["td", "pd"]);
  });

  it("prunes unavailable fixed participants only after a selected channel is loaded", () => {
    const previous = ["td", "unknown", "pd"];

    const next = pruneFixedParticipantRoleIdsForLoadedChannel(previous, false, channel(["td", "pd"]));

    expect(next).toEqual(["td", "pd"]);
  });
});
