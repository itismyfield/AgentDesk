import { localeName } from "../../i18n";
import { isCounterProvider, type StageDraft } from "./pipeline-visual-editor-model";
import {
  BUTTON_DANGER_STYLE,
  BUTTON_INFO_STYLE,
  BUTTON_NEUTRAL_STYLE,
  EMPTY_PANEL_STYLE,
  INPUT_CLASS,
  INPUT_STYLE,
  MUTED_TEXT_STYLE,
  PANEL_SOFT_STYLE,
  PANEL_STYLE,
} from "./pipeline-visual-editor-ui";

interface Props {
  ctx: any;
  actions: any;
}

export default function PipelineVisualEditorStagePanel({ ctx, actions }: Props) {
  const tr = ctx.tr;

  return (
    <div className="min-w-0 rounded-[24px] border p-4 sm:p-5 space-y-4" style={PANEL_STYLE}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h4 className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {tr("파이프라인 스테이지", "Pipeline Stages")}
          </h4>
          <p className="text-xs" style={MUTED_TEXT_STYLE}>
            {tr(
              "레포 전체에 적용되는 자동 실행 단계입니다. 카드가 트리거 상태에 들어가거나 리뷰를 통과하면 순서대로 진행합니다.",
              "Automated stages for the whole repository. They run in order when a card reaches the trigger state or passes review.",
            )}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button onClick={actions.addStage} className="rounded-xl border px-3 py-1.5 text-xs font-medium" style={BUTTON_INFO_STYLE}>
            + {tr("스테이지", "Stage")}
          </button>
          <button
            onClick={() => void actions.handleClearStages()}
            disabled={ctx.saving || (ctx.stageDrafts.length === 0 && ctx.allRepoStages.length === 0)}
            className="rounded-xl border px-3 py-1.5 text-xs"
            style={{
              ...BUTTON_DANGER_STYLE,
              opacity: ctx.saving || (ctx.stageDrafts.length === 0 && ctx.allRepoStages.length === 0) ? 0.45 : 1,
            }}
          >
            {tr("스테이지 모두 지우기", "Clear all stages")}
          </button>
        </div>
      </div>

      {ctx.stageDrafts.length === 0 ? (
        <div className="rounded-[20px] border px-4 py-6 text-center text-sm" style={EMPTY_PANEL_STYLE}>
          {tr(
            "스테이지가 없습니다. 아래의 + 버튼으로 자동 실행 단계를 추가하세요.",
            "No stages yet. Add an automated stage with the + button.",
          )}
        </div>
      ) : (
        <div className="grid min-w-0 gap-3 xl:grid-cols-2">
          {ctx.stageDrafts.map((stage: StageDraft, index: number) => (
            <StageCard key={`${stage.stage_name}-${index}`} ctx={ctx} actions={actions} stage={stage} index={index} />
          ))}
        </div>
      )}
    </div>
  );
}

function StageCard({ ctx, actions, stage, index }: Props & { stage: StageDraft; index: number }) {
  const tr = ctx.tr;
  const assignedAgent = ctx.agents.find((agent: any) => agent.id === stage.agent_override_id);

  return (
    <div className="min-w-0 rounded-[20px] border p-4 space-y-3" style={PANEL_SOFT_STYLE}>
      <div className="flex items-center gap-2">
        <span
          className="inline-flex h-7 w-7 items-center justify-center rounded-full text-xs font-semibold"
          style={{ background: "var(--th-accent-primary-soft)", color: "var(--th-text-primary)" }}
        >
          {index + 1}
        </span>
        <input
          value={stage.stage_name}
          onChange={(event) => actions.updateStage(index, { stage_name: event.target.value })}
          className={INPUT_CLASS}
          style={INPUT_STYLE}
          placeholder={tr("스테이지 이름", "Stage name")}
        />
      </div>

      <div className="grid gap-3 sm:grid-cols-2">
        <SelectField
          label={tr("트리거", "Trigger")}
          value={stage.trigger_after}
          onChange={(value) => actions.updateStage(index, { trigger_after: value as StageDraft["trigger_after"] })}
          options={[
            ["ready", tr("카드 준비 시", "On ready")],
            ["review_pass", tr("리뷰 통과 후", "After review pass")],
          ]}
        />
        <ReadOnlyField
          label={tr("실행 방식", "Provider")}
          value={isCounterProvider(stage.provider) ? tr("교차 모델", "Counter model") : stage.provider || tr("담당 에이전트", "Assigned agent")}
          note={tr("읽기 전용", "Read-only")}
        />
        {isCounterProvider(stage.provider) ? (
          <ReadOnlyField
            label={tr("담당 에이전트 지정", "Agent override")}
            value={assignedAgent ? localeName(ctx.locale, assignedAgent) : stage.agent_override_id || tr("카드 담당자", "Card assignee")}
            note={tr("읽기 전용", "Read-only")}
          />
        ) : (
          <AgentSelect ctx={ctx} label={tr("담당 에이전트 지정", "Agent override")} value={stage.agent_override_id} emptyLabel={tr("카드 담당자", "Card assignee")} onChange={(value) => actions.updateStage(index, { agent_override_id: value })} />
        )}
        <ReadOnlyField
          label={tr("건너뛰기", "Skip")}
          value={stage.skip_condition === "no_rs_changes" ? tr("Rust 변경이 없으면", "When no Rust files changed") : stage.skip_condition || tr("건너뛰지 않음", "Never")}
          note={tr("읽기 전용", "Read-only")}
        />
      </div>

      <div className="flex flex-wrap gap-2">
        {index > 0 && (
          <button onClick={() => actions.moveStage(index, -1)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
            ↑ {tr("앞으로", "Earlier")}
          </button>
        )}
        {index < ctx.stageDrafts.length - 1 && (
          <button onClick={() => actions.moveStage(index, 1)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_NEUTRAL_STYLE}>
            ↓ {tr("뒤로", "Later")}
          </button>
        )}
        <button onClick={() => actions.removeStage(index)} className="rounded-xl border px-3 py-1.5 text-xs" style={BUTTON_DANGER_STYLE}>
          {tr("삭제", "Delete")}
        </button>
      </div>
    </div>
  );
}

function ReadOnlyField(props: { label: string; value: string; note: string }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>{props.label}</label>
      <p className="whitespace-pre-wrap break-words text-sm" style={INPUT_STYLE}>{props.value}</p>
      <span className="text-xs" style={MUTED_TEXT_STYLE}>{props.note}</span>
    </div>
  );
}

function SelectField(props: { label: string; value: string; onChange: (value: string) => void; options: [string, string][] }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <select value={props.value} onChange={(event) => props.onChange(event.target.value)} className={INPUT_CLASS} style={INPUT_STYLE}>
        {props.options.map(([value, label]) => (
          <option key={value} value={value}>{label}</option>
        ))}
      </select>
    </div>
  );
}

function AgentSelect(props: { ctx: any; label: string; value: string; emptyLabel: string; onChange: (value: string) => void }) {
  return (
    <div>
      <label className="mb-1 block text-xs" style={MUTED_TEXT_STYLE}>
        {props.label}
      </label>
      <select value={props.value} onChange={(event) => props.onChange(event.target.value)} className={INPUT_CLASS} style={INPUT_STYLE}>
        <option value="">{props.emptyLabel}</option>
        {props.ctx.agents.map((agent: any) => (
          <option key={agent.id} value={agent.id}>
            {localeName(props.ctx.locale, agent)}
          </option>
        ))}
      </select>
    </div>
  );
}
