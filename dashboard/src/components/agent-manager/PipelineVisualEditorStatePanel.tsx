import { ActionMenu } from "../common/ActionMenu";
import {
  BUTTON_NEUTRAL_STYLE,
  INPUT_CLASS,
  INPUT_STYLE,
  MUTED_TEXT_STYLE,
  PANEL_SOFT_STYLE,
  TEXTAREA_CLASS,
  joinCommaSeparated,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

/** Destructive state actions live behind a menu with a confirmation step. */
export function PipelineStateMenu({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const state = ctx.selectedState;
  const name = state.label || state.id;
  return (
    <ActionMenu
      label={tr("상태 메뉴", "State menu")}
      items={[{
        key: "delete",
        label: tr("이 상태 삭제", "Delete state"),
        danger: true,
        onSelect: () => actions.removeState(state.id),
        confirm: {
          title: tr(`'${name}' 상태를 삭제할까요?`, `Delete the '${name}' state?`),
          description: tr(
            "이 상태로 들어오고 나가는 전환과 훅도 함께 지워져요. 편집기에서 저장하기 전까지는 다시 불러와 되돌릴 수 있어요.",
            "Transitions and hooks attached to this state are removed too. Until you save the editor, reloading restores it.",
          ),
          confirmLabel: tr("삭제", "Delete"),
          cancelLabel: tr("취소", "Cancel"),
        },
      }]}
    />
  );
}

export default function PipelineVisualEditorStatePanel({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const selectedState = ctx.selectedState;
  const pipelineDraft = ctx.pipelineDraft;

  return (
    <div className="space-y-3">
      <div className="grid gap-3 sm:grid-cols-2">
        <div className="min-w-0">
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("상태 ID", "State ID")}
          </label>
          <div
            className="rounded-xl border px-3 py-2 text-sm font-mono [overflow-wrap:anywhere]"
            style={{ ...PANEL_SOFT_STYLE, color: "var(--th-text-primary)" }}
          >
            {selectedState.id}
          </div>
        </div>
        <div className="min-w-0">
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("레이블", "Label")}
          </label>
          <input
            value={selectedState.label}
            onChange={(event) => actions.updateState(selectedState.id, { label: event.target.value })}
            className={INPUT_CLASS}
            style={INPUT_STYLE}
          />
        </div>
      </div>

      <label className="flex items-center gap-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
        <input
          type="checkbox"
          checked={!!selectedState.terminal}
          onChange={(event) => actions.updateState(selectedState.id, { terminal: event.target.checked })}
        />
        {tr("터미널 상태", "Terminal state")}
      </label>

      <div className="grid gap-3">
        <div className="min-w-0">
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("들어올 때 실행할 훅 (on_enter)", "on_enter hooks")}
          </label>
          <textarea
            rows={3}
            value={joinCommaSeparated(pipelineDraft.hooks[selectedState.id]?.on_enter)}
            onChange={(event) => actions.updateStateHooks(selectedState.id, "on_enter", event.target.value)}
            className={TEXTAREA_CLASS}
            style={INPUT_STYLE}
            placeholder="OnCardTransition, OnReviewEnter"
          />
        </div>
        <div className="min-w-0">
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("나갈 때 실행할 훅 (on_exit)", "on_exit hooks")}
          </label>
          <textarea
            rows={3}
            value={joinCommaSeparated(pipelineDraft.hooks[selectedState.id]?.on_exit)}
            onChange={(event) => actions.updateStateHooks(selectedState.id, "on_exit", event.target.value)}
            className={TEXTAREA_CLASS}
            style={INPUT_STYLE}
            placeholder="OnStateExit"
          />
        </div>
      </div>

      <div className="flex flex-wrap gap-2">
        <button onClick={() => actions.clearStateHooks(selectedState.id)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
          {tr("훅 비우기", "Clear hooks")}
        </button>
        <button onClick={() => actions.clearStateClock(selectedState.id)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
          {tr("클록 비우기", "Clear clock")}
        </button>
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("클록 필드", "Clock field")}
          </label>
          <input
            value={pipelineDraft.clocks[selectedState.id]?.set ?? ""}
            onChange={(event) => actions.updateStateClock(selectedState.id, { set: event.target.value })}
            className={INPUT_CLASS}
            style={INPUT_STYLE}
            placeholder="started_at"
          />
        </div>
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("클록 모드", "Clock mode")}
          </label>
          <input
            value={pipelineDraft.clocks[selectedState.id]?.mode ?? ""}
            onChange={(event) => actions.updateStateClock(selectedState.id, { mode: event.target.value || undefined })}
            className={INPUT_CLASS}
            style={INPUT_STYLE}
            placeholder="coalesce"
          />
        </div>
      </div>

    </div>
  );
}
