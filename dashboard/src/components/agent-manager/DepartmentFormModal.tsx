import { zodResolver } from "@hookform/resolvers/zod";
import { useEffect, useRef, useState } from "react";
import { useForm } from "react-hook-form";
import { z } from "zod";
import type { Department } from "../../types";
import { useI18n } from "../../i18n";
import * as api from "../../api";
import { DEPT_BLANK, DEPT_COLORS } from "./constants";
import EmojiPicker from "./EmojiPicker";
import type { Translator } from "./types";
import {
  SurfaceActionButton,
  SurfaceNotice,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";

const departmentFormSchema = z.object({
  id: z.string(),
  name: z.string().trim().min(1, "required"),
  name_ko: z.string(),
  name_ja: z.string(),
  name_zh: z.string(),
  icon: z.string().trim().min(1, "required"),
  color: z.string().trim().min(1, "required"),
  description: z.string(),
  prompt: z.string(),
});

type DepartmentFormValues = z.infer<typeof departmentFormSchema>;

function getDepartmentFormDefaults(department: Department | null): DepartmentFormValues {
  if (department) {
    return {
      id: department.id,
      name: department.name,
      name_ko: department.name_ko || "",
      name_ja: department.name_ja || "",
      name_zh: department.name_zh || "",
      icon: department.icon,
      color: department.color,
      description: department.description || "",
      prompt: department.prompt || "",
    };
  }
  return { ...DEPT_BLANK };
}

export default function DepartmentFormModal({
  locale,
  tr,
  department,
  departments,
  officeId,
  onSave,
  onClose,
  onSaveDepartment,
  onDeleteDepartment,
}: {
  locale: string;
  tr: Translator;
  department: Department | null;
  departments: Department[];
  officeId?: string | null;
  onSave: () => void;
  onClose: () => void;
  onSaveDepartment?: (input: {
    mode: "create" | "update";
    id: string;
    payload: {
      name: string;
      name_ko: string;
      name_ja: string | null;
      name_zh: string | null;
      icon: string;
      color: string;
      description: string | null;
      prompt: string | null;
      sort_order: number;
    };
  }) => Promise<void>;
  onDeleteDepartment?: (departmentId: string) => Promise<void>;
}) {
  const { t } = useI18n();
  const isEdit = !!department;
  const {
    register,
    handleSubmit,
    setValue,
    watch,
    formState: { errors },
  } = useForm<DepartmentFormValues>({
    resolver: zodResolver(departmentFormSchema),
    defaultValues: getDepartmentFormDefaults(department),
    mode: "onChange",
  });
  const form = watch();
  const [saving, setSaving] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const overlayRef = useRef<HTMLDivElement>(null);

  // sort_order 기반 다음 순번 계산
  const nextSortOrder = (() => {
    const orders = departments.map((d) => d.sort_order).filter((n) => typeof n === "number" && !isNaN(n));
    return Math.max(0, ...orders) + 1;
  })();

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const handleSave = handleSubmit(async (values) => {
    setSaving(true);
    try {
      const payload = {
        name: values.name,
        name_ko: values.name_ko.trim(),
        name_ja: values.name_ja.trim() || null,
        name_zh: values.name_zh.trim() || null,
        icon: values.icon,
        color: values.color,
        description: values.description.trim() || null,
        prompt: values.prompt.trim() || null,
        sort_order: department?.sort_order ?? nextSortOrder,
      };
      if (isEdit) {
        if (onSaveDepartment) {
          await onSaveDepartment({
            mode: "update",
            id: department!.id,
            payload: { ...payload, sort_order: department!.sort_order },
          });
        } else {
          await api.updateDepartment(department!.id, {
            name: payload.name,
            name_ko: payload.name_ko,
            name_ja: payload.name_ja,
            name_zh: payload.name_zh,
            icon: payload.icon,
            color: payload.color,
            description: payload.description,
            prompt: payload.prompt,
          });
        }
      } else {
        // name 기반 slug 생성, 비라틴 문자만인 경우 dept-N fallback
        const slug = values.name
          .toLowerCase()
          .replace(/[^a-z0-9]+/g, "-")
          .replace(/^-+|-+$/g, "");
        let deptId = slug || `dept-${nextSortOrder}`;
        // 기존 ID와 충돌 시 숫자 접미사 추가
        const existingIds = new Set(departments.map((d) => d.id));
        let suffix = 2;
        while (existingIds.has(deptId)) {
          deptId = `${slug || "dept"}-${suffix++}`;
        }
        if (onSaveDepartment) {
          await onSaveDepartment({
            mode: "create",
            id: deptId,
            payload: { ...payload, sort_order: nextSortOrder },
          });
        } else {
          await api.createDepartment({
            id: deptId,
            name: payload.name,
            name_ko: payload.name_ko,
            name_ja: payload.name_ja ?? "",
            name_zh: payload.name_zh ?? "",
            icon: payload.icon,
            color: payload.color,
            description: payload.description ?? undefined,
            prompt: payload.prompt ?? undefined,
            office_id: officeId ?? undefined,
          } as Partial<Department> & { office_id?: string });
        }
      }
      onSave();
      onClose();
    } catch (e: any) {
      console.error("Dept save failed:", e);
      if (api.isApiRequestError(e) && e.code === "department_id_exists") {
        alert(tr("이미 존재하는 부서 ID입니다.", "Department ID already exists."));
      } else if (api.isApiRequestError(e) && e.code === "sort_order_conflict") {
        alert(
          tr(
            "부서 정렬 순서가 충돌합니다. 잠시 후 다시 시도해주세요.",
            "Department sort order conflict. Please retry.",
          ),
        );
      }
    } finally {
      setSaving(false);
    }
  });

  const handleDelete = async () => {
    setSaving(true);
    try {
      if (onDeleteDepartment) {
        await onDeleteDepartment(department!.id);
      } else {
        await api.deleteDepartment(department!.id);
      }
      onSave();
      onClose();
    } catch (e: any) {
      console.error("Dept delete failed:", e);
      if (api.isApiRequestError(e) && e.code === "department_has_agents") {
        alert(tr("소속 직원이 있어 삭제할 수 없습니다.", "Cannot delete: department has agents."));
      } else if (api.isApiRequestError(e) && e.code === "department_has_tasks") {
        alert(tr("연결된 업무(Task)가 있어 삭제할 수 없습니다.", "Cannot delete: department has tasks."));
      } else if (api.isApiRequestError(e) && e.code === "department_protected") {
        alert(tr("기본 시스템 부서는 삭제할 수 없습니다.", "Cannot delete: protected system department."));
      }
    } finally {
      setSaving(false);
    }
  };

  const inputCls =
    "w-full px-3 py-2 border rounded-lg text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/40 focus:border-blue-500 transition-colors";
  const inputStyle = {
    background: "var(--th-input-bg)",
    borderColor: "var(--th-input-border)",
    color: "var(--th-text-primary)",
  };

  return (
    <div
      ref={overlayRef}
      className="fixed inset-0 z-50 flex items-end justify-center p-0 sm:items-center sm:p-4"
      style={{
        background: "var(--th-modal-overlay)",
        paddingTop: "calc(1rem + env(safe-area-inset-top))",
      }}
      onClick={(e) => {
        if (e.target === overlayRef.current) onClose();
      }}
    >
      <form
        role="dialog"
        aria-modal="true"
        aria-label={isEdit ? tr("부서 정보 수정", "Edit Department") : tr("신규 부서 추가", "Add Department")}
        className="w-full max-w-2xl max-h-full overflow-y-auto rounded-t-3xl p-5 shadow-2xl animate-in fade-in zoom-in-95 duration-200 sm:max-h-[85vh] sm:rounded-[28px] sm:p-6"
        style={{
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
          paddingBottom: "max(1.25rem, calc(1.25rem + env(safe-area-inset-bottom)))",
        }}
      >
        {/* Header */}
        <div className="flex items-center justify-between mb-5">
          <h3 className="text-base font-bold flex items-center gap-2" style={{ color: "var(--th-text-heading)" }}>
            <span className="text-lg">{form.icon}</span>
            {isEdit ? tr("부서 정보 수정", "Edit Department") : tr("신규 부서 추가", "Add Department")}
          </h3>
          <SurfaceActionButton
            onClick={onClose}
            tone="neutral"
            compact
            className="h-11 w-11"
            style={{ padding: 0 }}
          >
            ✕
          </SurfaceActionButton>
        </div>

        <div className="space-y-4">
          <SurfaceSubsection
            title={tr("기본 정보", "Identity")}
            description={tr("부서 이름과 시각 표현을 먼저 정리합니다.", "Set the department identity and visual accent first.")}
          >
            <div className="space-y-4">
              <div className="flex items-start gap-3">
                <div>
                  <label className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                    {tr("아이콘", "Icon")}
                  </label>
                  <EmojiPicker
                    value={form.icon}
                    onChange={(emoji) => setValue("icon", emoji, { shouldDirty: true, shouldValidate: true })}
                  />
                </div>
                <div className="flex-1">
                  <label
                    htmlFor="department-name"
                    className="block text-xs mb-1.5 font-medium"
                    style={{ color: "var(--th-text-secondary)" }}
                  >
                    {tr("영문 이름", "Name")} <span className="text-red-400">*</span>
                  </label>
                  <input
                    id="department-name"
                    type="text"
                    {...register("name")}
                    placeholder="Development"
                    aria-invalid={errors.name ? "true" : "false"}
                    aria-describedby={errors.name ? "department-name-error" : undefined}
                    className={inputCls}
                    style={inputStyle}
                  />
                  {errors.name && (
                    <p id="department-name-error" className="mt-1 text-xs text-red-400">
                      {tr("영문 이름을 입력해주세요.", "Enter a department name.")}
                    </p>
                  )}
                </div>
              </div>

              <div>
                <label className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {tr("테마 색상", "Theme Color")}
                </label>
                <div className="flex gap-2 flex-wrap">
                  {DEPT_COLORS.map((c) => (
                    <button
                      key={c}
                      type="button"
                      aria-label={`Color ${c}`}
                      aria-pressed={form.color === c}
                      onClick={() => setValue("color", c, { shouldDirty: true, shouldValidate: true })}
                      className="w-11 h-11 rounded-full transition-all hover:scale-110"
                      style={{
                        background: c,
                        outline: form.color === c ? `2px solid ${c}` : "2px solid transparent",
                        outlineOffset: "3px",
                      }}
                    />
                  ))}
                </div>
              </div>

              {locale.startsWith("ko") && (
                <div>
                  <label className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                    {tr("한글 이름", "Korean Name")}
                  </label>
                  <input
                    type="text"
                    {...register("name_ko")}
                    placeholder="개발팀"
                    className={inputCls}
                    style={inputStyle}
                  />
                </div>
              )}
              {locale.startsWith("ja") && (
                <div>
                  <label className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                    {t({ ko: "일본어 이름", en: "Japanese Name", ja: "日本語名", zh: "日语名" })}
                  </label>
                  <input
                    type="text"
                    {...register("name_ja")}
                    placeholder="開発チーム"
                    className={inputCls}
                    style={inputStyle}
                  />
                </div>
              )}
              {locale.startsWith("zh") && (
                <div>
                  <label className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                    {t({ ko: "중국어 이름", en: "Chinese Name", ja: "中国語名", zh: "中文名" })}
                  </label>
                  <input
                    type="text"
                    {...register("name_zh")}
                    placeholder="开发部"
                    className={inputCls}
                    style={inputStyle}
                  />
                </div>
              )}

              <div>
                <label className="block text-xs mb-1.5 font-medium" style={{ color: "var(--th-text-secondary)" }}>
                  {tr("부서 설명", "Description")}
                </label>
                <input
                  type="text"
                  {...register("description")}
                  placeholder={tr("부서의 역할 간단 설명", "Brief description of the department")}
                  className={inputCls}
                  style={inputStyle}
                />
              </div>
            </div>
          </SurfaceSubsection>

          <SurfaceSubsection
            title={tr("운영 프롬프트", "Department Prompt")}
            description={tr("소속 에이전트가 공통으로 따르는 부서 지침을 적습니다.", "Write the shared instruction applied to agents in this department.")}
          >
            <div className="space-y-3">
              <textarea
                {...register("prompt")}
                rows={4}
                placeholder={tr(
                  "이 부서 소속 에이전트의 공통 시스템 프롬프트...",
                  "Shared system prompt for agents in this department...",
                )}
                className={`${inputCls} resize-none`}
                style={inputStyle}
              />
              <SurfaceNotice tone="neutral" compact>
                {tr(
                  "소속 에이전트의 작업 실행 시 공통으로 적용되는 시스템 프롬프트",
                  "Applied as shared system prompt when agents in this department execute tasks",
                )}
              </SurfaceNotice>
            </div>
          </SurfaceSubsection>
        </div>

        {/* Actions */}
        <div className="flex items-center gap-2 mt-5 pt-4" style={{ borderTop: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)" }}>
          <SurfaceActionButton
            type="submit"
            disabled={saving || !form.name.trim()}
            tone="accent"
            className="flex-1 text-sm"
          >
            {saving
              ? tr("처리 중...", "Saving...")
              : isEdit
                ? tr("변경사항 저장", "Save Changes")
                : tr("부서 추가", "Add Department")}
          </SurfaceActionButton>
          {isEdit &&
            (confirmDelete ? (
              <div className="flex items-center gap-1">
                <SurfaceActionButton
                  onClick={handleDelete}
                  disabled={saving}
                  tone="danger"
                >
                  {tr("삭제 확인", "Confirm")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => setConfirmDelete(false)}
                  tone="neutral"
                >
                  {tr("취소", "No")}
                </SurfaceActionButton>
              </div>
            ) : (
              <SurfaceActionButton
                onClick={() => setConfirmDelete(true)}
                tone="danger"
                className="text-sm"
              >
                {tr("삭제", "Delete")}
              </SurfaceActionButton>
            ))}
          <SurfaceActionButton
            onClick={onClose}
            tone="neutral"
            className="text-sm"
          >
            {tr("취소", "Cancel")}
          </SurfaceActionButton>
        </div>
      </form>
    </div>
  );
}
