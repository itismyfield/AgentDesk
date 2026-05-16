import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Agent, CliProvider, Department } from "../../types";
import type { Translator } from "./types";
import * as api from "../../api";
import {
  dryRunSetupAgent,
  dryRunDuplicateAgent,
} from "../../api/agentsSetup";
import {
  WIZARD_STEPS,
  buildSetupBody,
  buildDuplicateBody,
  detectProviderSuffix,
  validateAllSteps,
  type WizardDraft,
} from "./setupWizardHelpers";

export type WizardMode = "create" | "duplicate";

export interface AgentSetupWizardProps {
  open: boolean;
  mode: WizardMode;
  sourceAgent?: Agent | null;
  departments: Department[];
  locale: string;
  tr: Translator;
  onClose: () => void;
  onDone: () => void;
}

export const steps = WIZARD_STEPS;

export const inputClass =
  "w-full rounded-2xl border px-3 py-2 text-sm outline-none transition-shadow focus:ring-2 focus:ring-blue-500/30";

export const inputStyle = {
  background: "var(--th-input-bg)",
  borderColor: "var(--th-input-border)",
  color: "var(--th-text-primary)",
};

export function buildDefaultDraft(sourceAgent?: Agent | null): WizardDraft {
  const sourceId = sourceAgent?.id ?? "";
  const fallbackId = sourceId ? `${sourceId}-copy` : "";
  return {
    agentId: fallbackId,
    name: sourceAgent ? `${sourceAgent.name} Copy` : "",
    nameKo: sourceAgent?.name_ko ? `${sourceAgent.name_ko} Copy` : "",
    departmentId: sourceAgent?.department_id ?? "",
    provider: sourceAgent?.cli_provider ?? "codex",
    channelId: "",
    promptTemplatePath:
      sourceAgent?.prompt_path ?? "~/.adk/release/config/agents/_shared.prompt.md",
    promptContent: sourceAgent?.prompt_content ?? "",
    skillsText: "",
    cronEnabled: false,
    cronSpec: "0 9 * * 1-5",
  };
}

export function labelForStep(step: (typeof steps)[number], tr: Translator): string {
  switch (step) {
    case "role":
      return tr("역할", "Role");
    case "discord":
      return tr("Discord", "Discord");
    case "prompt":
      return tr("프롬프트", "Prompt");
    case "workspace":
      return tr("작업공간", "Workspace");
    case "cron":
      return tr("Cron", "Cron");
    case "preview":
      return tr("확인", "Confirm");
  }
}

interface UseAgentSetupWizardStateOptions {
  open: boolean;
  mode: WizardMode;
  sourceAgent?: Agent | null;
  onDone: () => void;
}

