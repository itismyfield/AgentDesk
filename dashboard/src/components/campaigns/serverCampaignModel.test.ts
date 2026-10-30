import { describe, expect, it } from "vitest";
import { serverCampaignFixtures as fixtures } from "../../api/serverCampaignsFixtures";
import type { ServerCampaignView } from "../../api/serverCampaigns";
import {
  DISPLAY_LABELS, campaignLocationQuery, changedDefinitionFields, commandAvailability, displayText,
  feedbackText, nodeReasons, parseCampaignLocation, reconcileStop, stopPhase, stopRequest, unknownObservations,
  type StopIntent,
} from "./serverCampaignModel";

const tr = (_ko: string, en: string) => en;
const view = (doc = fixtures.prepared) => doc.view;
const node = (doc: typeof fixtures.prepared, id: string) => doc.view.nodes.find((entry) => entry.id === id)!;

describe("server display", () => {
  it("labels every server display state and never reads inspection or unknown as success", () => {
    const states: ServerCampaignView["display"][] = [
      "stopping", "stopped", "prepared", "succeeded", "running", "waiting", "unable_to_progress", "needs_inspection",
    ];
    for (const state of states) expect(DISPLAY_LABELS[state]).toBeDefined();
    expect(displayText({ ...view(), display: "needs_inspection" }, tr)).toBe("Needs inspection");
    expect(displayText(view(fixtures.stoppingUnknown), tr)).toBe("Stopping · finish current stage");
    expect(unknownObservations(view(fixtures.stoppingUnknown))).toEqual([
      { host: "host-a", session: "worker-A", reason: "Host unreachable", lastObservedAt: null },
    ]);
  });

  it("shows failures beside a running campaign instead of hiding them", () => {
    expect(displayText(view(fixtures.failedRunning), tr)).toBe("Running · 1 failed · 1 blocked");
    expect(displayText(view(fixtures.failedBlockedComplete), tr)).toBe("Unable to progress · 1 failed · 1 blocked");
  });

  it("lists a node's own wait reason and every campaign reason addressed to it", () => {
    expect(nodeReasons(node(fixtures.prepared, "B"), view().reasons)).toEqual(["predecessor_main"]);
    expect(nodeReasons(node(fixtures.reviewWaiting, "A"), view(fixtures.reviewWaiting).reasons)).toEqual(["review_resources"]);
    expect(nodeReasons(node(fixtures.failedRunning, "B"), view(fixtures.failedRunning).reasons)).toEqual(["predecessor_failed"]);
    expect(nodeReasons(node(fixtures.prepared, "C"), view().reasons)).toEqual([]);
  });

  it("reads the stop phase from the view only", () => {
    expect(stopPhase(view(fixtures.failedRunning))).toBeNull();
    expect(stopPhase(view(fixtures.stoppingUnknown))).toBe("accepted");
    expect(stopPhase({ ...view(fixtures.stoppingUnknown), stop_confirmed: true })).toBe("confirmed");
    expect(stopPhase(view(fixtures.stoppedGone))).toBe("stopped");
  });
});

