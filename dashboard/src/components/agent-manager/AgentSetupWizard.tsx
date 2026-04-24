import { useCallback, useEffect, useMemo, useState } from "react";
import type { Agent, CliProvider, Department } from "../../types";
import type { Translator } from "./types";
import { localeName } from "../../i18n";
import * as api from "../../api";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceNotice,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import AgentPromptEditor from "./AgentPromptEditor";
import { CLI_PROVIDERS } from "./constants";

type WizardMode = "create" | "duplicate";

interface AgentSetupWizardProps {
  open: boolean;
  mode: WizardMode;
  sourceAgent?: Agent | null;
  departments: Department[];
  locale: string;
  tr: Translator;
  onClose: () => void;
  onDone: () => void;
}

interface WizardDraft {
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

const steps = [
  "role",
  "discord",
  "prompt",
  "workspace",
  "cron",
  "preview",
] as const;

const inputClass =
  "w-full rounded-2xl border px-3 py-2 text-sm outline-none transition-shadow focus:ring-2 focus:ring-blue-500/30";
const inputStyle = {
  background: "var(--th-input-bg)",
  borderColor: "var(--th-input-border)",
  color: "var(--th-text-primary)",
};

function buildDefaultDraft(sourceAgent?: Agent | null): WizardDraft {
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

function parseSkills(skillsText: string): string[] {
  return skillsText
    .split(/[\n,]/)
    .map((skill) => skill.trim())
    .filter(Boolean);
}

function labelForStep(step: (typeof steps)[number], tr: Translator): string {
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

export default function AgentSetupWizard({
  open,
  mode,
  sourceAgent,
  departments,
  locale,
  tr,
  onClose,
  onDone,
}: AgentSetupWizardProps) {
  const [stepIndex, setStepIndex] = useState(0);
  const [draft, setDraft] = useState<WizardDraft>(() => buildDefaultDraft(sourceAgent));
  const [preview, setPreview] = useState<api.AgentSetupResponse | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setStepIndex(0);
    setDraft(buildDefaultDraft(sourceAgent));
    setPreview(null);
    setError(null);
  }, [open, sourceAgent]);

  const validationByStep = useMemo(() => {
    const agentIdValid = /^[a-zA-Z0-9_-]{2,64}$/.test(draft.agentId.trim());
    const channelValid = /^\d{10,32}$/.test(draft.channelId.trim());
    const promptPathValid =
      mode === "duplicate" || draft.promptTemplatePath.trim().length > 0;
    const cronValid =
      !draft.cronEnabled || draft.cronSpec.trim().split(/\s+/).length >= 5;

    return [
      agentIdValid && draft.name.trim().length > 0,
      channelValid,
      promptPathValid,
      draft.provider.trim().length > 0,
      cronValid,
      agentIdValid && channelValid && promptPathValid && cronValid,
    ];
  }, [draft, mode]);

  const currentValid = validationByStep[stepIndex];
  const currentStep = steps[stepIndex];

  const buildSetupBody = useCallback((dryRun: boolean): api.AgentSetupRequest => ({
    agent_id: draft.agentId.trim(),
    channel_id: draft.channelId.trim(),
    provider: draft.provider,
    prompt_template_path: draft.promptTemplatePath.trim(),
    skills: parseSkills(draft.skillsText),
    dry_run: dryRun,
  }), [draft]);

  const buildDuplicateBody = useCallback((dryRun: boolean): api.DuplicateAgentRequest => ({
    new_agent_id: draft.agentId.trim(),
    channel_id: draft.channelId.trim(),
    provider: draft.provider,
    name: draft.name.trim(),
    name_ko: draft.nameKo.trim() || draft.name.trim(),
    department_id: draft.departmentId || null,
    skills: parseSkills(draft.skillsText),
    dry_run: dryRun,
  }), [draft]);

  const runPreview = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const result =
        mode === "duplicate" && sourceAgent
          ? await api.duplicateAgent(sourceAgent.id, buildDuplicateBody(true))
          : await api.setupAgent(buildSetupBody(true));
      setPreview(result);
    } catch (caught) {
      setPreview(null);
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setBusy(false);
    }
  }, [buildDuplicateBody, buildSetupBody, mode, sourceAgent]);

  const runConfirm = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      if (mode === "duplicate" && sourceAgent) {
        await api.duplicateAgent(sourceAgent.id, buildDuplicateBody(false));
        if (draft.promptContent.trim()) {
          await api.updateAgent(draft.agentId.trim(), {
            prompt_content: draft.promptContent,
            auto_commit: false,
          });
        }
      } else {
        await api.setupAgent(buildSetupBody(false));
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
    } finally {
      setBusy(false);
    }
  }, [buildDuplicateBody, buildSetupBody, draft, mode, onDone, sourceAgent]);

  if (!open) return null;

  const field = (
    label: string,
    value: string,
    onChange: (next: string) => void,
    placeholder?: string,
    type = "text",
  ) => (
    <label className="block">
      <span className="mb-1.5 block text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
        {label}
      </span>
      <input
        type={type}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        placeholder={placeholder}
        className={inputClass}
        style={inputStyle}
      />
    </label>
  );

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-hidden px-3 py-4 sm:items-center sm:p-4"
      style={{ background: "var(--th-modal-overlay)" }}
      onClick={(event) => {
        if (event.currentTarget === event.target) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={tr("에이전트 설정 마법사", "Agent setup wizard")}
        className="w-full max-w-5xl overflow-hidden rounded-[30px] border shadow-2xl"
        style={{
          maxHeight: "min(92vh, 860px)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        }}
      >
        <div className="flex items-center justify-between gap-3 border-b px-5 py-4" style={{ borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)" }}>
          <div className="min-w-0">
            <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
              {mode === "duplicate"
                ? tr("복제 설정", "Duplicate Setup")
                : tr("온보딩 설정", "Onboarding Setup")}
            </div>
            <h3 className="mt-1 text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {mode === "duplicate"
                ? tr("에이전트 복제", "Duplicate Agent")
                : tr("에이전트 생성", "Create Agent")}
            </h3>
          </div>
          <SurfaceActionButton onClick={onClose} tone="neutral" compact className="h-10 w-10" style={{ padding: 0 }} aria-label="Close">
            x
          </SurfaceActionButton>
        </div>

        <div className="grid min-h-0 grid-cols-1 md:grid-cols-[220px_minmax(0,1fr)]">
          <aside className="border-b p-4 md:border-b-0 md:border-r" style={{ borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)" }}>
            <div className="grid gap-2">
              {steps.map((step, index) => (
                <button
                  key={step}
                  type="button"
                  onClick={() => setStepIndex(index)}
                  className="flex items-center justify-between gap-3 rounded-2xl border px-3 py-2 text-left text-xs"
                  style={{
                    borderColor:
                      stepIndex === index
                        ? "color-mix(in srgb, var(--th-accent) 58%, var(--th-border))"
                        : "color-mix(in srgb, var(--th-border) 68%, transparent)",
                    background:
                      stepIndex === index
                        ? "color-mix(in srgb, var(--th-accent-primary-soft) 70%, var(--th-card-bg) 30%)"
                        : "color-mix(in srgb, var(--th-card-bg) 84%, transparent)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  <span>{index + 1}. {labelForStep(step, tr)}</span>
                  <span style={{ color: validationByStep[index] ? "var(--th-accent-primary)" : "var(--th-text-muted)" }}>
                    {validationByStep[index] ? "OK" : "--"}
                  </span>
                </button>
              ))}
            </div>
          </aside>

          <main className="min-h-0 overflow-y-auto p-5" style={{ maxHeight: "calc(min(92vh, 860px) - 73px)" }}>
            <div className="space-y-4">
              {currentStep === "role" && (
                <SurfaceSubsection
                  title={tr("역할 정의", "Role Definition")}
                  description={tr("role_id와 표시 이름은 생성 후 관리 화면에서 계속 편집할 수 있습니다.", "Role ID and display names remain editable after creation.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    {field("role_id", draft.agentId, (agentId) => setDraft({ ...draft, agentId }), "adk-researcher")}
                    {field(tr("표시 이름", "Display Name"), draft.name, (name) => setDraft({ ...draft, name }), "Researcher")}
                    {field(tr("한글 이름", "Korean Name"), draft.nameKo, (nameKo) => setDraft({ ...draft, nameKo }), "리서처")}
                    <label className="block">
                      <span className="mb-1.5 block text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                        {tr("부서", "Department")}
                      </span>
                      <select
                        value={draft.departmentId}
                        onChange={(event) => setDraft({ ...draft, departmentId: event.target.value })}
                        className={inputClass}
                        style={inputStyle}
                      >
                        <option value="">{tr("미배정", "Unassigned")}</option>
                        {departments.map((department) => (
                          <option key={department.id} value={department.id}>
                            {department.icon} {localeName(locale, department)}
                          </option>
                        ))}
                      </select>
                    </label>
                  </div>
                  {!validationByStep[0] && (
                    <p className="mt-3 text-xs" style={{ color: "var(--th-accent-danger)" }}>
                      {tr("role_id는 영문/숫자/_/- 2자 이상이고 이름이 필요합니다.", "role_id needs 2+ letters/numbers/_/- and a name.")}
                    </p>
                  )}
                </SurfaceSubsection>
              )}

              {currentStep === "discord" && (
                <SurfaceSubsection
                  title={tr("Discord 채널", "Discord Channel")}
                  description={tr("신규 역할이 연결될 채널 ID를 지정합니다.", "Choose the Discord channel ID for this role.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    {field("channel_id", draft.channelId, (channelId) => setDraft({ ...draft, channelId }), "123456789012345678")}
                    <label className="block">
                      <span className="mb-1.5 block text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                        Provider
                      </span>
                      <select
                        value={draft.provider}
                        onChange={(event) => setDraft({ ...draft, provider: event.target.value as CliProvider })}
                        className={inputClass}
                        style={inputStyle}
                      >
                        {CLI_PROVIDERS.map((provider) => (
                          <option key={provider} value={provider}>{provider}</option>
                        ))}
                      </select>
                    </label>
                  </div>
                  {!validationByStep[1] && (
                    <p className="mt-3 text-xs" style={{ color: "var(--th-accent-danger)" }}>
                      {tr("Discord channel_id는 숫자 ID여야 합니다.", "Discord channel_id must be a numeric ID.")}
                    </p>
                  )}
                </SurfaceSubsection>
              )}

              {currentStep === "prompt" && (
                <SurfaceSubsection
                  title={tr("Role prompt", "Role Prompt")}
                  description={tr("템플릿을 복사한 뒤, 필요하면 최종 프롬프트 파일을 다시 씁니다.", "The template is copied first, then the final prompt file can be rewritten.")}
                >
                  <div className="space-y-4">
                    {mode !== "duplicate" && field(
                      "prompt_template_path",
                      draft.promptTemplatePath,
                      (promptTemplatePath) => setDraft({ ...draft, promptTemplatePath }),
                      "~/.adk/release/config/agents/_shared.prompt.md",
                    )}
                    <AgentPromptEditor
                      label={tr("프롬프트 본문", "Prompt content")}
                      value={draft.promptContent}
                      onChange={(promptContent) => setDraft({ ...draft, promptContent })}
                      minHeight={320}
                    />
                  </div>
                </SurfaceSubsection>
              )}

              {currentStep === "workspace" && (
                <SurfaceSubsection
                  title={tr("Workspace + MCP", "Workspace + MCP")}
                  description={tr("스킬은 setup API에 그대로 전달됩니다.", "Skills are passed through to the setup API.")}
                >
                  <div className="grid gap-4 md:grid-cols-2">
                    <label className="block md:col-span-2">
                      <span className="mb-1.5 block text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                        {tr("스킬", "Skills")}
                      </span>
                      <textarea
                        value={draft.skillsText}
                        onChange={(event) => setDraft({ ...draft, skillsText: event.target.value })}
                        placeholder="github, playwright, memory-read"
                        rows={5}
                        className={`${inputClass} resize-y`}
                        style={inputStyle}
                      />
                    </label>
                    <SurfaceCard className="rounded-[24px] p-4">
                      <div className="text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                        {tr("작업공간", "Workspace")}
                      </div>
                      <div className="mt-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
                        ~/.adk/release/config/agents/{draft.agentId || "{role_id}"}
                      </div>
                    </SurfaceCard>
                    <SurfaceCard className="rounded-[24px] p-4">
                      <div className="text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                        MCP
                      </div>
                      <div className="mt-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
                        {parseSkills(draft.skillsText).length || tr("기본값", "Default")}
                      </div>
                    </SurfaceCard>
                  </div>
                </SurfaceSubsection>
              )}

              {currentStep === "cron" && (
                <SurfaceSubsection
                  title={tr("Cron", "Cron")}
                  description={tr("선택 항목입니다. 일정 연결은 관리 화면에서 이어서 조정합니다.", "Optional. Schedule wiring can be adjusted from management later.")}
                >
                  <div className="space-y-4">
                    <label className="flex items-center gap-3 text-sm" style={{ color: "var(--th-text-primary)" }}>
                      <input
                        type="checkbox"
                        checked={draft.cronEnabled}
                        onChange={(event) => setDraft({ ...draft, cronEnabled: event.target.checked })}
                      />
                      {tr("Cron 초안 포함", "Include cron draft")}
                    </label>
                    {draft.cronEnabled && field("cron", draft.cronSpec, (cronSpec) => setDraft({ ...draft, cronSpec }), "0 9 * * 1-5")}
                    {!validationByStep[4] && (
                      <p className="text-xs" style={{ color: "var(--th-accent-danger)" }}>
                        {tr("Cron 표현식은 최소 5개 필드가 필요합니다.", "Cron expression needs at least 5 fields.")}
                      </p>
                    )}
                  </div>
                </SurfaceSubsection>
              )}

              {currentStep === "preview" && (
                <SurfaceSubsection
                  title={tr("Preview + Confirm", "Preview + Confirm")}
                  description={tr("dry-run으로 파일/설정 변경을 확인한 뒤 생성합니다. 실패 시 setup rollback 결과를 확인하고 같은 화면에서 재시도합니다.", "Dry-run the file/config changes before creation. On failure, review rollback output and retry here.")}
                >
                  <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(280px,0.8fr)]">
                    <SurfaceCard className="rounded-[24px] p-4">
                      <div className="grid gap-2 text-xs" style={{ color: "var(--th-text-secondary)" }}>
                        <div>role_id: <span style={{ color: "var(--th-text-primary)" }}>{draft.agentId}</span></div>
                        <div>channel_id: <span style={{ color: "var(--th-text-primary)" }}>{draft.channelId || "--"}</span></div>
                        <div>provider: <span style={{ color: "var(--th-text-primary)" }}>{draft.provider}</span></div>
                        <div>prompt: <span style={{ color: "var(--th-text-primary)" }}>{mode === "duplicate" ? tr("원본 복사", "copy source") : draft.promptTemplatePath}</span></div>
                        <div>skills: <span style={{ color: "var(--th-text-primary)" }}>{parseSkills(draft.skillsText).join(", ") || "--"}</span></div>
                      </div>
                    </SurfaceCard>
                    <SurfaceCard className="max-h-80 overflow-auto rounded-[24px] p-4">
                      <pre className="whitespace-pre-wrap text-xs leading-5" style={{ color: "var(--th-text-secondary)" }}>
                        {preview ? JSON.stringify(preview, null, 2) : tr("아직 preview가 없습니다.", "No preview yet.")}
                      </pre>
                    </SurfaceCard>
                  </div>
                </SurfaceSubsection>
              )}

              {error && (
                <SurfaceNotice tone="danger">
                  <span className="text-sm">{error}</span>
                </SurfaceNotice>
              )}
            </div>
          </main>
        </div>

        <div className="flex flex-col gap-2 border-t px-5 py-4 sm:flex-row sm:items-center sm:justify-between" style={{ borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)" }}>
          <div className="flex flex-wrap gap-2">
            {steps.map((step, index) => (
              <SurfaceSegmentButton
                key={step}
                active={stepIndex === index}
                tone={validationByStep[index] ? "success" : "neutral"}
                onClick={() => setStepIndex(index)}
              >
                {index + 1}
              </SurfaceSegmentButton>
            ))}
          </div>
          <div className="flex flex-wrap justify-end gap-2">
            <SurfaceActionButton tone="neutral" onClick={onClose} disabled={busy}>
              {tr("취소", "Cancel")}
            </SurfaceActionButton>
            <SurfaceActionButton
              tone="neutral"
              onClick={() => setStepIndex((prev) => Math.max(0, prev - 1))}
              disabled={busy || stepIndex === 0}
            >
              {tr("이전", "Back")}
            </SurfaceActionButton>
            {stepIndex < steps.length - 1 ? (
              <SurfaceActionButton
                onClick={() => setStepIndex((prev) => Math.min(steps.length - 1, prev + 1))}
                disabled={!currentValid || busy}
              >
                {tr("다음", "Next")}
              </SurfaceActionButton>
            ) : (
              <>
                <SurfaceActionButton tone="info" onClick={runPreview} disabled={!currentValid || busy}>
                  {busy ? tr("확인 중...", "Checking...") : tr("Preview", "Preview")}
                </SurfaceActionButton>
                <SurfaceActionButton tone="accent" onClick={runConfirm} disabled={!currentValid || busy}>
                  {busy ? tr("처리 중...", "Working...") : tr("Confirm", "Confirm")}
                </SurfaceActionButton>
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
