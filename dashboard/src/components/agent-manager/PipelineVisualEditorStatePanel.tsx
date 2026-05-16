import {
  BUTTON_DANGER_STYLE,
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

export default function PipelineVisualEditorStatePanel({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const selectedState = ctx.selectedState;
  const pipelineDraft = ctx.pipelineDraft;

  return (
    <div className="space-y-3">
      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("상태 ID", "State ID")}
          </label>
          <div
            className="rounded-xl border px-3 py-2 text-sm font-mono"
            style={{ ...PANEL_SOFT_STYLE, color: "var(--th-text-primary)" }}
          >
            {selectedState.id}
          </div>
        </div>
        <div>
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

      <div className="grid gap-3 sm:grid-cols-2">
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("on_enter 훅", "on_enter hooks")}
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
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("on_exit 훅", "on_exit hooks")}
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
        <button onClick={() => actions.clearStateTimeout(selectedState.id)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
          {tr("타임아웃 비우기", "Clear timeout")}
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

      <TimeoutPanel ctx={ctx} actions={actions} />

      <button
        onClick={() => actions.removeState(selectedState.id)}
        className="rounded-xl border px-3 py-1.5 text-xs font-medium"
        style={BUTTON_DANGER_STYLE}
      >
        {tr("이 상태 삭제", "Delete state")}
      </button>
    </div>
  );
}

function TimeoutPanel({ ctx, actions }: Props) {
  const tr = ctx.tr;
  const selectedState = ctx.selectedState;
  const pipelineDraft = ctx.pipelineDraft;

  return (
    <div className="rounded-[20px] border p-4 space-y-3" style={PANEL_SOFT_STYLE}>
      <div className="flex items-center justify-between gap-2">
        <h5 className="text-xs font-semibold uppercase tracking-wider" style={MUTED_TEXT_STYLE}>
          {tr("타임아웃", "Timeout")}
        </h5>
        <span className="text-xs" style={MUTED_TEXT_STYLE}>
          {tr("gate, timeout 등 노드 속성", "Node properties like gates and timeout")}
        </span>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <TextField
          label={tr("지속 시간", "Duration")}
          value={pipelineDraft.timeouts[selectedState.id]?.duration ?? ""}
          onChange={(value: string) => actions.updateStateTimeout(selectedState.id, { duration: value })}
          placeholder="30m"
        />
        <TextField
          label={tr("참조 클록", "Clock key")}
          value={pipelineDraft.timeouts[selectedState.id]?.clock ?? ""}
          onChange={(value: string) => actions.updateStateTimeout(selectedState.id, { clock: value })}
          placeholder="review_entered_at"
        />
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("최대 재시도", "Max retries")}
          </label>
          <input
            type="number"
            value={pipelineDraft.timeouts[selectedState.id]?.max_retries ?? ""}
            onChange={(event) =>
              actions.updateStateTimeout(selectedState.id, {
                max_retries: event.target.value === "" ? undefined : Number(event.target.value),
              })
            }
            className={INPUT_CLASS}
            style={INPUT_STYLE}
            min={0}
          />
        </div>
        <div>
          <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
            {tr("소진 시 이동", "On exhaust")}
          </label>
          <select
            value={pipelineDraft.timeouts[selectedState.id]?.on_exhaust ?? ""}
            onChange={(event) =>
              actions.updateStateTimeout(selectedState.id, {
                on_exhaust: event.target.value || undefined,
              })
            }
            className={INPUT_CLASS}
            style={INPUT_STYLE}
          >
            <option value="">{tr("없음", "None")}</option>
            {pipelineDraft.states.map((state: any) => (
              <option key={state.id} value={state.id}>
                {state.id}
              </option>
            ))}
          </select>
        </div>
      </div>
      <TextField
        label={tr("조건식", "Condition")}
        value={pipelineDraft.timeouts[selectedState.id]?.condition ?? ""}
        onChange={(value: string) => actions.updateStateTimeout(selectedState.id, { condition: value || undefined })}
        placeholder="review_status = 'awaiting_dod'"
      />
    </div>
  );
}

function TextField(props: { label: string; value: string; onChange: (value: string) => void; placeholder?: string }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <input
        value={props.value}
        onChange={(event) => props.onChange(event.target.value)}
        className={INPUT_CLASS}
        style={INPUT_STYLE}
        placeholder={props.placeholder}
      />
    </div>
  );
}
