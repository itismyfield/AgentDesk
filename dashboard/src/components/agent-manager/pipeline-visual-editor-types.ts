import type {
  Agent,
  PipelineConfigFull,
  PipelineStage,
  UiLanguage,
} from "../../types";
import type { Selection, StageDraft } from "./pipeline-visual-editor-model";

export interface PipelineVisualEditorProps {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  repo?: string;
  agents: Agent[];
  selectedAgentId?: string | null;
  variant?: "advanced" | "fsm";
}

export type EditLevel = "repo" | "agent";

export interface EditorSnapshot {
  pipeline: PipelineConfigFull;
  layers: { default: boolean; repo: boolean; agent: boolean };
  rawOverride: unknown;
  repoStages: PipelineStage[];
}

export interface PersistedFsmDraftEntry {
  repo: string;
  level: EditLevel;
  agentId: string | null;
  updatedAtMs: number;
  pipeline: PipelineConfigFull;
  stageDrafts: StageDraft[];
  selection: Selection;
  overrideExtras: Record<string, unknown>;
}

export interface PersistedFsmDraftStore {
  version: 2;
  entries: Record<string, PersistedFsmDraftEntry>;
}

export interface PersistedPipelineSnapshotEntry {
  repo: string;
  level: EditLevel;
  agentId: string | null;
  updatedAtMs: number;
  snapshot: EditorSnapshot;
}

export interface PersistedPipelineSnapshotStore {
  version: 1;
  entries: Record<string, PersistedPipelineSnapshotEntry>;
}
