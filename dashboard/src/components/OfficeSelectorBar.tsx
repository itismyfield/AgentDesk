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
      className="flex items-center gap-1.5 overflow-x-auto px-4 py-2 shrink-0"
      style={{
        borderBottom:
          "1px solid color-mix(in srgb, var(--th-border) 68%, transparent)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 88%, transparent) 100%)",
      }}
    >
      <button
        onClick={() => onSelectOffice(null)}
        className="whitespace-nowrap rounded-full px-2.5 py-1 text-xs font-medium transition-all"
        style={
          selectedOfficeId === null
            ? {
                background: "var(--th-accent-primary-soft)",
                color: "var(--th-accent-primary)",
                border:
                  "1px solid color-mix(in srgb, var(--th-accent-primary) 28%, var(--th-border) 72%)",
              }
            : inactiveButtonStyle
        }
      >
        {isKo ? "전체" : "All"}
      </button>

      {offices.map((o) => (
        <button
          key={o.id}
          onClick={() => onSelectOffice(o.id)}
          className="flex items-center gap-1 whitespace-nowrap rounded-full px-2.5 py-1 text-xs font-medium transition-all"
          style={
            selectedOfficeId === o.id
              ? {
                  background: `color-mix(in srgb, ${o.color} 18%, var(--th-card-bg) 82%)`,
                  color: o.color,
                  border: `1px solid color-mix(in srgb, ${o.color} 28%, var(--th-border) 72%)`,
                }
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
        className="ml-auto shrink-0 rounded-full p-1.5 transition-colors"
        style={{
          color: "var(--th-text-muted)",
          background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
          border: "1px solid color-mix(in srgb, var(--th-border) 70%, transparent)",
        }}
        title={isKo ? "오피스 관리" : "Manage Offices"}
      >
        <Settings size={14} />
      </button>
    </div>
  );
}
