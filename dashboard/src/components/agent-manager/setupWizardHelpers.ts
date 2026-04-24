/**
 * Pure helpers for the agent Setup Wizard.
 *
 * These utilities are kept in a standalone module so they can be unit tested
 * without pulling in React or the API client. The wizard component imports
 * them directly.
 *
 * Covers:
 *   - provider suffix auto-detection (channel name ends with -cc / -cdx / etc.)
 *   - per-step validation for the 6-step wizard
 *   - dry-run / confirm request body construction for /api/agents/setup
 *   - skills text parsing
 */

import type { CliProvider } from "../../types";

export type WizardStepId =
  | "role"
  | "discord"
  | "prompt"
  | "workspace"
  | "cron"
  | "preview";

export const WIZARD_STEPS: readonly WizardStepId[] = [
  "role",
  "discord",
  "prompt",
  "workspace",
  "cron",
  "preview",
] as const;

export interface WizardDraft {
  agentId: string;
  name: string;
  nameKo: string;
  departmentId: string;
  provider: CliProvider;
  channelId: string;
  promptTemplatePath: string;
  promptContent: string;
  skillsText: string;
  cronEnabled: boolean;
  cronSpec: string;
}

export interface ValidationResult {
  valid: boolean;
  errors: string[];
}

export interface ProviderSuffixDetection {
  provider: CliProvider | null;
  suffix: string | null;
  source: "channelName" | "agentId" | null;
}

/** Map of well-known suffixes to the provider they imply. */
export const PROVIDER_SUFFIX_MAP: Record<string, CliProvider> = {
  "-cc": "claude",
  "-cdx": "codex",
  "-gem": "gemini",
  "-qw": "qwen",
  "-oc": "opencode",
  "-cop": "copilot",
  "-ag": "antigravity",
  "-api": "api",
};

/**
 * Detects the CLI provider from a Discord channel name or agent_id suffix.
 *
 * Rules (case-insensitive):
 *   channel `adk-cc`      → claude
 *   channel `agentdesk-cdx` → codex
 *   channel `my-bot-gem`  → gemini
 *   agent_id `research-cc` → claude (fallback)
 *
 * Returns `{ provider: null, suffix: null, source: null }` when nothing matches.
 */
export function detectProviderSuffix(
  channelName: string | null | undefined,
  agentId: string | null | undefined,
): ProviderSuffixDetection {
  const check = (value: string | null | undefined, source: "channelName" | "agentId") => {
    if (!value) return null;
    const lowered = value.toLowerCase();
    for (const [suffix, provider] of Object.entries(PROVIDER_SUFFIX_MAP)) {
      if (lowered.endsWith(suffix)) {
        return { provider, suffix, source } as ProviderSuffixDetection;
      }
    }
    return null;
  };

  const fromChannel = check(channelName, "channelName");
  if (fromChannel) return fromChannel;

  const fromAgent = check(agentId, "agentId");
  if (fromAgent) return fromAgent;

  return { provider: null, suffix: null, source: null };
}

/** Parses a user-entered skills string (comma- or newline-separated). */
export function parseSkills(skillsText: string): string[] {
  return skillsText
    .split(/[\n,]/)
    .map((skill) => skill.trim())
    .filter(Boolean);
}

const AGENT_ID_RE = /^[a-zA-Z0-9_-]{2,64}$/;
const CHANNEL_ID_RE = /^\d{10,32}$/;

/** Per-step validator. Called for each step to compute the OK/-- indicator. */
export function validateWizardStep(
  step: WizardStepId,
  draft: WizardDraft,
  mode: "create" | "duplicate",
): ValidationResult {
  const errors: string[] = [];

  switch (step) {
    case "role": {
      if (!AGENT_ID_RE.test(draft.agentId.trim())) {
        errors.push("agent_id must be 2-64 chars of [a-zA-Z0-9_-]");
      }
      if (draft.name.trim().length === 0) {
        errors.push("display name is required");
      }
      break;
    }
    case "discord": {
      if (!CHANNEL_ID_RE.test(draft.channelId.trim())) {
        errors.push("channel_id must be a numeric Discord ID (10-32 digits)");
      }
      if (draft.provider.trim().length === 0) {
        errors.push("provider is required");
      }
      break;
    }
    case "prompt": {
      if (mode !== "duplicate" && draft.promptTemplatePath.trim().length === 0) {
        errors.push("prompt template path is required");
      }
      break;
    }
    case "workspace": {
      // Skills / workspace fields are optional. Validation is informational only.
      break;
    }
    case "cron": {
      if (draft.cronEnabled) {
        const fields = draft.cronSpec.trim().split(/\s+/);
        if (fields.length < 5) {
          errors.push("cron expression needs at least 5 fields");
        }
      }
      break;
    }
    case "preview": {
      // Preview aggregates earlier-step validation.
      const aggregate: WizardStepId[] = ["role", "discord", "prompt", "cron"];
      for (const prevStep of aggregate) {
        const result = validateWizardStep(prevStep, draft, mode);
        errors.push(...result.errors);
      }
      break;
    }
  }

  return { valid: errors.length === 0, errors };
}

/** Convenience: validate every step, return array aligned to WIZARD_STEPS. */
export function validateAllSteps(
  draft: WizardDraft,
  mode: "create" | "duplicate",
): ValidationResult[] {
  return WIZARD_STEPS.map((step) => validateWizardStep(step, draft, mode));
}

/** Body for the composite /api/agents/setup endpoint. */
export interface SetupRequestBody {
  agent_id: string;
  channel_id: string;
  provider: CliProvider;
  prompt_template_path: string;
  skills: string[];
  dry_run: boolean;
}

/** Body for /api/agents/{id}/duplicate. */
export interface DuplicateRequestBody {
  new_agent_id: string;
  channel_id: string;
  provider: CliProvider;
  name: string;
  name_ko: string;
  department_id: string | null;
  skills: string[];
  dry_run: boolean;
}

export function buildSetupBody(draft: WizardDraft, dryRun: boolean): SetupRequestBody {
  return {
    agent_id: draft.agentId.trim(),
    channel_id: draft.channelId.trim(),
    provider: draft.provider,
    prompt_template_path: draft.promptTemplatePath.trim(),
    skills: parseSkills(draft.skillsText),
    dry_run: dryRun,
  };
}

export function buildDuplicateBody(
  draft: WizardDraft,
  dryRun: boolean,
): DuplicateRequestBody {
  return {
    new_agent_id: draft.agentId.trim(),
    channel_id: draft.channelId.trim(),
    provider: draft.provider,
    name: draft.name.trim(),
    name_ko: draft.nameKo.trim() || draft.name.trim(),
    department_id: draft.departmentId || null,
    skills: parseSkills(draft.skillsText),
    dry_run: dryRun,
  };
}
