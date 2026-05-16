import AgentAvatar from "../AgentAvatar";
import { SurfaceCard } from "../common/SurfacePrimitives";
import {
  BUTTON_ACCENT_STYLE,
  BUTTON_INFO_STYLE,
  BUTTON_NEUTRAL_STYLE,
  FSM_PANEL_STYLE,
  MUTED_TEXT_STYLE,
  PANEL_SOFT_STYLE,
  PANEL_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorToolbar({ ctx, actions }: Props) {
  return (
    <SurfaceCard
      className="rounded-[24px] p-4 sm:p-5"
      style={ctx.isFsmVariant ? FSM_PANEL_STYLE : PANEL_STYLE}
    >
      {ctx.isFsmVariant ? (
        <FsmToolbar ctx={ctx} actions={actions} />
      ) : (
        <AdvancedToolbar ctx={ctx} actions={actions} />
      )}
    </SurfaceCard>
  );
}

function FsmToolbar({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0 space-y-2">
          <div
            className="text-[11px] font-bold uppercase tracking-[0.22em]"
            style={{
              color: "var(--th-accent-primary)",
              fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
            }}
          >
            Pipeline FSM
          </div>
          <div className="text-[15px] font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {tr("칸반 상태 머신 · 비주얼 에디터", "Kanban state machine · visual editor")}
          </div>
          <div className="flex flex-wrap items-center gap-2 text-[11px] leading-5" style={MUTED_TEXT_STYLE}>
            <span>{tr("엣지를 클릭해 훅/정책을 편집합니다.", "Select an edge to edit hook and policy.")}</span>
            <code
              className="rounded-md border px-2 py-0.5"
              style={{
                borderColor: "color-mix(in srgb, var(--th-border) 78%, transparent)",
                background: "color-mix(in srgb, var(--th-overlay-subtle) 90%, transparent)",
                color: "var(--th-text-secondary)",
                fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
              }}
            >
              kv_meta.kanban_fsm
            </code>
          </div>
          <SelectedAgentLine ctx={ctx} size={18} />
          <div className="flex flex-wrap gap-2">
            {ctx.activeLayers.length > 1 && (
              <Badge tone="warn">{ctx.activeLayers.join(" → ")}</Badge>
            )}
            {ctx.pipelineDraft && (
              <Badge>
                {ctx.pipelineDraft.states.length} {tr("states", "states")} ·{" "}
                {ctx.pipelineDraft.transitions.length} {tr("edges", "edges")}
              </Badge>
            )}
          </div>
        </div>

        <div className="fsm-actions flex flex-wrap items-center justify-end gap-2">
          <button onClick={actions.addState} className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium" style={BUTTON_INFO_STYLE}>
            + {tr("상태 추가", "State")}
          </button>
          <button onClick={actions.addTransition} className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium" style={BUTTON_ACCENT_STYLE}>
            + {tr("전환 추가", "Edge")}
          </button>
          <button
            onClick={actions.handleExportJson}
            disabled={!ctx.pipelineDraft}
            className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
            style={{ ...BUTTON_NEUTRAL_STYLE, opacity: ctx.pipelineDraft ? 1 : 0.45 }}
          >
            {tr("JSON 내보내기", "Export JSON")}
          </button>
          <button
            onClick={() => void actions.handleClearOverride()}
            disabled={ctx.saving || !ctx.overrideExists}
            className="btn rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
            style={{ ...BUTTON_NEUTRAL_STYLE, opacity: ctx.saving || !ctx.overrideExists ? 0.45 : 1 }}
          >
            {tr("기본값", "Reset")}
          </button>
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-2">
        <LevelSwitch ctx={ctx} actions={actions} />
        <SaveControls ctx={ctx} actions={actions} compact />
      </div>
    </div>
  );
}

function AdvancedToolbar({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <div className="flex flex-wrap items-start justify-between gap-3">
      <div className="space-y-2">
        <p className="text-xs leading-6 sm:text-sm sm:leading-6" style={MUTED_TEXT_STYLE}>
          {ctx.editorHelpText}
        </p>
        <SelectedAgentLine ctx={ctx} size={20} />
      </div>

      <div className="flex flex-wrap items-center justify-end gap-2">
        <LevelSwitch ctx={ctx} actions={actions} />
        <button
          onClick={() => actions.setReloadKey((current: number) => current + 1)}
          className="rounded-xl border px-3 py-1.5 text-xs font-medium"
          style={PANEL_SOFT_STYLE}
        >
          {tr("새로고침", "Refresh")}
        </button>
        <button
          onClick={() => void actions.handleClearOverride()}
          disabled={ctx.saving || !ctx.overrideExists}
          className="rounded-xl border px-3 py-1.5 text-xs font-medium"
          style={{
            ...PANEL_SOFT_STYLE,
            borderColor: "color-mix(in srgb, var(--th-accent-warn) 30%, var(--th-border) 70%)",
            color: "var(--th-text-primary)",
            opacity: ctx.saving || !ctx.overrideExists ? 0.45 : 1,
          }}
        >
          {tr("오버라이드 상속", "Clear override")}
        </button>
        <SaveButton ctx={ctx} actions={actions} label={tr("변경 저장", "Save changes")} />
      </div>
    </div>
  );
}

function SelectedAgentLine({ ctx, size }: { ctx: any; size: number }) {
  if (!ctx.selectedAgentDetail) {
    return null;
  }
  return (
    <p className="flex flex-wrap items-center gap-1.5 text-xs leading-5" style={MUTED_TEXT_STYLE}>
      {ctx.tr("현재 선택된 에이전트", "Selected agent")}:{" "}
      <AgentAvatar agent={ctx.selectedAgentDetail.agent} agents={ctx.agents} size={size} />
      <span style={{ color: "var(--th-text-primary)" }}>{ctx.selectedAgentDetail.name}</span>
    </p>
  );
}

function LevelSwitch({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <div
      className="inline-flex rounded-full border p-1"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-overlay-subtle) 86%, transparent)",
      }}
    >
      <button
        onClick={() => actions.setLevel("repo")}
        className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
        style={{
          background: ctx.level === "repo" ? "var(--th-accent-primary-soft)" : "transparent",
          color: ctx.level === "repo" ? "var(--th-text-primary)" : "var(--th-text-muted)",
        }}
      >
        {tr("레포 레벨", "Repo level")}
      </button>
      <button
        onClick={() => actions.setLevel("agent")}
        disabled={!ctx.selectedAgentId}
        className="rounded-full px-3 py-1.5 text-xs font-medium transition-colors"
        style={{
          background: ctx.level === "agent" ? "var(--th-accent-primary-soft)" : "transparent",
          color: ctx.level === "agent" ? "var(--th-text-primary)" : "var(--th-text-muted)",
          opacity: ctx.selectedAgentId ? 1 : 0.45,
        }}
      >
        {tr("에이전트 레벨", "Agent level")}
      </button>
    </div>
  );
}