describe("controls", () => {
  it("enables only commands the server offers without a refusal", () => {
    expect(commandAvailability(view(), "edit_definition", "live")).toEqual({ enabled: true, reason: null });
    expect(commandAvailability(view(), "start", "live")).toEqual({ enabled: false, reason: "execution_unavailable" });
    expect(commandAvailability(view(), "stop_immediate", "live")).toEqual({ enabled: false, reason: "invalid_state" });
  });

  it("keeps stop available from a stale snapshot but not start, edit, resume or retry", () => {
    const running = view(fixtures.failedRunning);
    expect(commandAvailability(running, "stop_immediate", "stale")).toEqual({ enabled: true, reason: null });
    expect(commandAvailability(view(), "edit_definition", "stale")).toEqual({ enabled: false, reason: "disconnected" });
    const offered = { ...running, commands: running.commands.map((entry) => ({ ...entry, rejection: null })) };
    for (const command of ["start", "edit_definition", "resume", "prepare_retry"] as const) {
      expect(commandAvailability(offered, command, "stale")).toEqual({ enabled: false, reason: "disconnected" });
    }
  });

  it("targets a stop at the run and definition only and resends only an unanswered one under its key", () => {
    const first = stopRequest(null, view(fixtures.stoppingUnknown), "finish_stage", () => "key-1");
    expect(first.command).toEqual({ command: "stop", expected_definition_revision: 7, expected_run: "run-1", mode: "finish_stage" });
    const unanswered: StopIntent = { ...first, status: "unconfirmed" };
    // Events moved on, the run and revision did not: still the same request.
    expect(stopRequest(unanswered, view(fixtures.eventAdvanced), "finish_stage", () => "key-2").key).toBe("key-1");
    expect(stopRequest(unanswered, view(fixtures.eventAdvanced), "immediate", () => "key-3").key).toBe("key-3");
    expect(stopRequest(first, view(fixtures.eventAdvanced), "finish_stage", () => "key-4").key).toBe("key-4");
  });

  it("settles an unanswered stop only by its own event or another run", () => {
    const sent = stopRequest(null, view(fixtures.failedRunning), "finish_stage", () => "k");
    const pending: StopIntent = { ...sent, status: "unconfirmed" };
    expect(reconcileStop(pending, view(fixtures.failedRunning))).toBe(pending);
    // Another client's stop also moves the control version; that is no proof this one arrived.
    expect(reconcileStop(pending, view(fixtures.stoppingUnknown))).toBe(pending);
    const other = { sequence: 29, run_id: "run-1", command_id: "someone-else", observed_at: "2026-10-28T05:00:01Z", kind: { event: "stop_requested" } };
    expect(reconcileStop(pending, view(fixtures.stoppingUnknown), [other])).toBe(pending);
    expect(reconcileStop(pending, { ...view(fixtures.failedRunning), run_id: "run-2" })).toBeNull();
    const event = { sequence: 30, run_id: "run-1", command_id: "k", observed_at: "2026-10-28T05:00:02Z", kind: { event: "stop_requested" } };
    expect(reconcileStop(pending, view(fixtures.failedRunning), [event])).toBeNull();
  });
});

describe("links and feedback", () => {
  it("round-trips mode, campaign and task through the URL query", () => {
    const query = campaignLocationQuery({ mode: "server", campaign: "release /a", node: "B" });
    expect(parseCampaignLocation(query)).toEqual({ mode: "server", campaign: "release /a", node: "B" });
    expect(parseCampaignLocation("?mode=legacy&campaign=x")).toEqual({ mode: "legacy", campaign: "x", node: null });
    expect(parseCampaignLocation("?mode=other&campaign=x")).toBeNull();
    expect(parseCampaignLocation("")).toBeNull();
  });

  it("copies only the link, as_of, definition revision, event number and display state", () => {
    const text = feedbackText("https://adk.example/campaigns?mode=server&campaign=release", view(fixtures.reviewWaiting), tr);
    expect(text.split("\n")).toEqual([
      "https://adk.example/campaigns?mode=server&campaign=release",
      "as_of 2026-10-28T05:00:00Z",
      "definition revision 7",
      "event 21",
      "display Waiting",
    ]);
  });

  it("lists the fields a newer definition changed by task and acceptance ID", () => {
    const base = fixtures.prepared.definition;
    const latest = structuredClone(base);
    latest.goal = "Changed elsewhere";
    latest.nodes[1].acceptance.push({ id: "extra", description: "More", verification: "test" });
    latest.nodes[0].acceptance[0].description = "Merged";
    latest.nodes.splice(2, 1);
    latest.nodes.push({ ...base.nodes[2], id: "D" });
    expect(changedDefinitionFields(base, latest)).toEqual([
      "goal", "A.acceptance.main", "B.acceptance.extra (added)", "C (removed)", "D (added)",
    ]);
  });
});
