import type { CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import { SERVER_CAMPAIGN_STAGES, type ServerCampaignDocument } from "../../api/serverCampaigns";
import type { TaskView } from "./CampaignOverview";
import { legacyTask } from "./campaignOverviewModel";
import { LABELS, type Tr } from "./campaignPresentation";
import { DISPOSITION_LABELS, STAGE_LABELS } from "./serverCampaignModel";

type Label = [string, string];
type Runtime = ServerCampaignDocument["view"]["nodes"][number]["runtime"];
/** One card: the task, its one-line status, a server state word the task status would hide, and whether retry applies. */
export interface BoardCard { task: TaskView; line: string | null; chip?: Label; retry: boolean }
export interface BoardColumn { key: string; label: Label; done: boolean; cards: BoardCard[] }

// A ledger keeps only a status per task; its free-text stage stays a sentence inside the card.
const LEDGER_COLUMNS: CampaignNodeStatus[] = ["pending", "running", "blocked", "failed", "completed", "skipped"];
// The ledger schema allows these two, but they get a column only when a task holds them.
const LEDGER_OPTIONAL = new Set<string>(["blocked", "failed"]);
const LEDGER_DONE = new Set<string>(["completed", "skipped"]);

const time = (task: TaskView) => Date.parse(task.updatedAt ?? "") || 0;
const rank = (task: TaskView) => task.gate ? 0 : task.status === "failed" ? 1 : task.attention ? 2 : task.status === "running" ? 3 : 4;
/** Waiting approvals, failures and blocks lead; then the latest change. Ties keep the read order (server: definition order). */
const byUrgency = (a: BoardCard, b: BoardCard) => rank(a.task) - rank(b.task) || time(b.task) - time(a.task);

/** Columns and chips read the ledger's own status; work on the linked card only adds its live mark. */
export function legacyBoard(nodes: CampaignNode[], live?: Record<string, CampaignNodeLive>): BoardColumn[] {
  const tasks: TaskView[] = nodes.map((node) => ({ ...legacyTask(node), live: live?.[node.id] }));
  return LEDGER_COLUMNS.map((status) => ({
    key: status, label: LABELS[status], done: LEDGER_DONE.has(status),
    cards: tasks.filter((task) => task.status === status).map((task) => ({ task, line: task.stage || null, retry: false })).sort(byUrgency),
  })).filter((column) => column.cards.length > 0 || !LEDGER_OPTIONAL.has(column.key));
}

const WORKED = new Set<Runtime["disposition"]>(["assigned", "running", "verifying", "failed", "interrupted", "complete"]);
// Every node starts at series 1 in the first stage; one the server shows no work on yet carries no series.
const started = (runtime: Runtime) => WORKED.has(runtime.disposition) || runtime.current_attempt !== null
  || runtime.stage !== SERVER_CAMPAIGN_STAGES[0] || runtime.series > 1 || runtime.repair_cycles > 0;

/** Columns are the server's stages in pipeline order; a task the server has not placed yet waits in its own column. */
export function serverBoard({ view }: ServerCampaignDocument, tasks: TaskView[], tr: Tr): BoardColumn[] {
  const runtimes = new Map(view.nodes.map((node) => [node.id, node.runtime]));
  const retryOffered = view.commands.some((entry) => entry.command === "prepare_retry");
  const card = (task: TaskView): BoardCard => {
    const runtime = runtimes.get(task.id);
    // The same words as the task detail: series, repair cycle and the server's own word for a user stop.
    const line = runtime && [started(runtime) && tr(`계열 ${runtime.series}`, `series ${runtime.series}`),
      runtime.repair_cycles > 0 && tr(`수리 차수 ${runtime.repair_cycles}`, `repair cycle ${runtime.repair_cycles}`)].filter(Boolean).join(" · ");
    return { task, line: line || null, chip: runtime?.disposition === "interrupted" ? DISPOSITION_LABELS.interrupted : undefined, retry: retryOffered && task.status === "failed" };
  };
  const columns: BoardColumn[] = SERVER_CAMPAIGN_STAGES.map((stage) => ({
    key: stage, label: STAGE_LABELS[stage], done: stage === "complete",
    cards: tasks.filter((task) => runtimes.get(task.id)?.stage === stage).map(card).sort(byUrgency),
  }));
  const unplaced = tasks.filter((task) => !runtimes.has(task.id));
  return unplaced.length ? [{ key: "unplaced", label: ["서버 상태 없음", "No server state"], done: false, cards: unplaced.map(card) }, ...columns] : columns;
}