export function useAgentSetupWizardState({
  open,
  mode,
  sourceAgent,
  onDone,
}: UseAgentSetupWizardStateOptions) {
  const [stepIndex, setStepIndex] = useState(0);
  const [draft, setDraft] = useState<WizardDraft>(() => buildDefaultDraft(sourceAgent));
  const [preview, setPreview] = useState<api.AgentSetupResponse | null>(null);
  const [liveValidation, setLiveValidation] = useState<api.AgentSetupResponse | null>(null);
  const [liveValidationError, setLiveValidationError] = useState<string | null>(null);
  const [validating, setValidating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [rollback, setRollback] = useState<unknown>(null);
  const [providerAuto, setProviderAuto] = useState<string | null>(null);
  const [channelName, setChannelName] = useState<string | null>(null);
  const providerTouchedRef = useRef(false);

  useEffect(() => {
    if (!open) return;
    setStepIndex(0);
    setDraft(buildDefaultDraft(sourceAgent));
    setPreview(null);
    setLiveValidation(null);
    setLiveValidationError(null);
    setError(null);
    setRollback(null);
    setProviderAuto(null);
    setChannelName(null);
    providerTouchedRef.current = false;
  }, [open, sourceAgent]);

  // Debounced Discord channel lookup. A resolved channel name lets provider
  // suffix detection use visible channel naming before falling back to role_id.
  useEffect(() => {
    if (!open) return;
    const channelId = draft.channelId.trim();
    if (!/^\d{10,32}$/.test(channelId)) {
      setChannelName(null);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      api
        .getDiscordChannelInfo(channelId)
        .then((info) => {
          if (!cancelled) setChannelName(info.name ?? null);
        })
        .catch(() => {
          if (!cancelled) setChannelName(null);
        });
    }, 400);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [draft.channelId, open]);

  useEffect(() => {
    if (!open) return;
    const detection = detectProviderSuffix(channelName, draft.agentId);
    if (detection.provider && detection.suffix) {
      setProviderAuto(
        `${detection.suffix} → ${detection.provider} (${detection.source})`,
      );
      if (!providerTouchedRef.current && draft.provider !== detection.provider) {
        setDraft((prev) => ({ ...prev, provider: detection.provider as CliProvider }));
      }
    } else {
      setProviderAuto(null);
    }
  }, [channelName, draft.agentId, draft.provider, open]);

  const validationResults = useMemo(
    () => validateAllSteps(draft, mode),
    [draft, mode],
  );
  const validationByStep = validationResults.map((result) => result.valid);
  const currentValid = validationByStep[stepIndex];
  const currentStep = steps[stepIndex];

  useEffect(() => {
    if (!open) return;
    const structurallyValid =
      validationByStep[0] && validationByStep[1] && validationByStep[2];
    if (!structurallyValid) {
      setLiveValidation(null);
      setLiveValidationError(null);
      return;
    }
    let cancelled = false;
    const timer = setTimeout(() => {
      setValidating(true);
      const request =
        mode === "duplicate" && sourceAgent
          ? dryRunDuplicateAgent(sourceAgent.id, buildDuplicateBody(draft, true))
          : dryRunSetupAgent(buildSetupBody(draft, true));
      request
        .then((result) => {
          if (cancelled) return;
          setLiveValidation(result);
          setLiveValidationError(null);
        })
        .catch((caught) => {
          if (cancelled) return;
          setLiveValidation(null);
          setLiveValidationError(
            caught instanceof Error ? caught.message : String(caught),
          );
        })
        .finally(() => {
          if (!cancelled) setValidating(false);
        });
    }, 500);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
    // We intentionally depend on individual draft fields so updates to any
    // wizard input re-run the dry-run conflict check.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    open,
    mode,
    sourceAgent,
    draft.agentId,
    draft.channelId,
    draft.provider,
    draft.promptTemplatePath,
    draft.skillsText,
    validationByStep[0],
    validationByStep[1],
    validationByStep[2],
  ]);

  const runPreview = useCallback(async () => {
    setBusy(true);
    setError(null);
    setRollback(null);
    try {
      const result =
        mode === "duplicate" && sourceAgent
          ? await api.duplicateAgent(sourceAgent.id, buildDuplicateBody(draft, true))
          : await api.setupAgent(buildSetupBody(draft, true));
      setPreview(result);
      if (result.rollback) setRollback(result.rollback);
    } catch (caught) {
      setPreview(null);
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  }, [draft, mode, sourceAgent]);

  const runConfirm = useCallback(async () => {
    setBusy(true);
    setError(null);
    setRollback(null);
    try {
      if (mode === "duplicate" && sourceAgent) {
        const result = await api.duplicateAgent(
          sourceAgent.id,
          buildDuplicateBody(draft, false),
        );
        if (result.rollback) setRollback(result.rollback);
        if (draft.promptContent.trim()) {
          await api.updateAgent(draft.agentId.trim(), {
            prompt_content: draft.promptContent,
            auto_commit: false,
          });
        }
      } else {
        const result = await api.setupAgent(buildSetupBody(draft, false));
        if (result.rollback) setRollback(result.rollback);
        await api.updateAgent(draft.agentId.trim(), {
          name: draft.name.trim(),
          name_ko: draft.nameKo.trim() || draft.name.trim(),
          department_id: draft.departmentId || null,
          cli_provider: draft.provider,
          prompt_content: draft.promptContent,
          auto_commit: false,
        });
      }
      onDone();
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : String(caught));
      const maybeDetails = (caught as { details?: unknown } | null)?.details;
      if (maybeDetails && typeof maybeDetails === "object") {
        const rb = (maybeDetails as { rollback?: unknown }).rollback;
        if (rb) setRollback(rb);
      }
    } finally {
      setBusy(false);
    }
  }, [draft, mode, onDone, sourceAgent]);

  return {
    stepIndex,
    setStepIndex,
    draft,
    setDraft,
    preview,
    liveValidation,
    liveValidationError,
    validating,
    busy,
    error,
    rollback,
    providerAuto,
    channelName,
    providerTouchedRef,
    validationByStep,
    currentValid,
    currentStep,
    runPreview,
    runConfirm,
  };
}
