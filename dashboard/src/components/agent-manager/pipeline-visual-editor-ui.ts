import type { CSSProperties } from "react";

import { localeName } from "../../i18n";
import type { Agent, PipelineConfigFull, UiLanguage } from "../../types";
import type { FsmEdgeBinding, Selection } from "./pipeline-visual-editor-model";
import type { PipelineVisualEditorProps } from "./pipeline-visual-editor-types";

export const INPUT_CLASS =
  "w-full rounded-xl border bg-transparent px-3 py-2 text-sm outline-none";
export const TEXTAREA_CLASS =
  "w-full rounded-xl border bg-transparent px-3 py-2 text-sm outline-none resize-y";

export const INPUT_STYLE = {
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
  color: "var(--th-text-primary)",
  backgroundColor: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
} as const;

export const FSM_INPUT_STYLE = {
  borderColor: "color-mix(in srgb, var(--th-border) 80%, transparent)",
  color: "var(--th-text-primary)",
  backgroundColor: "#11141b",
} as const;

export const MUTED_TEXT_STYLE = { color: "var(--th-text-muted)" } as const;

export const SECTION_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
};

export const PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
};

export const PANEL_SOFT_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
  background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
};

export const EMPTY_PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
  background: "color-mix(in srgb, var(--th-overlay-subtle) 92%, transparent)",
  color: "var(--th-text-muted)",
};

export const STATUS_INFO_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
  background: "color-mix(in srgb, var(--th-overlay-subtle) 88%, transparent)",
  color: "var(--th-text-secondary)",
};

export const STATUS_SUCCESS_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 28%, var(--th-border) 72%)",
  background: "color-mix(in srgb, var(--th-badge-emerald-bg) 84%, var(--th-card-bg) 16%)",
  color: "var(--th-text-primary)",
};

export const STATUS_ERROR_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
  background: "color-mix(in srgb, rgba(255, 107, 107, 0.16) 84%, var(--th-card-bg) 16%)",
  color: "var(--th-text-primary)",
};

export const BUTTON_NEUTRAL_STYLE: CSSProperties = {
  ...PANEL_SOFT_STYLE,
  color: "var(--th-text-primary)",
};

export const BUTTON_ACCENT_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
  background: "var(--th-accent-primary-soft)",
};

export const BUTTON_INFO_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-info) 30%, var(--th-border) 70%)",
};

export const BUTTON_WARN_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-warn) 30%, var(--th-border) 70%)",
};

export const BUTTON_DANGER_STYLE: CSSProperties = {
  ...BUTTON_NEUTRAL_STYLE,
  borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
};

export const FSM_PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 78%, transparent)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, #090b0f 3%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, #05070a 2%) 100%)",
};

export const FSM_INSPECTOR_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-accent-primary) 40%, var(--th-border) 60%)",
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, #0b0d10 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, #080a0d 2%) 100%)",
};

export const FSM_DETAIL_PANEL_STYLE: CSSProperties = {
  borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
  background: "#11141b",
};

export const FSM_EDGE_BINDINGS_KEY = "fsm_edge_bindings";

export const FSM_EVENT_OPTIONS = [
  "on_enqueue",
  "on_dispatch",
  "on_submit",
  "on_approve",
  "on_changes_request",
  "on_error",
  "on_recover",
] as const;

export const FSM_HOOK_OPTIONS = [
  "OnQueueReady",
  "OnDispatchRequested",
  "OnDispatchCompleted",
  "OnReviewEnter",
  "OnReviewApproved",
  "OnChangesRequested",
  "OnPipelineError",
  "OnRecoverFromFailure",
] as const;

export function parseCommaSeparated(value: string) {
  return value
    .split(",")
    .map((part) => part.trim())
    .filter(Boolean);
}

export function joinCommaSeparated(value: string[] | undefined) {
  return value && value.length > 0 ? value.join(", ") : "";
}

export function downloadTextFile(filename: string, content: string) {
  const blob = new Blob([content], { type: "application/json;charset=utf-8" });
  const href = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = href;
  link.download = filename;
  link.click();
  URL.revokeObjectURL(href);
}

export function normalizeFsmEdgeBindings(value: unknown): Record<string, FsmEdgeBinding> {
  if (!value || typeof value !== "object") {
    return {};
  }

  return Object.fromEntries(
    Object.entries(value as Record<string, unknown>).flatMap(([key, entry]) => {
      if (!entry || typeof entry !== "object" || typeof (entry as FsmEdgeBinding).event !== "string") {
        return [];
      }
      return [[key, { event: (entry as FsmEdgeBinding).event }]];
    }),
  );
}

export function formatSelectionTitle(
  tr: PipelineVisualEditorProps["tr"],
  selection: Selection,
  pipeline: PipelineConfigFull | null,
) {
  if (!pipeline || !selection) {
    return tr("속성을 편집할 요소를 선택하세요", "Select a node or edge to edit");
  }
  if (selection.kind === "state") {
    return `${tr("상태", "State")} · ${selection.stateId}`;
  }
  if (selection.kind === "transition") {
    const transition = pipeline.transitions[selection.index];
    return transition ? `${transition.from} → ${transition.to}` : tr("전환 편집", "Transition editor");
  }
  return tr("Phase Gate", "Phase Gate");
}

export function selectedAgentInfo(
  agents: Agent[],
  locale: UiLanguage,
  selectedAgentId?: string | null,
): { agent: Agent; name: string } | null {
  const agent = selectedAgentId
    ? agents.find((candidate) => candidate.id === selectedAgentId)
    : null;
  return agent ? { agent, name: localeName(locale, agent) } : null;
}
