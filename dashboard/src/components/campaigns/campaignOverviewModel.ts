import { z } from "zod";
import type { Campaign, CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import type { ServerCampaignDocument, ServerCampaignEvent, ServerCampaignSummary } from "../../api/serverCampaigns";
import type { Agent } from "../../types";
import type { DotTone, TrackSegment } from "../common/KitPrimitives";
import { CAMPAIGN_STAGES, campaignProgress, campaignStageStep, campaignTaskNumber, campaignTaskTitle, liveIsWorking, safeCampaignLink } from "./campaignModel";
import { LABELS, type Tr } from "./campaignPresentation";
import { DISPLAY_LABELS, DISPOSITION_LABELS, REASON_LABELS, STAGE_LABELS, nodeReasons } from "./serverCampaignModel";
import { nodeMarker, type MarkerTone } from "./StatusMarker";

type Label = [string, string];
export type CampaignBucket = "running" | "ready" | "done";
export const BUCKET_LABELS: Record<CampaignBucket, Label> = { running: ["진행", "Running"], ready: ["준비", "Ready"], done: ["끝남", "Done"] };

// Ledger statuses by list tab; the chip keeps the ledger's own word ("멈춤" for paused).
const LEDGER_BUCKETS: Record<Campaign["status"], CampaignBucket> = { active: "running", planned: "ready", paused: "ready", completed: "done", cancelled: "done" };
export const LEDGER_STATUS_LABELS: Record<Campaign["status"], Label> = {
  active: ["진행 중", "Running"], planned: ["계획됨", "Planned"], paused: ["멈춤", "Paused"], completed: ["끝남", "Done"], cancelled: ["취소됨", "Cancelled"],
};
type Display = ServerCampaignSummary["view"]["display"];
export function serverBucket(display: Display): CampaignBucket {
  return display === "succeeded" ? "done" : display === "prepared" || display === "stopped" ? "ready" : "running";
}

/** One part of a campaign's split bar: a ledger status or a server stage, with its task count and colour. */
export interface SplitPart { key: string; label: Label; count: number; tone: TrackSegment | "skip" }

/** One card in the campaign list, for a ledger or a server campaign. `split` is null until a server campaign's tasks are read. */
export interface CampaignEntry {
  key: string; kind: "legacy" | "server"; id: string; title: string; description: string; repo: string | null;
  bucket: CampaignBucket; status: string; statusLabel: string; done: number | null; total: number | null; attention: string | null;
  split: SplitPart[] | null;
}
export const entryKey = (kind: CampaignEntry["kind"], id: string) => `${kind}:${id}`;

// A ledger's bar: finished work first, then what is moving, what needs someone and what waits.
const LEDGER_SPLIT: [CampaignNodeStatus, SplitPart["tone"]][] = [["completed", "done"], ["running", "now"], ["blocked", "wait"], ["failed", "fail"], ["pending", "todo"], ["skipped", "skip"]];

/** Counts with the campaign's live map, the same tasks its overview shows. */
export function legacyEntry(campaign: Campaign, live: Record<string, CampaignNodeLive> | undefined, tr: Tr): CampaignEntry {
  const { total } = campaignProgress(campaign.nodes);
  const tasks = campaign.nodes.map((node) => legacyTask(node, live?.[node.id]));
  const counts = taskCounts(tasks);
  return {
    key: entryKey("legacy", campaign.id), kind: "legacy", id: campaign.id, title: campaign.title, description: campaign.description, repo: null,
    bucket: LEDGER_BUCKETS[campaign.status], status: campaign.status, statusLabel: tr(...LEDGER_STATUS_LABELS[campaign.status]),
    done: counts.done, total, attention: attentionText(counts.blocked, counts.failed, tr),
    split: LEDGER_SPLIT.map(([status, tone]) => ({ key: status, label: LABELS[status], count: tasks.filter((task) => task.status === status).length, tone })).filter((part) => part.count > 0),
  };
}

type Disposition = keyof typeof DISPOSITION_LABELS;
const PAUSED_DISPLAYS = new Set<Display>(["waiting", "stopping", "stopped", "needs_inspection"]);
/** A stage's colour is its most pressing task's: failed, held, moving (waiting while the campaign is held), not started. */
function stageTone(stage: string, dispositions: Disposition[], display: Display): SplitPart["tone"] {
  if (stage === "complete") return "done";
  if (dispositions.includes("failed")) return "fail";
  if (dispositions.some((value) => value === "blocked" || value === "interrupted")) return "wait";
  if (dispositions.some((value) => value === "assigned" || value === "running" || value === "verifying")) return PAUSED_DISPLAYS.has(display) ? "wait" : "now";
  return "todo";
}

/** A summary carries no node list; the full document, once read, adds the task counts and the bar split by stage. */
export function serverEntry(summary: ServerCampaignSummary, doc: ServerCampaignDocument | null, tr: Tr): CampaignEntry {
  const { definition, view } = summary;
  const nodes = doc?.view.nodes ?? [];
  const done = doc ? nodes.filter((node) => node.runtime.disposition === "complete").length : null;
  const split = SERVER_STAGES.flatMap((stage): SplitPart[] => {
    const here = nodes.filter((node) => node.runtime.stage === stage).map((node) => node.runtime.disposition);
    return here.length ? [{ key: stage, label: STAGE_LABELS[stage as keyof typeof STAGE_LABELS], count: here.length, tone: stageTone(stage, here, view.display) }] : [];
  });
  return {
    key: entryKey("server", view.campaign_id), kind: "server", id: view.campaign_id, title: definition.title, description: definition.goal,
    repo: definition.repository, bucket: serverBucket(view.display), status: view.display, statusLabel: tr(...DISPLAY_LABELS[view.display]),
    done, total: doc ? doc.definition.nodes.length : null, attention: attentionText(view.blocked_count, view.failed_count, tr), split: doc ? split : null,
  };
}

function attentionText(blocked: number, failed: number, tr: Tr): string | null {
  const parts = [failed && tr(`실패 ${failed}`, `${failed} failed`), blocked && tr(`막힘 ${blocked}`, `${blocked} blocked`)].filter(Boolean);
  return parts.length ? parts.join(" · ") : null;
}

const GATES = ["start", "implementation", "review", "landing", "merge", "deploy"] as const;
export const GATE_LABELS: Record<(typeof GATES)[number], Label> = {
  start: ["착수", "Start"], implementation: ["구현", "Implementation"], review: ["리뷰", "Review"], landing: ["착지", "Landing"], merge: ["머지", "Merge"], deploy: ["배포", "Deploy"],
};
const gateEventSchema = z.looseObject({
  event: z.enum(["approval_gate_opened", "approval_gate_approved"]),
  binding: z.looseObject({ run_id: z.string(), node_id: z.string(), gate: z.enum(GATES) }),
});
export interface PendingGate { gate: (typeof GATES)[number]; openedAt: string }

/** Gates the shown run opened and has not approved, by node. Only the read events count, so an older opening is missed. */
export function openGates(events: ServerCampaignEvent[], runId: string | null): Map<string, PendingGate> {
  const open = new Map<string, PendingGate & { node: string }>();
  for (const event of events) {
    const kind = gateEventSchema.safeParse(event.kind).data;
    if (!kind || kind.binding.run_id !== runId) continue;
    const { node_id: node, gate } = kind.binding;
    if (kind.event === "approval_gate_approved") open.delete(`${node}\n${gate}`);
    else open.set(`${node}\n${gate}`, { node, gate, openedAt: event.observed_at });
  }
  const byNode = new Map<string, PendingGate>();
  for (const { node, gate, openedAt } of open.values()) if (!byNode.has(node)) byNode.set(node, { gate, openedAt });
  return byNode;
}

const PLACEHOLDER_BLOCKERS = /^(none|없음|n\/a|-)$/i;
/** A blank or placeholder blocker ("none", "없음", "n/a", "-") means nothing blocks the task. */
export function cleanBlocker(text: string | null | undefined): string | null {
  const value = text?.trim();
  return value && !PLACEHOLDER_BLOCKERS.test(value) ? value : null;
}
// A pending task's blocker is a precondition, not a block; it shows only as the row's reason.
const needsSomeone = (status: CampaignNodeStatus, blocker: string | null) => status === "blocked" || status === "failed" || (status === "running" && !!blocker);

/** A task as the overview shows it, read from a ledger node or a server node. Only fields the server gave are set.
 * `progress` is a server task's short state line and `sessionNow` its session's; a ledger's stage is a coordinator's note
 * the detail keeps, and its session comes from the live card read. */
export interface OverviewTask {
  id: string; title: string; number: string | null; status: CampaignNodeStatus; marker: { tone: MarkerTone; ratio: number };
  stage: string; step: Label | null; summary: string | null; benefit: string | null; blocker: string | null; group: string | null;
  updatedAt: string | null; session: string | null; agent: string | null; prUrl: string | null; attention: boolean; gate: PendingGate | null;
  progress: string | null; sessionNow: SessionNow | null;
}
/** A session as a card's foot shows it: a dot, the state words and when it was last seen. */
export interface SessionNow { tone: DotTone; text: string; seenAt: string | null }

/** Live work makes an open node running. */
export function legacyTask(node: CampaignNode, live?: CampaignNodeLive): OverviewTask {
  const status = node.status !== "completed" && node.status !== "skipped" && liveIsWorking(live) ? "running" : node.status;
  const blocker = cleanBlocker(node.blocker);
  const step = campaignStageStep(node.stage).index;
  const title = campaignTaskTitle(node);
  return {
    id: node.id, title, number: campaignTaskNumber(node), status, marker: nodeMarker(status, node.stage),
    stage: node.stage.trim(), step: step < 0 ? null : CAMPAIGN_STAGES[step].label,
    summary: node.summary?.trim() && node.summary.trim() !== title ? node.summary.trim() : null, benefit: node.benefit?.trim() || null, blocker, group: node.group,
    updatedAt: node.updated_at, session: node.session_id, agent: node.assignee, prUrl: safeCampaignLink(node.pr_url) ?? null,
    attention: needsSomeone(status, blocker), gate: null, progress: null, sessionNow: null,
  };
}

const DISPOSITION_STATUS: Record<keyof typeof DISPOSITION_LABELS, CampaignNodeStatus> = {
  pending: "pending", assigned: "running", running: "running", verifying: "running", failed: "failed", blocked: "blocked", interrupted: "pending", complete: "completed",
};
const SESSION_STATES: Record<string, Label> = { alive: ["세션 살아 있음", "session alive"], gone: ["세션 끝남", "session gone"], unknown: ["세션 확인 불가", "session unknown"] };
const SESSION_TONES: Record<string, DotTone> = { alive: "ok", gone: "neutral", unknown: "warn" };
const SERVER_STAGES = Object.keys(STAGE_LABELS);

export function serverTasks({ definition, view }: ServerCampaignDocument, events: ServerCampaignEvent[], tr: Tr): OverviewTask[] {
  const nodes = new Map(view.nodes.map((node) => [node.id, node]));
  const gates = openGates(events, view.run_id);
  return definition.nodes.map((node) => {
    const nodeView = nodes.get(node.id);
    const runtime = nodeView?.runtime;
    const status = runtime ? DISPOSITION_STATUS[runtime.disposition] : "pending";
    const reasons = nodeView ? nodeReasons(nodeView, view.reasons) : [];
    const session = runtime?.current_attempt ? view.sessions.find((value) => value.attempt_id === runtime.current_attempt) : undefined;
    const blocker = reasons.length ? reasons.map((code) => tr(...REASON_LABELS[code])).join(" · ") : null;
    const issue = safeCampaignLink(node.issue_url) ? new URL(node.issue_url).pathname.match(/\/issues\/(\d+)\/?$/)?.[1] : undefined;
    const stage = runtime ? [tr(...DISPOSITION_LABELS[runtime.disposition]), tr(...STAGE_LABELS[runtime.stage]), tr(`계열 ${runtime.series}`, `series ${runtime.series}`)].join(" · ") : tr("서버 상태 없음", "No server state");
    const sessionText = session ? `${tr(...SESSION_STATES[session.observation.state])} · ${session.identity.gate.host}` : null;
    const observation = session?.observation;
    return {
      id: node.id, title: node.title, number: issue ? `#${issue}` : null, status,
      marker: !runtime ? { tone: "plan", ratio: 0 } : status === "running" ? { tone: "progress", ratio: (SERVER_STAGES.indexOf(runtime.stage) + 1) / (SERVER_STAGES.length + 1) } : nodeMarker(status),
      stage, step: runtime ? STAGE_LABELS[runtime.stage] : null, summary: null, benefit: null,
      blocker, group: null,
      updatedAt: null, session: sessionText,
      agent: null, prUrl: null, attention: needsSomeone(status, blocker), gate: gates.get(node.id) ?? null, progress: runtime ? stage : null,
      sessionNow: observation && sessionText ? { tone: SESSION_TONES[observation.state], text: sessionText, seenAt: observation.state === "unknown" ? observation.last_observed_at : observation.observed_at } : null,
    };
  });
}

/** The assignee's agent by id, name or alias; no match means no icon. */
export function findAgent(roster: Agent[] | null, name: string | null): Agent | undefined {
  const key = name?.trim().toLowerCase();
  return key ? roster?.find((agent) => [agent.id, agent.name, agent.alias, agent.name_ko].some((value) => value?.toLowerCase() === key)) : undefined;
}

/** 작업 찾기: every task whose id, number, title, summary, stage, group, assignee or session holds the query. */
export function findTasks<T extends OverviewTask>(tasks: T[], query: string): T[] {
  const key = query.trim().toLocaleLowerCase();
  return key ? tasks.filter((task) => [task.id, task.number, task.title, task.summary, task.stage, task.group, task.agent, task.session].some((value) => value?.toLocaleLowerCase().includes(key))) : [];
}

export function taskCounts(tasks: OverviewTask[]) {
  const count = (test: (task: OverviewTask) => boolean) => tasks.filter(test).length;
  return {
    running: count((task) => task.status === "running"), approvals: count((task) => !!task.gate), failed: count((task) => task.status === "failed"),
    blocked: count((task) => task.attention && !task.gate && task.status !== "failed"), done: count((task) => task.status === "completed"),
    withPr: count((task) => task.status === "running" && !!task.prUrl),
  };
}

/** "진행 중 · 2개 진행, 1개 막힘": the campaign's status word, then what its tasks are doing now. */
export function statusSentence(statusLabel: string, tasks: OverviewTask[], tr: Tr): string {
  const { running, approvals, blocked, failed } = taskCounts(tasks);
  const parts = [
    running && tr(`${running}개 진행`, `${running} running`), approvals && tr(`${approvals}개 승인 대기`, `${approvals} awaiting approval`),
    blocked && tr(`${blocked}개 막힘`, `${blocked} blocked`), failed && tr(`${failed}개 실패`, `${failed} failed`),
  ].filter(Boolean);
  return `${statusLabel} · ${parts.length ? parts.join(", ") : tr("진행 중인 작업 없음", "nothing running")}`;
}

export interface RecentEntry { key: string; at: string; text: string; status: CampaignNodeStatus | null }
const time = (value: string) => Date.parse(value) || 0;

/** The ledger keeps no event log in the campaign read; each task's last write stands for its latest change. */
export function recentTaskChanges(tasks: OverviewTask[], limit = 5): RecentEntry[] {
  return tasks.filter((task) => task.updatedAt).sort((a, b) => time(b.updatedAt!) - time(a.updatedAt!)).slice(0, limit)
    .map((task) => ({ key: task.id, at: task.updatedAt!, text: `${task.title} · ${task.stage}`, status: task.status }));
}

export function recentServerEvents(events: ServerCampaignEvent[], limit = 5): RecentEntry[] {
  return events.slice(-limit).reverse().map((event) => ({ key: String(event.sequence), at: event.observed_at, text: event.kind.event, status: null }));
}
