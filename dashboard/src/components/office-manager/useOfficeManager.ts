import { useCallback, useMemo, useState } from "react";
import type { Agent, Office } from "../../types";
import * as api from "../../api/client";

export interface OfficeDraft {
  name: string;
  name_ko: string;
  icon: string;
  color: string;
  description: string;
}

export const OFFICE_ICONS = ["🏢", "🏠", "🏭", "🏗️", "🏛️", "🍳", "🎮", "📚", "🔬", "🎨", "🛠️", "🌐"];
export const OFFICE_COLORS = [
  "#10b981", "#14b8a6", "#06b6d4", "#3b82f6", "#84cc16",
  "#f59e0b", "#f97316", "#ef4444", "#64748b", "#22c55e",
];

export function makeBlankDraft(): OfficeDraft {
  return {
    name: "",
    name_ko: "",
    icon: "🏢",
    color: "#10b981",
    description: "",
  };
}

export function makeDraftFromOffice(office: Office | null): OfficeDraft {
  if (!office) return makeBlankDraft();
  return {
    name: office.name ?? "",
    name_ko: office.name_ko ?? "",
    icon: office.icon ?? "🏢",
    color: office.color ?? "#10b981",
    description: office.description ?? "",
  };
}

function draftPayload(draft: OfficeDraft) {
  return {
    name: draft.name.trim(),
    name_ko: draft.name_ko.trim() || draft.name.trim(),
    icon: draft.icon,
    color: draft.color,
    description: draft.description.trim() || null,
  };
}

interface UseOfficeManagerOptions {
  allAgents: Agent[];
  onChanged: () => void;
}

export function useOfficeManager({
  allAgents,
  onChanged,
}: UseOfficeManagerOptions) {
  const [draft, setDraft] = useState<OfficeDraft>(() => makeBlankDraft());
  const [search, setSearch] = useState("");
  const [memberIds, setMemberIds] = useState<Set<string>>(new Set());
  const [membersLoading, setMembersLoading] = useState(false);
  const [saving, setSaving] = useState(false);

  const resetDraft = useCallback((office: Office | null) => {
    setDraft(makeDraftFromOffice(office));
  }, []);

  const clearMembers = useCallback(() => {
    setMemberIds(new Set());
    setMembersLoading(false);
  }, []);

  const loadMembers = useCallback((officeId: string | null) => {
    if (!officeId) {
      clearMembers();
      return () => {};
    }

    let cancelled = false;
    setMemberIds(new Set());
    setMembersLoading(true);
    api.getAgents(officeId)
      .then((agents) => {
        if (!cancelled) {
          setMemberIds(new Set(agents.map((agent) => agent.id)));
        }
      })
      .catch(() => {
        if (!cancelled) {
          setMemberIds(new Set());
        }
      })
      .finally(() => {
        if (!cancelled) {
          setMembersLoading(false);
        }
      });

    return () => {
      cancelled = true;
    };
  }, [clearMembers]);

  const filteredAgents = useMemo(() => {
    const query = search.trim().toLowerCase();
    const filtered = !query
      ? allAgents
      : allAgents.filter((agent) => (
        agent.name.toLowerCase().includes(query)
        || (agent.name_ko ?? "").toLowerCase().includes(query)
        || (agent.alias && agent.alias.toLowerCase().includes(query))
        || agent.avatar_emoji.includes(query)
      ));
    return [...filtered].sort((left, right) => {
      const leftAssigned = memberIds.has(left.id) ? 0 : 1;
      const rightAssigned = memberIds.has(right.id) ? 0 : 1;
      if (leftAssigned !== rightAssigned) return leftAssigned - rightAssigned;
      return (left.alias || left.name_ko || left.name).localeCompare(right.alias || right.name_ko || right.name);
    });
  }, [allAgents, memberIds, search]);

  const saveOffice = useCallback(async ({
    creating,
    office,
  }: {
    creating: boolean;
    office: Office | null;
  }) => {
    setSaving(true);
    try {
      const payload = draftPayload(draft);
      const saved = creating
        ? await api.createOffice(payload)
        : office
          ? await api.updateOffice(office.id, payload).then(() => office)
          : null;
      onChanged();
      return saved;
    } finally {
      setSaving(false);
    }
  }, [draft, onChanged]);

  const deleteOffice = useCallback(async (office: Office) => {
    setSaving(true);
    try {
      await api.deleteOffice(office.id);
      onChanged();
    } finally {
      setSaving(false);
    }
  }, [onChanged]);

  const toggleMember = useCallback(async (office: Office, agentId: string) => {
    setSaving(true);
    const assigned = memberIds.has(agentId);
    setMemberIds((prev) => {
      const next = new Set(prev);
      if (assigned) next.delete(agentId);
      else next.add(agentId);
      return next;
    });
    try {
      if (assigned) {
        await api.removeAgentFromOffice(office.id, agentId);
      } else {
        await api.addAgentToOffice(office.id, agentId);
      }
      onChanged();
    } catch {
      setMemberIds((prev) => {
        const next = new Set(prev);
        if (assigned) next.add(agentId);
        else next.delete(agentId);
        return next;
      });
    } finally {
      setSaving(false);
    }
  }, [memberIds, onChanged]);

  return {
    clearMembers,
    deleteOffice,
    draft,
    filteredAgents,
    loadMembers,
    memberIds,
    membersLoading,
    resetDraft,
    saveOffice,
    saving,
    search,
    setDraft,
    setSearch,
    toggleMember,
  };
}
