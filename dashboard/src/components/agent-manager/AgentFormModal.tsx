import { createPortal } from "react-dom";
import { zodResolver } from "@hookform/resolvers/zod";
import { useEffect, useRef } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import { catalogLabel, useProviderCatalog } from "../../api/providers";
import type { Department } from "../../types";
import { localeName, useI18n } from "../../i18n";
import AgentPersona from "../AgentPersona";
import EmojiPicker from "./EmojiPicker";
import AgentPromptEditor from "./AgentPromptEditor";
import type { FormData } from "./types";
import { useFocusTrap } from "../common/overlay/useFocusTrap";
import { useReturnFocus } from "../common/overlay/useReturnFocus";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";

const agentFormSchema = z.object({
  name: z.string().trim().min(1, "required"),
  name_ko: z.string(),
  name_ja: z.string(),
  name_zh: z.string(),
  department_id: z.string(),
  cli_provider: z.string().trim().min(1, "required"),
  avatar_emoji: z.string().trim().min(1, "required"),
  personality: z.string(),
  prompt_content: z.string(),
  auto_commit: z.boolean(),
});

type AgentFormValues = z.infer<typeof agentFormSchema>;

export default function AgentFormModal({
  isKo,
  locale,
  tr,
  form,
  departments,
  isEdit,
  saving,
  onSave,
  onClose,
}: {
  isKo: boolean;
  locale: string;
  tr: (ko: string, en: string) => string;
  form: FormData;
  departments: Department[];
  isEdit: boolean;
  saving: boolean;
  onSave: (values: FormData) => void | Promise<void>;
  onClose: () => void;
}) {
  const { t } = useI18n();
  const overlayRef = useFocusTrap(true);
  useReturnFocus(true);
  const {
    register,
    handleSubmit,
    reset,
    setValue,
    watch,
    formState: { errors },
  } = useForm<AgentFormValues>({
    resolver: zodResolver(agentFormSchema),
    defaultValues: form,
    mode: "onChange",
  });
  const formValues = watch();
  const providerCatalog = useProviderCatalog(formValues.cli_provider);

  // ESC 키로 닫기
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  useEffect(() => {
    reset(form);
  }, [form, reset]);

  const inputCls =
    "w-full min-h-11 px-3 py-2 border rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/40 focus:border-blue-500 transition-colors";
  const inputStyle = {
    background: "var(--th-input-bg)",
    borderColor: "var(--th-input-border)",
    color: "var(--th-text-primary)",
  };
  const handleSave = handleSubmit(async (values) => {
    await onSave({
      ...values,
      cli_provider: values.cli_provider as FormData["cli_provider"],
    });
  });

  return createPortal(
    <>
    <div aria-hidden="true" className="fixed inset-0 z-[70]" style={{ background: "var(--th-modal-overlay)" }} />
    <div
      ref={overlayRef}
      className="fixed inset-0 z-[80] flex items-start justify-center overflow-hidden px-3 py-4 sm:items-center sm:p-4"
      style={{
        paddingTop: "max(1rem, calc(env(safe-area-inset-top) + 0.75rem))",
        paddingBottom: "max(1rem, calc(env(safe-area-inset-bottom) + 0.75rem))",
      }}
      onClick={(e) => {
        if (e.target === overlayRef.current) onClose();
      }}
    >
      <form
        role="dialog"
        aria-modal="true"
        aria-labelledby="agent-modal-title"
        onSubmit={handleSave}
        className="w-full self-start max-h-[calc(100dvh-env(safe-area-inset-top)-env(safe-area-inset-bottom)-1.5rem)] max-w-[calc(100vw-1.5rem)] overflow-x-hidden overflow-y-auto overscroll-contain rounded-lg border p-4 shadow-2xl animate-in fade-in zoom-in-95 duration-200 sm:my-auto sm:max-h-[90vh] sm:max-w-5xl sm:p-6"
        style={{
          background:
            "var(--th-bg-surface)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
          paddingBottom: "max(1.25rem, calc(1.25rem + env(safe-area-inset-bottom)))",
          WebkitOverflowScrolling: "touch",
          touchAction: "pan-y",
        }}
      >
        {/* Modal header */}
        <div className="flex items-center justify-between mb-5">
          <h3 id="agent-modal-title" className="text-base font-bold" style={{ color: "var(--th-text-heading)" }}>
            {isEdit ? tr("에이전트 수정", "Edit agent") : tr("새 에이전트", "New agent")}
          </h3>
          <SurfaceActionButton onClick={onClose} tone="neutral" compact className="h-11 w-11" style={{ padding: 0 }} aria-label={tr("닫기", "Close")}>
            ✕
          </SurfaceActionButton>
        </div>

        {/* 2-column layout */}
        <div className="grid grid-cols-1 gap-5 md:grid-cols-2">
          {/* ── Left column: 기본 정보 ── */}
          <SurfaceSubsection
            className="min-w-0"
            title={tr("기본 정보", "Basic Info")}
            description={tr("이름, 이모지, 부서를 먼저 설정합니다.", "Set the identity, emoji, and department first.")}
          >
            <div className="space-y-4">
            <div className="flex min-h-11 items-start gap-3">
              <AgentPersona
                agent={{
                  id: formValues.name || "new-agent",
                  name: formValues.name || "Agent",
                  avatar_emoji: formValues.avatar_emoji,
                  department_color: departments.find((d) => d.id === formValues.department_id)?.color ?? null,
                }}
                state="idle"
                size={48}
              />
              <div className="flex-1 min-w-0">
                <div>
                  <label
                    htmlFor="agent-name"
                    className="block text-xs mb-1.5 font-medium"
                    style={{ color: "var(--th-text-secondary)" }}
                  >
                    {tr("영문 이름", "Name")} <span className="text-red-400">*</span>
                  </label>
                  <input
                    id="agent-name"
                    type="text"
                    {...register("name")}
                    placeholder="DORO"
                    aria-invalid={errors.name ? "true" : "false"}
                    aria-describedby={errors.name ? "agent-name-error" : undefined}
                    className={inputCls}
                    style={inputStyle}
                  />
                  {errors.name && (
                    <p id="agent-name-error" className="mt-1 text-xs text-red-400">
                      {tr("영문 이름을 입력해주세요.", "Enter an agent name.")}
                    </p>
                  )}
                </div>
              </div>
            </div>
            {/* 로캘 기반 현지 이름 필드 */}
            {locale.startsWith("ko") && (
              <div>
                <label htmlFor="agent-name-ko" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {tr("한글 이름", "Korean Name")}
                </label>
                <input
                  id="agent-name-ko"
                  type="text"
                  {...register("name_ko")}
                  placeholder="도로롱"
                  className={inputCls}
                  style={inputStyle}
                />
              </div>
            )}
            {locale.startsWith("ja") && (
              <div>
                <label htmlFor="agent-name-ja" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {t({ ko: "일본어 이름", en: "Japanese Name", ja: "日本語名", zh: "日语名" })}
                </label>
                <input
                  id="agent-name-ja"
                  type="text"
                  {...register("name_ja")}
                  placeholder="ドロロン"
                  className={inputCls}
                  style={inputStyle}
                />
              </div>
            )}
            {locale.startsWith("zh") && (
              <div>
                <label htmlFor="agent-name-zh" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {t({ ko: "중국어 이름", en: "Chinese Name", ja: "中国語名", zh: "中文名" })}
                </label>
                <input
                  id="agent-name-zh"
                  type="text"
                  {...register("name_zh")}
                  placeholder="多罗隆"
                  className={inputCls}
                  style={inputStyle}
                />
              </div>
            )}
            <div className="grid grid-cols-[72px_minmax(0,1fr)] gap-2 md:grid-cols-[72px_minmax(0,1fr)_minmax(0,1fr)]">
              <div>
                <label htmlFor="agent-emoji" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {tr("아이콘", "Icon")}
                </label>
                <EmojiPicker
                  id="agent-emoji"
                  value={formValues.avatar_emoji}
                  onChange={(emoji) => setValue("avatar_emoji", emoji, { shouldDirty: true, shouldValidate: true })}
                />
              </div>
              <div>
                <label htmlFor="agent-department" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {tr("소속 부서", "Department")}
                </label>
                <select
                  id="agent-department"
                  {...register("department_id")}
                  className={`${inputCls} cursor-pointer`}
                  style={inputStyle}
                >
                  <option value="">{tr("— 미배정 —", "— Unassigned —")}</option>
                  {departments.map((d) => (
                    <option key={d.id} value={d.id}>
                      {d.icon} {localeName(locale, d)}
                    </option>
                  ))}
                </select>
              </div>
              <div className="col-span-2 md:col-span-1">
                <label htmlFor="agent-provider" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {tr("메인 Provider", "Main Provider")}
                </label>
                <select
                  id="agent-provider"
                  {...register("cli_provider")}
                  disabled={providerCatalog.loading}
                  className={`${inputCls} cursor-pointer`}
                  style={inputStyle}
                >
                  {providerCatalog.selectableIds.map((provider) => (
                    <option key={provider} value={provider}>
                      {catalogLabel(providerCatalog.entries, provider)}
                    </option>
                  ))}
                </select>
                {providerCatalog.loading ? (
                  <span className="mt-1 block text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {tr("프로바이더 목록을 불러오는 중...", "Loading providers...")}
                  </span>
                ) : providerCatalog.error ? (
                  <span className="mt-1 block text-[11px]" style={{ color: "var(--th-accent-danger)" }}>
                    {tr("카탈로그를 못 불러 로컬 목록을 씁니다.", "Catalog unavailable; using local list.")}
                  </span>
                ) : null}
              </div>
            </div>
            </div>
          </SurfaceSubsection>

          {/* ── Right column ── */}
          <SurfaceSubsection
            className="min-w-0"
            title={tr("추가 정보", "Details")}
            description={tr("프롬프트와 성격을 정리합니다.", "Describe the agent personality and prompt.")}
          >
            <div className="space-y-4">
            {/* 성격/프롬프트 */}
            <div>
              <label htmlFor="agent-personality" className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                {tr("성격 / 역할 프롬프트", "Personality / Prompt")}
              </label>
              <textarea
                id="agent-personality"
                {...register("personality")}
                rows={6}
                placeholder={tr("전문 분야나 성격 설명...", "Expertise or personality...")}
                className={`${inputCls} resize-none`}
                style={inputStyle}
              />
            </div>
            {isEdit && (
              <div>
                <div className="mb-1.5 flex flex-wrap items-center justify-between gap-2">
                  <label className="text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                    prompt.md
                  </label>
                  <label className="flex items-center gap-2 text-xs" style={{ color: "var(--th-text-muted)" }}>
                    <input
                      type="checkbox"
                      {...register("auto_commit")}
                    />
                    {tr("자동 커밋", "Auto commit")}
                  </label>
                </div>
                <AgentPromptEditor
                  label="prompt.md"
                  value={formValues.prompt_content}
                  onChange={(prompt_content) => setValue("prompt_content", prompt_content, { shouldDirty: true })}
                  minHeight={300}
                />
              </div>
            )}
            </div>
          </SurfaceSubsection>
        </div>
        {/* Actions — full width */}
        <div
          className="mt-5 flex flex-col gap-2 pt-4 sm:flex-row"
          style={{ borderTop: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)" }}
        >
          <SurfaceActionButton
            type="submit"
            disabled={saving || !formValues.name.trim()}
            tone="accent"
            className="flex-1 text-sm"
          >
            {saving
              ? tr("처리 중...", "Saving...")
              : isEdit
                ? tr("변경사항 저장", "Save Changes")
                : tr("채용 확정", "Confirm Hire")}
          </SurfaceActionButton>
          <SurfaceActionButton
            onClick={onClose}
            tone="neutral"
            className="text-sm sm:self-auto"
          >
            {tr("취소", "Cancel")}
          </SurfaceActionButton>
        </div>
      </form>
    </div>
    </>,
    document.body,
  );
}
