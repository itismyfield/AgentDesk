import { useCallback, useEffect, useMemo, useState, type DragEvent } from "react";
import type { Agent, Department, DispatchedSession } from "../../types";
import type { UiLanguage } from "../../i18n";
import * as api from "../../api";
import { buildSpriteMap } from "../AgentAvatar";
import { pickRandomSpritePair } from "./utils";
import { BLANK, ICON_SPRITE_POOL } from "./constants";
import type { FormData } from "./types";

export type AgentManagerTab = "agents" | "departments" | "backlog" | "dispatch";

interface UseAgentManagerControllerParams {
  agents: Agent[];
  departments: Department[];
  language: UiLanguage;
  officeId?: string | null;
  onAgentsChange: () => void;
  onDepartmentsChange: () => void;
  sessions?: DispatchedSession[];
  onAssign?: (id: string, patch: Partial<DispatchedSession>) => Promise<void>;
  activeTab?: AgentManagerTab;
  onTabChange?: (tab: AgentManagerTab) => void;
}

export function useAgentManagerController({
  agents,
  departments,
  language,
  officeId,
  onAgentsChange,
  onDepartmentsChange,
  sessions,
  onAssign,
  activeTab,
  onTabChange,
}: UseAgentManagerControllerParams) {
  const locale = language;
  const isKo = locale.startsWith("ko");
  const tr = useCallback(
    (ko: string, en: string) => (isKo ? ko : en),
    [isKo],
  );

  const [internalTab, setInternalTab] = useState<AgentManagerTab>("agents");
  const [deptTab, setDeptTab] = useState("all");
  const [statusFilter, setStatusFilter] = useState<string>("all");
  const [search, setSearch] = useState("");
  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [dispatchOpen, setDispatchOpen] = useState(activeTab === "dispatch");
  const [agentModal, setAgentModal] = useState<{
    open: boolean;
    editAgent: Agent | null;
  }>({ open: false, editAgent: null });
  const [form, setForm] = useState<FormData>(BLANK);
  const [deptModal, setDeptModal] = useState<{
    open: boolean;
    editDept: Department | null;
  }>({ open: false, editDept: null });
  const [deptOrder, setDeptOrder] = useState<Department[]>(departments);
  const [deptOrderDirty, setDeptOrderDirty] = useState(false);
  const [reorderSaving, setReorderSaving] = useState(false);
  const [draggingDeptId, setDraggingDeptId] = useState<string | null>(null);
  const [dragOverDeptId, setDragOverDeptId] = useState<string | null>(null);
  const [dragOverPosition, setDragOverPosition] = useState<"before" | "after" | null>(null);

  const tab = activeTab ?? internalTab;
  const canShowDispatch = Boolean(sessions && onAssign);
  const resolvedTab = tab === "dispatch" ? "agents" : tab;

  useEffect(() => {
    if (tab === "dispatch" && canShowDispatch) {
      setDispatchOpen(true);
    }
  }, [canShowDispatch, tab]);

  useEffect(() => {
    if (deptOrderDirty) return;
    const currentIds = deptOrder.map((dept) => dept.id).join("|");
    const nextIds = departments.map((dept) => dept.id).join("|");
    if (currentIds !== nextIds) {
      setDeptOrder(departments);
    }
  }, [departments, deptOrder, deptOrderDirty]);

  const handleTabChange = useCallback((nextTab: AgentManagerTab) => {
    if (activeTab === undefined) {
      setInternalTab(nextTab);
    }
    onTabChange?.(nextTab);
  }, [activeTab, onTabChange]);

  const spriteMap = useMemo(() => buildSpriteMap(agents), [agents]);
  const randomIconSprites = useMemo(
    () => ({ total: pickRandomSpritePair(ICON_SPRITE_POOL) }),
    [],
  );

  const sortedAgents = useMemo(() => {
    let filtered = agents;
    if (deptTab !== "all") {
      filtered = filtered.filter((agent) => agent.department_id === deptTab);
    }
    if (statusFilter !== "all") {
      filtered = filtered.filter((agent) => agent.status === statusFilter);
    }
    if (search.trim()) {
      const q = search.toLowerCase();
      filtered = filtered.filter(
        (agent) =>
          agent.name.toLowerCase().includes(q) ||
          agent.name_ko.toLowerCase().includes(q) ||
          (agent.alias && agent.alias.toLowerCase().includes(q)) ||
          agent.avatar_emoji.includes(q),
      );
    }

    return [...filtered].sort((left, right) => {
      const statusOrder = { working: 0, idle: 1, break: 2, offline: 3 };
      const leftStatus = statusOrder[left.status] ?? 4;
      const rightStatus = statusOrder[right.status] ?? 4;
      if (leftStatus !== rightStatus) return leftStatus - rightStatus;
      return left.name.localeCompare(right.name);
    });
  }, [agents, deptTab, search, statusFilter]);

  const openCreateAgent = useCallback(() => {
    setForm(BLANK);
    setAgentModal({ open: true, editAgent: null });
  }, []);

  const openEditAgent = useCallback((agent: Agent) => {
    setForm({
      name: agent.name,
      name_ko: agent.name_ko ?? "",
      name_ja: agent.name_ja ?? "",
      name_zh: agent.name_zh ?? "",
      department_id: agent.department_id ?? "",
      cli_provider: agent.cli_provider ?? "claude",
      avatar_emoji: agent.avatar_emoji ?? "🤖",
      sprite_number: agent.sprite_number ?? null,
      personality: agent.personality ?? "",
    });
    setAgentModal({ open: true, editAgent: agent });
  }, []);

  const handleSaveAgent = useCallback(async () => {
    setSaving(true);
    try {
      const payload: Record<string, unknown> = {
        name: form.name.trim(),
        name_ko: form.name_ko.trim() || form.name.trim(),
        name_ja: form.name_ja.trim() || undefined,
        name_zh: form.name_zh.trim() || undefined,
        department_id: form.department_id || null,
        cli_provider: form.cli_provider,
        avatar_emoji: form.avatar_emoji,
        sprite_number: form.sprite_number,
        personality: form.personality.trim() || null,
      };

      if (!agentModal.editAgent && officeId) {
        payload.office_id = officeId;
      }

      if (agentModal.editAgent) {
        await api.updateAgent(agentModal.editAgent.id, payload);
      } else {
        await api.createAgent(payload);
      }

      setAgentModal({ open: false, editAgent: null });
      onAgentsChange();
    } catch (error) {
      console.error("Agent save failed:", error);
    } finally {
      setSaving(false);
    }
  }, [agentModal.editAgent, form, officeId, onAgentsChange]);

  const handleDeleteAgent = useCallback(async (id: string) => {
    setSaving(true);
    try {
      await api.deleteAgent(id);
      setConfirmDeleteId(null);
      onAgentsChange();
    } catch (error) {
      console.error("Agent delete failed:", error);
    } finally {
      setSaving(false);
    }
  }, [onAgentsChange]);

  const openCreateDept = useCallback(() => {
    setDeptModal({ open: true, editDept: null });
  }, []);

  const openEditDept = useCallback((dept: Department) => {
    setDeptModal({ open: true, editDept: dept });
  }, []);

  const handleMoveDept = useCallback((index: number, direction: -1 | 1) => {
    setDeptOrder((prev) => {
      const next = [...prev];
      const target = index + direction;
      if (target < 0 || target >= next.length) return prev;
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
    setDeptOrderDirty(true);
  }, []);

  const handleSaveOrder = useCallback(async () => {
    setReorderSaving(true);
    try {
      for (let index = 0; index < deptOrder.length; index += 1) {
        await api.updateDepartment(deptOrder[index].id, { sort_order: index });
      }
      setDeptOrderDirty(false);
      onDepartmentsChange();
    } catch (error) {
      console.error("Order save failed:", error);
    } finally {
      setReorderSaving(false);
    }
  }, [deptOrder, onDepartmentsChange]);

  const handleCancelOrder = useCallback(() => {
    setDeptOrder(departments);
    setDeptOrderDirty(false);
  }, [departments]);

  const handleDragStart = useCallback((deptId: string, event: DragEvent<HTMLDivElement>) => {
    setDraggingDeptId(deptId);
    event.dataTransfer.effectAllowed = "move";
  }, []);

  const handleDragOver = useCallback((deptId: string, event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
    const rect = event.currentTarget.getBoundingClientRect();
    const midY = rect.top + rect.height / 2;
    setDragOverDeptId(deptId);
    setDragOverPosition(event.clientY < midY ? "before" : "after");
  }, []);

  const handleDrop = useCallback((targetId: string, _event: DragEvent<HTMLDivElement>) => {
    if (!draggingDeptId || draggingDeptId === targetId) {
      setDraggingDeptId(null);
      setDragOverDeptId(null);
      setDragOverPosition(null);
      return;
    }

    setDeptOrder((prev) => {
      const next = prev.filter((dept) => dept.id !== draggingDeptId);
      const targetIndex = next.findIndex((dept) => dept.id === targetId);
      const insertAt = dragOverPosition === "after" ? targetIndex + 1 : targetIndex;
      const dragged = prev.find((dept) => dept.id === draggingDeptId);
      if (dragged) next.splice(insertAt, 0, dragged);
      return next;
    });

    setDeptOrderDirty(true);
    setDraggingDeptId(null);
    setDragOverDeptId(null);
    setDragOverPosition(null);
  }, [draggingDeptId, dragOverPosition]);

  const handleDragEnd = useCallback(() => {
    setDraggingDeptId(null);
    setDragOverDeptId(null);
    setDragOverPosition(null);
  }, []);

  return {
    canShowDispatch,
    confirmDeleteId,
    deptModal,
    deptOrder,
    deptOrderDirty,
    deptTab,
    draggingDeptId,
    dragOverDeptId,
    dragOverPosition,
    form,
    agentModal,
    handleCancelOrder,
    handleDeleteAgent,
    handleDragEnd,
    handleDragOver,
    handleDragStart,
    handleDrop,
    handleMoveDept,
    handleSaveAgent,
    handleSaveOrder,
    handleTabChange,
    isKo,
    locale,
    dispatchOpen,
    openCreateAgent,
    openCreateDept,
    openEditAgent,
    openEditDept,
    randomIconSprites,
    reorderSaving,
    resolvedTab,
    saving,
    search,
    setAgentModal,
    setConfirmDeleteId,
    setDeptModal,
    setDeptTab,
    setForm,
    setDispatchOpen,
    setSearch,
    setStatusFilter,
    sortedAgents,
    spriteMap,
    statusFilter,
    tr,
  };
}