function SaveControls({ ctx, actions, compact }: Props & { compact?: boolean }) {
  const tr = ctx.tr;
  return (
    <div className="flex flex-wrap items-center justify-end gap-2">
      <button
        onClick={() => actions.setReloadKey((current: number) => current + 1)}
        className="rounded-lg border px-2.5 py-1.5 text-[11px] font-medium"
        style={BUTTON_NEUTRAL_STYLE}
      >
        {tr("새로고침", "Refresh")}
      </button>
      <SaveButton ctx={ctx} actions={actions} label={compact ? tr("저장", "Save") : tr("변경 저장", "Save changes")} />
    </div>
  );
}

function SaveButton({ ctx, actions, label }: Props & { label: string }) {
  return (
    <button
      onClick={() => void actions.handleSave()}
      disabled={ctx.saving || !ctx.hasVisibleChanges}
      className="rounded-xl border px-3.5 py-1.5 text-xs font-semibold disabled:opacity-50"
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 30%, var(--th-border) 70%)",
        background: "var(--th-accent-primary-soft)",
        color: "var(--th-text-primary)",
      }}
    >
      {ctx.saving
        ? ctx.tr("저장 중…", "Saving…")
        : ctx.hasVisibleChanges
          ? label
          : ctx.tr("변경 없음", "No changes")}
    </button>
  );
}

function Badge({ children, tone }: { children: React.ReactNode; tone?: "warn" }) {
  return (
    <span
      className="rounded-md border px-2 py-1 text-[10px] font-semibold uppercase tracking-[0.16em]"
      style={{
        borderColor:
          tone === "warn"
            ? "color-mix(in srgb, var(--th-accent-warn) 36%, var(--th-border) 64%)"
            : "color-mix(in srgb, var(--th-border) 82%, transparent)",
        background:
          tone === "warn"
            ? "color-mix(in srgb, var(--th-badge-amber-bg) 72%, var(--th-card-bg) 28%)"
            : "color-mix(in srgb, var(--th-overlay-subtle) 88%, transparent)",
        color: tone === "warn" ? "var(--th-text-primary)" : "var(--th-text-secondary)",
        fontFamily: "ui-monospace, SFMono-Regular, SF Mono, Menlo, monospace",
      }}
    >
      {children}
    </span>
  );
}
