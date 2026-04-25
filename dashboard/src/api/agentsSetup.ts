/**
 * Thin wrappers around /api/agents/setup that expose dry_run + execute as
 * distinct methods, matching the semantics of the dashboard Setup Wizard.
 *
 * This module intentionally delegates to the existing client.ts functions
 * (`setupAgent`, `duplicateAgent`) so there is no duplicated request plumbing.
 * It provides:
 *
 *   dryRunSetupAgent(body)       - forces `dry_run: true`
 *   executeSetupAgent(body)      - forces `dry_run: false`
 *   dryRunDuplicateAgent(...)    - forces `dry_run: true`
 *   executeDuplicateAgent(...)   - forces `dry_run: false`
 *
 * These helpers return the same `AgentSetupResponse` shape as the underlying
 * endpoint, so callers can inspect `steps`, `errors`, `warnings`, and
 * `rollback` uniformly.
 */

import type { Agent } from "../types";
import {
  duplicateAgent,
  setupAgent,
  type AgentSetupRequest,
  type AgentSetupResponse,
  type DuplicateAgentRequest,
} from "./client";

export type { AgentSetupRequest, AgentSetupResponse, DuplicateAgentRequest };

export async function dryRunSetupAgent(
  body: Omit<AgentSetupRequest, "dry_run">,
): Promise<AgentSetupResponse> {
  return setupAgent({ ...body, dry_run: true });
}

export async function executeSetupAgent(
  body: Omit<AgentSetupRequest, "dry_run">,
): Promise<AgentSetupResponse> {
  return setupAgent({ ...body, dry_run: false });
}

export async function dryRunDuplicateAgent(
  sourceAgentId: string,
  body: Omit<DuplicateAgentRequest, "dry_run">,
): Promise<AgentSetupResponse & { agent?: Agent }> {
  return duplicateAgent(sourceAgentId, { ...body, dry_run: true });
}

export async function executeDuplicateAgent(
  sourceAgentId: string,
  body: Omit<DuplicateAgentRequest, "dry_run">,
): Promise<AgentSetupResponse & { agent?: Agent }> {
  return duplicateAgent(sourceAgentId, { ...body, dry_run: false });
}

/**
 * Best-effort summariser for the rollback payload returned by the backend when
 * setup fails partway. The exact shape is not strongly typed on the server
 * side, so we render whatever we can get.
 */
export interface RollbackSummary {
  attempted: string[];
  reverted: string[];
  failed: string[];
  raw: unknown;
}

export function summarizeRollback(rollback: unknown): RollbackSummary {
  const fallback: RollbackSummary = {
    attempted: [],
    reverted: [],
    failed: [],
    raw: rollback ?? null,
  };
  if (!rollback || typeof rollback !== "object") return fallback;
  const obj = rollback as Record<string, unknown>;

  const toArray = (value: unknown): string[] => {
    if (!Array.isArray(value)) return [];
    return value
      .map((item) => {
        if (typeof item === "string") return item;
        if (item && typeof item === "object") {
          const rec = item as Record<string, unknown>;
          const name = rec.name ?? rec.step ?? rec.action;
          const detail = rec.detail ?? rec.message;
          if (typeof name === "string" && typeof detail === "string") {
            return `${name}: ${detail}`;
          }
          if (typeof name === "string") return name;
          return JSON.stringify(rec);
        }
        return String(item);
      })
      .filter(Boolean);
  };

  return {
    attempted: toArray(obj.attempted ?? obj.steps),
    reverted: toArray(obj.reverted ?? obj.rolled_back ?? obj.undone),
    failed: toArray(obj.failed ?? obj.errors),
    raw: rollback,
  };
}
