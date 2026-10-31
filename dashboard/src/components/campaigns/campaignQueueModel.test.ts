import { expect, it } from "vitest";
import type { Campaign } from "../../api/campaigns";
import type { KanbanCard } from "../../types";
import { cardTitle } from "../agent-manager/kanban-utils";
import { campaignTaskNumber, campaignTaskTitle } from "./campaignModel";
import {
  EMPTY_QUEUE_FILTERS, campaignNodeFromCard, filterQueueCards, parseQueueLocation, queueCards, queueLocationQuery,
} from "./campaignQueueModel";
import { makeCampaignNode } from "./campaignTestFixtures";

function card(id: string, patch: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id, title: id, description: null, status: "backlog", github_repo: "itismyfield/AgentDesk", owner_agent_id: null, requester_agent_id: null,
    assignee_agent_id: null, parent_card_id: null, latest_dispatch_id: null, sort_order: 0, priority: "medium", depth: 0, blocked_reason: null,
    review_notes: null, github_issue_number: null, github_issue_url: null, metadata_json: null, pipeline_stage_id: null, review_status: null,
    created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z", started_at: null, requested_at: null, completed_at: null, ...patch,
  };
}
function campaign(id: string, nodes: Campaign["nodes"], status: Campaign["status"] = "active"): Campaign {
  return { id, title: id, description: "", status, round: 2, revision: 3, auto_queue: false, nodes, created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z" };
}

it("queues open cards no open task holds, matching tasks by issue link, title number or id", () => {
  const cards = [
    card("linked", { github_issue_number: 10 }),
    card("titled", { github_issue_number: 11 }),
    card("by-id", { github_issue_number: 12 }),
    card("other-repo", { github_issue_number: 10, github_repo: "itismyfield/CookingHeart" }),
    card("closed-task", { github_issue_number: 13 }),
    card("closed-campaign", { github_issue_number: 14 }),
    card("done", { github_issue_number: 15, status: "done" }),
    card("no-issue"),
  ];
  const campaigns = [
    campaign("open", [
      makeCampaignNode("a", { issue_url: "https://github.com/itismyfield/agentdesk/issues/10" }),
      makeCampaignNode("b", { title: "#11 fix the relay" }),
      makeCampaignNode("i12_part", { title: "part of twelve" }),
      makeCampaignNode("c", { issue_url: "https://github.com/itismyfield/AgentDesk/issues/13", status: "completed" }),
    ]),
    campaign("finished", [makeCampaignNode("d", { issue_url: "https://github.com/itismyfield/AgentDesk/issues/14" })], "completed"),
  ];
  expect(queueCards(cards, campaigns).map((value) => value.id)).toEqual(["other-repo", "closed-task", "closed-campaign", "no-issue"]);
});

it("sorts by priority then most recent change, and filters by text, status and repository", () => {
  const cards = [
    card("old-low", { priority: "low", updated_at: "2026-10-03T00:00:00Z" }),
    card("urgent", { priority: "urgent", updated_at: "2026-09-01T00:00:00Z", github_issue_number: 7 }),
    card("medium-new", { updated_at: "2026-10-05T00:00:00Z", status: "ready" }),
    card("medium-old", { updated_at: "2026-10-02T00:00:00Z", github_repo: "itismyfield/CookingHeart" }),
  ];
  expect(filterQueueCards(cards, EMPTY_QUEUE_FILTERS).map((value) => value.id)).toEqual(["urgent", "medium-new", "medium-old", "old-low"]);
  expect(filterQueueCards(cards, { ...EMPTY_QUEUE_FILTERS, sort: "recent" })[0].id).toBe("medium-new");
  expect(filterQueueCards(cards, { ...EMPTY_QUEUE_FILTERS, query: "#7" }).map((value) => value.id)).toEqual(["urgent"]);
  expect(filterQueueCards(cards, { ...EMPTY_QUEUE_FILTERS, status: "ready" }).map((value) => value.id)).toEqual(["medium-new"]);
  expect(filterQueueCards(cards, { ...EMPTY_QUEUE_FILTERS, repo: "itismyfield/CookingHeart" }).map((value) => value.id)).toEqual(["medium-old"]);
});

it("adds a card as a pending task numbered like the campaign's own tasks", () => {
  const target = campaign("open", [makeCampaignNode("i42")]);
  const node = campaignNodeFromCard({ ...card("c", { title: "Relay\\nfix", assignee_agent_id: "agent-1" }), github_issue_number: 42 }, target,
    "https://github.com/itismyfield/AgentDesk/issues/42", "Added from the queue", "2026-10-31T00:00:00Z");
  expect(node).toMatchObject({ id: "i42_2", title: "#42 Relay fix", status: "pending", round: 2, assignee: "agent-1", dependencies: [], stage: "Added from the queue" });
  expect(campaignTaskNumber(node)).toBe("#42");
  expect(campaignTaskTitle(node)).toBe("Relay fix");
});

it("shows a task number once with one prefix and keeps a title that merely starts with another number", () => {
  const linked = makeCampaignNode("i6335", { title: "#6335 relay catch-up", issue_url: "https://github.com/o/r/issues/6335" });
  const byId = makeCampaignNode("i7336", { title: "#7336 failover" });
  const differs = makeCampaignNode("x", { title: "#6325 follow-up", issue_url: "https://github.com/o/r/issues/6335" });
  const bare = makeCampaignNode("cli", { title: "Update CLIs" });
  expect([linked, byId, differs, bare].map(campaignTaskNumber)).toEqual(["#6335", "#7336", "#6335", null]);
  expect([linked, byId, differs, bare].map(campaignTaskTitle)).toEqual(["relay catch-up", "failover", "#6325 follow-up", "Update CLIs"]);
});

it("reads one line from titles with newline characters or a literal backslash-n", () => {
  expect(cardTitle("first\\nsecond")).toBe("first second");
  expect(cardTitle("first\n\nsecond\t ")).toBe("first second");
});

it("round-trips the queue deep link", () => {
  expect(queueLocationQuery(null)).toBe("?tab=queue");
  expect(parseQueueLocation(queueLocationQuery("card-1"))).toEqual({ card: "card-1" });
  expect(parseQueueLocation("?mode=legacy&campaign=x")).toBeNull();
});
