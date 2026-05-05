import { useState, useCallback } from "react";
import { X, Plus, Trash2, UserPlus, UserMinus, Settings2 } from "lucide-react";
import type { Office, Agent } from "../types";
import * as api from "../api/client";
import AgentAvatar from "./AgentAvatar";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceNotice,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";

interface OfficeManagerModalProps {
  offices: Office[];
  allAgents: Agent[];
  isKo: boolean;
  onClose: () => void;
  onChanged: () => void;
}

type ModalView = "list" | "edit" | "agents";

const OFFICE_ICONS = ["🏢", "🏠", "🏭", "🏗️", "🏛️", "🍳", "🎮", "📚", "🔬", "🎨", "🛠️", "🌐"];
const OFFICE_COLORS = [
  "#10b981", "#14b8a6", "#06b6d4", "#3b82f6", "#84cc16",
  "#f59e0b", "#f97316", "#ef4444", "#64748b", "#22c55e",
];

export default function OfficeManagerModal({
  offices,
  allAgents,
  isKo,
  onClose,
  onChanged,
}: OfficeManagerModalProps) {
  const [view, setView] = useState<ModalView>("list");
  const [editOffice, setEditOffice] = useState<Office | null>(null);
  const [agentsOffice, setAgentsOffice] = useState<Office | null>(null);
  const [officeAgentIds, setOfficeAgentIds] = useState<Set<string>>(new Set());
  const [saving, setSaving] = useState(false);

  // Form state
  const [formName, setFormName] = useState("");
  const [formNameKo, setFormNameKo] = useState("");
  const [formIcon, setFormIcon] = useState("🏢");
  const [formColor, setFormColor] = useState("#10b981");
  const [formDesc, setFormDesc] = useState("");

  const tr = useCallback(
    (ko: string, en: string) => (isKo ? ko : en),
    [isKo],
  );

  const openCreate = () => {
    setEditOffice(null);
    setFormName("");
    setFormNameKo("");
    setFormIcon("🏢");
    setFormColor("#10b981");
    setFormDesc("");
    setView("edit");
  };

  const openEdit = (o: Office) => {
    setEditOffice(o);
    setFormName(o.name);
    setFormNameKo(o.name_ko);
    setFormIcon(o.icon);
    setFormColor(o.color);
    setFormDesc(o.description ?? "");
    setView("edit");
  };

  const openAgents = async (o: Office) => {
    setAgentsOffice(o);
    try {
      const agents = await api.getAgents(o.id);
      setOfficeAgentIds(new Set(agents.map((a) => a.id)));
    } catch {
      setOfficeAgentIds(new Set());
    }
    setView("agents");
  };

  const handleSave = async () => {
    setSaving(true);
    try {
      const payload = {
        name: formName.trim(),
        name_ko: formNameKo.trim() || formName.trim(),
        icon: formIcon,
        color: formColor,
        description: formDesc.trim() || null,
      };
      if (editOffice) {
        await api.updateOffice(editOffice.id, payload);
      } else {
        await api.createOffice(payload);
      }
      onChanged();
      setView("list");
    } catch (e) {
      console.error("Office save failed:", e);
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (id: string) => {
    if (!confirm(tr("이 오피스를 삭제하시겠습니까?", "Delete this office?")))
      return;
    setSaving(true);
    try {
      await api.deleteOffice(id);
      onChanged();
    } catch (e) {
      console.error("Office delete failed:", e);
    } finally {
      setSaving(false);
    }
  };

  const toggleAgent = async (agentId: string) => {
    if (!agentsOffice) return;
    setSaving(true);
    try {
      if (officeAgentIds.has(agentId)) {
        await api.removeAgentFromOffice(agentsOffice.id, agentId);
        setOfficeAgentIds((prev) => {
          const next = new Set(prev);
          next.delete(agentId);
          return next;
        });
      } else {
        await api.addAgentToOffice(agentsOffice.id, agentId);
        setOfficeAgentIds((prev) => new Set(prev).add(agentId));
      }
      onChanged();
    } catch (e) {
      console.error("Toggle agent failed:", e);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      style={{ background: "var(--th-modal-overlay)" }}
      onClick={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={tr("오피스 관리", "Manage Offices")}
        className="mx-4 flex max-h-[84vh] w-full max-w-2xl flex-col rounded-[28px] border"
        style={{
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
          borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        }}
      >
        {/* Header */}
        <div
          className="flex items-center justify-between p-4"
          style={{ borderBottom: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)" }}
        >
          <h2
            className="text-lg font-bold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {view === "list" && tr("오피스 관리", "Manage Offices")}
            {view === "edit" &&
              (editOffice
                ? tr("오피스 편집", "Edit Office")
                : tr("새 오피스", "New Office"))}
            {view === "agents" &&
              `${agentsOffice?.icon ?? ""} ${isKo ? agentsOffice?.name_ko : agentsOffice?.name} — ${tr("멤버 관리", "Manage Members")}`}
          </h2>
          <SurfaceActionButton tone="neutral" compact onClick={onClose}>
            <X size={16} />
          </SurfaceActionButton>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-4">
          {/* ── LIST VIEW ── */}
          {view === "list" && (
            <div className="space-y-2">
              <SurfaceNotice tone="info">
                <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                  {tr("오피스 목록", "Office List")}
                </div>
                <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {tr("오피스 프로필 편집과 멤버 관리를 여기서 바로 이동합니다.", "Move between office profile editing and member management from here.")}
                </div>
              </SurfaceNotice>
              {offices.map((o) => (
                <SurfaceCard
                  key={o.id}
                  className="flex items-center gap-3 p-3 rounded-lg"
                  style={{
                    background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                    border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
                  }}
                >
                  <span className="text-xl">{o.icon}</span>
                  <div className="flex-1 min-w-0">
                    <div
                      className="font-medium text-sm truncate"
                      style={{ color: "var(--th-text-primary)" }}
                    >
                      {isKo ? o.name_ko || o.name : o.name}
                    </div>
                    <div
                      className="text-xs"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {o.agent_count ?? 0} {tr("명", "agents")} · {o.department_count ?? 0} {tr("부서", "depts")}
                    </div>
                  </div>
                  <div className="flex items-center gap-1">
                    <SurfaceActionButton
                      onClick={() => openAgents(o)}
                      tone="neutral"
                      compact
                      title={tr("멤버 관리", "Manage Members")}
                    >
                      <Settings2
                        size={14}
                        style={{ color: "var(--th-text-secondary)" }}
                      />
                    </SurfaceActionButton>
                    <SurfaceActionButton
                      onClick={() => openEdit(o)}
                      tone="neutral"
                      compact
                    >
                      {tr("편집", "Edit")}
                    </SurfaceActionButton>
                    <SurfaceActionButton
                      onClick={() => handleDelete(o.id)}
                      tone="danger"
                      compact
                    >
                      <Trash2 size={14} className="text-red-400" />
                    </SurfaceActionButton>
                  </div>
                </SurfaceCard>
              ))}

              {offices.length === 0 && (
                <SurfaceEmptyState className="py-8 text-center text-sm">
                  {tr("오피스가 없습니다", "No offices yet")}
                </SurfaceEmptyState>
              )}

              <SurfaceActionButton
                onClick={openCreate}
                tone="accent"
                className="w-full border-dashed"
                style={{
                  borderStyle: "dashed",
                }}
              >
                <Plus size={16} />
                {tr("오피스 추가", "Add Office")}
              </SurfaceActionButton>
            </div>
          )}

          {/* ── EDIT VIEW ── */}
          {view === "edit" && (
            <div className="grid gap-4 md:grid-cols-2">
              <SurfaceSubsection
                title={tr("기본 정보", "Identity")}
                description={tr("오피스 이름과 설명을 정리합니다.", "Define office naming and description.")}
              >
                <div className="space-y-4">
                  <div>
                <label
                  className="block text-xs font-medium mb-1"
                  style={{ color: "var(--th-text-secondary)" }}
                >
                  {tr("이름 (영문)", "Name (EN)")}
                </label>
                <input
                  value={formName}
                  onChange={(e) => setFormName(e.target.value)}
                  className="w-full px-3 py-2 rounded-lg text-sm"
                  style={{
                    background: "var(--th-input-bg)",
                    border: "1px solid var(--th-input-border)",
                    color: "var(--th-text-primary)",
                  }}
                  placeholder="e.g. CookingHeart"
                />
              </div>
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("이름 (한국어)", "Name (KO)")}
                    </label>
                    <input
                      value={formNameKo}
                      onChange={(e) => setFormNameKo(e.target.value)}
                      className="w-full px-3 py-2 rounded-lg text-sm"
                      style={{
                        background: "var(--th-input-bg)",
                        border: "1px solid var(--th-input-border)",
                        color: "var(--th-text-primary)",
                      }}
                      placeholder="e.g. 쿠킹하트"
                    />
                  </div>
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("설명", "Description")}
                    </label>
                    <textarea
                      value={formDesc}
                      onChange={(e) => setFormDesc(e.target.value)}
                      className="w-full px-3 py-2 rounded-lg text-sm resize-none"
                      rows={3}
                      style={{
                        background: "var(--th-input-bg)",
                        border: "1px solid var(--th-input-border)",
                        color: "var(--th-text-primary)",
                      }}
                    />
                  </div>
                </div>
              </SurfaceSubsection>

              <SurfaceSubsection
                title={tr("표현", "Appearance")}
                description={tr("아이콘과 대표 색상을 선택합니다.", "Choose the office icon and accent color.")}
              >
                <div className="space-y-4">
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("아이콘", "Icon")}
                    </label>
                    <div className="flex gap-1.5 flex-wrap">
                      {OFFICE_ICONS.map((ic) => (
                        <button
                          key={ic}
                          onClick={() => setFormIcon(ic)}
                          className="flex h-8 w-8 items-center justify-center rounded text-base transition-all"
                          style={{
                            color: "var(--th-text-heading)",
                            border: formIcon === ic
                              ? `1px solid ${formColor}`
                              : "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
                            background: formIcon === ic
                              ? `color-mix(in srgb, ${formColor} 16%, var(--th-bg-surface) 84%)`
                              : "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
                            boxShadow: formIcon === ic ? `0 0 0 1px ${formColor}55` : "none",
                          }}
                        >
                          {ic}
                        </button>
                      ))}
                    </div>
                  </div>
                  <div>
                    <label
                      className="block text-xs font-medium mb-1"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("색상", "Color")}
                    </label>
                    <div className="flex gap-1.5 flex-wrap">
                      {OFFICE_COLORS.map((c) => (
                        <button
                          key={c}
                          type="button"
                          aria-label={`Color ${c}`}
                          aria-pressed={formColor === c}
                          onClick={() => setFormColor(c)}
                          className={`w-7 h-7 rounded-full transition-all ${
                            formColor === c
                              ? "ring-2 ring-offset-2 ring-offset-gray-900 ring-white"
                              : ""
                          }`}
                          style={{ background: c }}
                        />
                      ))}
                    </div>
                  </div>
                  <SurfaceCard
                    style={{
                      borderColor: `${formColor}55`,
                      background: `color-mix(in srgb, ${formColor} 12%, var(--th-card-bg) 88%)`,
                    }}
                  >
                    <div className="flex items-center gap-3">
                      <div className="flex h-11 w-11 items-center justify-center rounded-2xl text-2xl" style={{ background: `${formColor}22` }}>
                        {formIcon}
                      </div>
                      <div className="min-w-0">
                        <div className="truncate text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                          {formNameKo.trim() || formName.trim() || tr("오피스 이름", "Office Name")}
                        </div>
                        <div className="truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {formName.trim() || tr("영문 이름 미입력", "No English name yet")}
                        </div>
                      </div>
                    </div>
                  </SurfaceCard>
                </div>
              </SurfaceSubsection>
            </div>
          )}

          {/* ── AGENTS VIEW ── */}
          {view === "agents" && agentsOffice && (
            <div className="space-y-1">
              <SurfaceNotice tone="accent" compact>
                <div>
                  <div className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                    {tr("멤버 토글", "Toggle Membership")}
                  </div>
                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    {tr("오피스에 포함할 에이전트를 선택하세요.", "Choose which agents belong to this office.")}
                  </div>
                </div>
              </SurfaceNotice>
              {allAgents.map((a) => {
                const inOffice = officeAgentIds.has(a.id);
                return (
                  <SurfaceCard
                    key={a.id}
                    onClick={() => toggleAgent(a.id)}
                    className="w-full cursor-pointer p-2.5 text-left transition-all"
                    style={{
                      background: inOffice
                        ? "color-mix(in srgb, var(--th-accent-primary-soft) 22%, var(--th-bg-surface) 78%)"
                        : "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                      border: inOffice
                        ? "1px solid color-mix(in srgb, var(--th-accent-primary) 28%, transparent)"
                        : "1px solid color-mix(in srgb, var(--th-border) 66%, transparent)",
                    }}
                  >
                    <div className="flex items-center gap-3">
                      <AgentAvatar agent={a} agents={allAgents} size={24} rounded="xl" />
                      <div className="flex-1 min-w-0">
                        <div
                          className="text-sm truncate"
                          style={{ color: "var(--th-text-primary)" }}
                        >
                          {isKo ? a.name_ko || a.name : a.name}
                        </div>
                      </div>
                      {inOffice ? (
                        <UserMinus size={14} className="text-red-400 shrink-0" />
                      ) : (
                        <UserPlus
                          size={14}
                          className="shrink-0"
                          style={{ color: "var(--th-text-muted)" }}
                        />
                      )}
                    </div>
                  </SurfaceCard>
                );
              })}
              {allAgents.length === 0 && (
                <SurfaceEmptyState className="py-8 text-center text-sm">
                  {tr("등록된 에이전트가 없습니다", "No agents registered")}
                </SurfaceEmptyState>
              )}
            </div>
          )}
        </div>

        {/* Footer */}
        <div
          className="flex items-center justify-end gap-2 p-4"
          style={{ borderTop: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)" }}
        >
          {view !== "list" && (
            <SurfaceActionButton tone="neutral" onClick={() => setView("list")}>
              {tr("뒤로", "Back")}
            </SurfaceActionButton>
          )}
          {view === "edit" && (
            <SurfaceActionButton
              onClick={handleSave}
              disabled={saving || !formName.trim()}
              tone="accent"
            >
              {saving
                ? tr("저장 중...", "Saving...")
                : editOffice
                  ? tr("저장", "Save")
                  : tr("생성", "Create")}
            </SurfaceActionButton>
          )}
          {view === "list" && (
            <SurfaceActionButton tone="neutral" onClick={onClose}>
              {tr("닫기", "Close")}
            </SurfaceActionButton>
          )}
          {view === "agents" && (
            <SurfaceActionButton tone="accent" onClick={onClose}>
              {tr("완료", "Done")}
            </SurfaceActionButton>
          )}
        </div>
      </div>
    </div>
  );
}
