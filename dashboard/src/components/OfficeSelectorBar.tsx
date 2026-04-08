import { Settings } from "lucide-react";
import type { Office } from "../types";

interface OfficeSelectorBarProps {
  offices: Office[];
  selectedOfficeId: string | null;
  onSelectOffice: (id: string | null) => void;
  onManageOffices: () => void;
  isKo: boolean;
}

export default function OfficeSelectorBar({
  offices,
  selectedOfficeId,
  onSelectOffice,
  onManageOffices,
  isKo,
}: OfficeSelectorBarProps) {
  if (offices.length === 0) return null;

  const inactiveButtonStyle = {
    color: "var(--th-text-secondary)",
    background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
    border: "1px solid color-mix(in srgb, var(--th-border) 72%, transparent)",
  };

  return (
    <div
      className="flex items-center gap-1.5 px-3 py-2 overflow-x-auto shrink-0"
      style={{
        borderBottom: "1px solid var(--th-card-border)",
        background: "var(--th-bg-surface)",
      }}
    >
      <button
        onClick={() => onSelectOffice(null)}
        className="px-2.5 py-1 rounded-md text-xs font-medium whitespace-nowrap transition-all"
        style={
          selectedOfficeId === null
            ? { background: "var(--th-accent-primary)", color: "white" }
            : inactiveButtonStyle
        }
      >
        {isKo ? "전체" : "All"}
      </button>

      {offices.map((o) => (
        <button
          key={o.id}
          onClick={() => onSelectOffice(o.id)}
          className="px-2.5 py-1 rounded-md text-xs font-medium whitespace-nowrap transition-all flex items-center gap-1"
          style={
            selectedOfficeId === o.id
              ? { background: o.color }
              : inactiveButtonStyle
          }
        >
          <span>{o.icon}</span>
          <span>{isKo ? o.name_ko || o.name : o.name}</span>
          {o.agent_count !== undefined && o.agent_count > 0 && (
            <span
              className="ml-0.5 text-xs opacity-70"
              style={
                selectedOfficeId === o.id
                  ? undefined
                  : { color: "var(--th-text-muted)" }
              }
            >
              {o.agent_count}
            </span>
          )}
        </button>
      ))}

      <button
        onClick={onManageOffices}
        className="ml-auto p-1.5 rounded-md transition-colors shrink-0"
        style={{
          color: "var(--th-text-muted)",
          background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
          border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
        }}
        title={isKo ? "오피스 관리" : "Manage Offices"}
      >
        <Settings size={14} />
      </button>
    </div>
  );
}
