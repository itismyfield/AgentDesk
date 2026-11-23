import { useEffect, useState, type CSSProperties, type FormEvent, type KeyboardEvent, type ReactNode } from "react";
import { Folder } from "lucide-react";
import type { Agent, Department } from "../types";
import { CommandMenu, type CommandMenuGroup, type CommandMenuLabels } from "../kit/pro/command-menu";
import AgentPersona from "./AgentPersona";
import { filterCommandGroups } from "./commandPaletteSearch";

function searchTerms(...values: Array<string | null | undefined>): string[] {
  return values.filter((value): value is string => typeof value === "string" && value.trim().length > 0);
}

interface PaletteRoute {
  id: string;
  labelKo: string;
  labelEn: string;
  icon: ReactNode;
}

interface CommandPaletteProps {
  agents: Agent[];
  departments: Department[];
  isKo: boolean;
  onSelectAgent: (agent: Agent) => void;
  onNavigate: (view: string) => void;
  onClose: () => void;
  routes: PaletteRoute[];
  departmentRouteId?: string;
}

// The kit backdrop takes the shell's modal tint.
const OVERLAY_STYLE = { "--aui-overlay": "var(--th-modal-overlay)" } as CSSProperties;

export default function CommandPalette({
  agents,
  departments,
  isKo,
  onSelectAgent,
  onNavigate,
  onClose,
  routes,
  departmentRouteId = "settings_organization",
}: CommandPaletteProps) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const [query, setQuery] = useState("");
  // Closing hands focus back to what opened the palette: the search button, or what had focus before ⌘K.
  const [opener] = useState(() => (document.activeElement instanceof HTMLElement ? document.activeElement : null));
  useEffect(() => () => {
    if (opener?.isConnected && opener !== document.body) opener.focus({ preventScroll: true });
  }, [opener]);

  // Every row runs through its own onSelect and the palette closes after it. There is no onRun,
  // so the kit adds no assistant row and never calls onRun for these rows.
  const pick = (run: () => void) => () => {
    run();
    onClose();
  };
  const groups: CommandMenuGroup[] = [
    {
      id: "navigate",
      label: tr("이동", "Navigate"),
      recommended: false,
      commands: routes.map((route) => ({
        id: route.id,
        label: tr(route.labelKo, route.labelEn),
        icon: route.icon,
        keywords: [route.id],
        onSelect: pick(() => onNavigate(route.id)),
      })),
    },
    {
      id: "agents",
      label: tr("에이전트", "Agents"),
      recommended: false,
      commands: agents.map((agent) => ({
        id: agent.id,
        label: agent.alias || agent.name_ko || agent.name,
        hint: searchTerms(agent.department_name_ko, agent.status).join(" · ") || undefined,
        icon: <AgentPersona agent={agent} size={24} />,
        keywords: searchTerms(agent.id, agent.name, agent.name_ko, agent.alias, agent.avatar_emoji, agent.department_name_ko, agent.status),
        onSelect: pick(() => onSelectAgent(agent)),
      })),
    },
    {
      id: "departments",
      label: tr("부서", "Departments"),
      recommended: false,
      commands: departments.map((dept) => ({
        id: dept.id,
        label: dept.name_ko || dept.name,
        hint: typeof dept.agent_count === "number" ? tr(`에이전트 ${dept.agent_count}명`, `${dept.agent_count} agents`) : undefined,
        // The department emoji stays searchable but is not drawn (no decorative emoji).
        icon: <Folder />,
        keywords: searchTerms(dept.id, dept.name, dept.name_ko, dept.icon),
        onSelect: pick(() => onNavigate(departmentRouteId)),
      })),
    },
  ];

  const labels: Partial<CommandMenuLabels> = {
    launch: tr("명령 팔레트 열기", "Open command palette"),
    search: tr("에이전트, 메뉴, 부서 검색", "Search agents, menu and departments"),
    detailsShow: tr("설명 보기 (F2)", "Show details (F2)"),
    detailsHide: tr("설명 숨기기 (F2)", "Hide details (F2)"),
    close: tr("명령 팔레트 닫기", "Close command palette"),
    details: tr("항목 설명", "Details"),
    list: tr("검색 결과", "Search results"),
    empty: tr("보여 줄 항목이 없어요.", "Nothing to show."),
    noMatch: tr("맞는 항목이 없어요.", "No results."),
    groupMove: tr("다음 묶음", "Next group"),
    searchKey: tr("찾기", "Search"),
  };

  // Esc closes at once, even mid-composition, as the old palette did; the kit's own Esc would clear the query first.
  const closeOnEscape = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    event.stopPropagation();
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-[100] flex items-start justify-center px-4 pt-[8dvh]"
      style={OVERLAY_STYLE}
      onKeyDownCapture={closeOnEscape}
      // The kit keeps its query to itself; follow its search field to filter like the old palette.
      onChange={(event: FormEvent<HTMLDivElement>) => {
        if (event.target instanceof HTMLInputElement) setQuery(event.target.value);
      }}
    >
      <CommandMenu
        open
        modal
        autoFocus
        groups={filterCommandGroups(groups, query)}
        onClose={onClose}
        label={tr("명령 팔레트", "Command Palette")}
        placeholder={tr("검색... (에이전트, 메뉴, 부서)", "Search... (agents, menu, departments)")}
        labels={labels}
      />
    </div>
  );
}
