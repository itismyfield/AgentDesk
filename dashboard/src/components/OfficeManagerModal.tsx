import { useState, useCallback } from "react";
import { X, Plus, Trash2, UserPlus, UserMinus, Settings2 } from "lucide-react";
import type { Office, Agent } from "../types";
import * as api from "../api/client";

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
  "#6366f1", "#3b82f6", "#06b6d4", "#10b981", "#f59e0b",
  "#ef4444", "#ec4899", "#8b5cf6", "#64748b", "#14b8a6",
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
  const [formColor, setFormColor] = useState("#6366f1");
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
    setFormColor("#6366f1");
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
        className="rounded-xl w-full max-w-lg mx-4 max-h-[80vh] flex flex-col"
        style={{
          background: "var(--th-card-bg)",
          border: "1px solid var(--th-card-border)",
        }}
      >
        {/* Header */}
        <div
          className="flex items-center justify-between p-4"
          style={{ borderBottom: "1px solid var(--th-card-border)" }}
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
          <button onClick={onClose} className="p-1 hover:bg-surface-hover rounded">
            <X size={18} style={{ color: "var(--th-text-muted)" }} />
          </button>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-4">
          {/* ── LIST VIEW ── */}
          {view === "list" && (
            <div className="space-y-2">
              {offices.map((o) => (
                <div
                  key={o.id}
                  className="flex items-center gap-3 p-3 rounded-lg"
                  style={{
                    background: "var(--th-bg-surface)",
                    border: "1px solid var(--th-card-border)",
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
                    <button
                      onClick={() => openAgents(o)}
                      className="p-1.5 rounded hover:bg-surface-hover"
                      title={tr("멤버 관리", "Manage Members")}
                    >
                      <Settings2
                        size={14}
                        style={{ color: "var(--th-text-secondary)" }}
                      />
                    </button>
                    <button
                      onClick={() => openEdit(o)}
                      className="p-1.5 rounded hover:bg-surface-hover text-xs"
                      style={{ color: "var(--th-text-secondary)" }}
                    >
                      {tr("편집", "Edit")}
                    </button>
                    <button
                      onClick={() => handleDelete(o.id)}
                      className="p-1.5 rounded hover:bg-red-500/20"
                    >
                      <Trash2 size={14} className="text-red-400" />
                    </button>
                  </div>
                </div>
              ))}

              {offices.length === 0 && (
                <div
                  className="text-center py-8 text-sm"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("오피스가 없습니다", "No offices yet")}
                </div>
              )}

              <button
                onClick={openCreate}
                className="w-full flex items-center justify-center gap-2 p-3 rounded-lg border-dashed transition-colors hover:bg-surface-subtle"
                style={{
                  border: "1px dashed var(--th-input-border)",
                  color: "var(--th-text-secondary)",
                }}
              >
                <Plus size={16} />
                {tr("오피스 추가", "Add Office")}
              </button>
            </div>
          )}

          {/* ── EDIT VIEW ── */}
          {view === "edit" && (
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
                  {tr("아이콘", "Icon")}
                </label>
                <div className="flex gap-1.5 flex-wrap">
                  {OFFICE_ICONS.map((ic) => (
                    <button
                      key={ic}
                      onClick={() => setFormIcon(ic)}
                      className={`w-8 h-8 rounded flex items-center justify-center text-base transition-all ${
                        formIcon === ic
                          ? "ring-2 ring-indigo-500 bg-surface-hover"
                          : "hover:bg-surface-hover"
                      }`}
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
                  rows={2}
                  style={{
                    background: "var(--th-input-bg)",
                    border: "1px solid var(--th-input-border)",
                    color: "var(--th-text-primary)",
                  }}
                />
              </div>
            </div>
          )}

          {/* ── AGENTS VIEW ── */}
          {view === "agents" && agentsOffice && (
            <div className="space-y-1">
              {allAgents.map((a) => {
                const inOffice = officeAgentIds.has(a.id);
                return (
                  <button
                    key={a.id}
                    onClick={() => toggleAgent(a.id)}
                    disabled={saving}
                    className={`w-full flex items-center gap-3 p-2.5 rounded-lg transition-all text-left ${
                      inOffice ? "bg-indigo-500/10" : "hover:bg-surface-subtle"
                    }`}
                    style={{
                      border: inOffice
                        ? "1px solid rgba(99,102,241,0.3)"
                        : "1px solid transparent",
                    }}
                  >
                    <span className="text-base">{a.avatar_emoji}</span>
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
                  </button>
                );
              })}
              {allAgents.length === 0 && (
                <div
                  className="text-center py-8 text-sm"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("등록된 에이전트가 없습니다", "No agents registered")}
                </div>
              )}
            </div>
          )}
        </div>

        {/* Footer */}
        <div
          className="flex items-center justify-end gap-2 p-4"
          style={{ borderTop: "1px solid var(--th-card-border)" }}
        >
          {view !== "list" && (
            <button
              onClick={() => setView("list")}
              className="px-3 py-1.5 rounded-lg text-sm hover:bg-surface-hover"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {tr("뒤로", "Back")}
            </button>
          )}
          {view === "edit" && (
            <button
              onClick={handleSave}
              disabled={saving || !formName.trim()}
              className="px-4 py-1.5 rounded-lg text-sm font-medium bg-indigo-600 hover:bg-indigo-500 text-white disabled:opacity-40 transition-all"
            >
              {saving
                ? tr("저장 중...", "Saving...")
                : editOffice
                  ? tr("저장", "Save")
                  : tr("생성", "Create")}
            </button>
          )}
          {view === "list" && (
            <button
              onClick={onClose}
              className="px-3 py-1.5 rounded-lg text-sm hover:bg-surface-hover"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {tr("닫기", "Close")}
            </button>
          )}
          {view === "agents" && (
            <button
              onClick={onClose}
              className="px-3 py-1.5 rounded-lg text-sm hover:bg-surface-hover"
              style={{ color: "var(--th-text-secondary)" }}
            >
              {tr("완료", "Done")}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
