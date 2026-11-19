import type { CampaignNodeStatus } from "../../api/campaigns";
import { SERVER_CAMPAIGN_STAGES, type ServerCampaignDocument } from "../../api/serverCampaigns";
import type { TaskView } from "./CampaignOverview";
import { LABELS, type Tr } from "./campaignPresentation";
import { STAGE_LABELS } from "./serverCampaignModel";

type Label = [string, string];
/** One card: the task, its one-line status and whether the server's retry command applies to it. */
export interface BoardCard { task: TaskView; line: string | null; retry: boolean }
export interface BoardColumn { key: string; label: Label; done: boolean; cards: BoardCard[] }

// A ledger keeps only a status per task; its free-text stage stays a sentence inside the card.
const LEDGER_COLUMNS: CampaignNodeStatus[] = ["pending", "running", "blocked", "failed", "completed", "skipped"];
// The ledger schema allows these two, but they get a column only when a task holds them.
const LEDGER_OPTIONAL = new Set<string>(["blocked", "failed"]);
const LEDGER_DONE = new Set<string>(["completed", "skipped"]);

const time = (task: TaskView) => Date.parse(task.updatedAt ?? "") || 0;
const rank = (task: TaskView) => task.gate ? 0 : task.status === "failed" ? 1 : task.attention ? 2 : task.status === "running" ? 3 : 4;
/** Waiting approvals, failures and blocks lead; then the latest change. Ties keep the read order. */
const byUrgency = (a: BoardCard, b: BoardCard) => rank(a.task) - rank(b.task) || time(b.task) - time(a.task);

export function legacyBoard(tasks: TaskView[]): BoardColumn[] {
  return LEDGER_COLUMNS.map((status) => ({
    key: status, label: LABELS[status], done: LEDGER_DONE.has(status),
    cards: tasks.filter((task) => task.status === status).map((task) => ({ task, line: task.stage || null, retry: false })).sort(byUrgency),
  })).filter((column) => column.cards.length > 0 || !LEDGER_OPTIONAL.has(column.key));
}

/** Columns are the server's stages in pipeline order; a task the server has not placed yet waits in its own column. */
export function serverBoard({ view }: ServerCampaignDocument, tasks: TaskView[], tr: Tr): BoardColumn[] {
  const runtimes = new Map(view.nodes.map((node) => [node.id, node.runtime]));
  const retryOffered = view.commands.some((entry) => entry.command === "prepare_retry");
  const card = (task: TaskView): BoardCard => {
    const runtime = runtimes.get(task.id);
    // Every node starts at series 1, so an untouched one shows no try count.
    const tried = runtime && (runtime.disposition !== "pending" || runtime.series > 1);
    const line = runtime && [tried && tr(`${runtime.series}번째 시도`, `Try ${runtime.series}`),
      runtime.repair_cycles > 0 && tr(`수리 ${runtime.repair_cycles}회`, `${runtime.repair_cycles} repairs`)].filter(Boolean).join(" · ");
    return { task, line: line || null, retry: retryOffered && task.status === "failed" };
  };
  const columns: BoardColumn[] = SERVER_CAMPAIGN_STAGES.map((stage) => ({
    key: stage, label: STAGE_LABELS[stage], done: stage === "complete",
    cards: tasks.filter((task) => runtimes.get(task.id)?.stage === stage).map(card).sort(byUrgency),
  }));
  const unplaced = tasks.filter((task) => !runtimes.has(task.id));
  return unplaced.length ? [{ key: "unplaced", label: ["서버 상태 없음", "No server state"], done: false, cards: unplaced.map(card) }, ...columns] : columns;
}
