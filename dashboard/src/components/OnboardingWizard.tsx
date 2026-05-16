import { useState, useEffect, useCallback, useRef } from "react";
import {
  clearOnboardingDraft,
  isMeaningfulOnboardingDraft,
  pickPreferredOnboardingDraft,
  readOnboardingDraft,
  serverDraftToLocalDraft,
  writeOnboardingDraft,
  type AgentDef,
  type BotInfo,
  type ChannelAssignment,
  type CommandBotEntry,
  type OnboardingDraft,
  type OnboardingResumeState,
  type OnboardingStatusResponse,
  type ProviderStatus,
  type ServerOnboardingDraftResponse,
} from "./onboardingDraft";

// ── Types ─────────────────────────────────────────────

const COMMAND_PROVIDERS = ["claude", "codex", "gemini", "opencode", "qwen"] as const;

const PROVIDER_CONFIG = {
  claude: {
    suffix: "cc",
    label: "Claude",
    cliName: "Claude Code",
    installHintKo: "설치: npm install -g @anthropic-ai/claude-code",
    installHintEn: "Install: npm install -g @anthropic-ai/claude-code",
    loginHintKo: "로그인: claude login",
    loginHintEn: "Login: claude login",
    loginCommand: "claude login",
  },
  codex: {
    suffix: "cdx",
    label: "Codex",
    cliName: "Codex CLI",
    installHintKo: "설치: npm install -g @openai/codex",
    installHintEn: "Install: npm install -g @openai/codex",
    loginHintKo: "로그인: codex login",
    loginHintEn: "Login: codex login",
    loginCommand: "codex login",
  },
  gemini: {
    suffix: "gm",
    label: "Gemini",
    cliName: "Gemini CLI",
    installHintKo: "설치: npm install -g @google/gemini-cli",
    installHintEn: "Install: npm install -g @google/gemini-cli",
    loginHintKo: "로그인: gemini",
    loginHintEn: "Login: gemini",
    loginCommand: "gemini",
  },
  opencode: {
    suffix: "oc",
    label: "OpenCode",
    cliName: "OpenCode",
    installHintKo: "설치: npm install -g opencode-ai",
    installHintEn: "Install: npm install -g opencode-ai",
    loginHintKo: "로그인: opencode 실행 후 provider 인증 확인",
    loginHintEn: "Login: run opencode, then verify provider auth",
    loginCommand: "opencode",
  },
  qwen: {
    suffix: "qw",
    label: "Qwen",
    cliName: "Qwen Code",
    installHintKo: "설치: npm install -g @qwen-code/qwen-code@latest",
    installHintEn: "Install: npm install -g @qwen-code/qwen-code@latest",
    loginHintKo: "로그인: qwen 실행 후 /auth",
    loginHintEn: "Login: run qwen, then /auth",
    loginCommand: "qwen -> /auth",
  },
} as const satisfies Record<CommandBotEntry["provider"], {
  suffix: string;
  label: string;
  cliName: string;
  installHintKo: string;
  installHintEn: string;
  loginHintKo: string;
  loginHintEn: string;
  loginCommand: string;
}>;

function providerSuffix(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].suffix;
}

function providerLabel(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].label;
}

function providerCliName(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].cliName;
}

function providerInstallHint(provider: CommandBotEntry["provider"], isKo: boolean) {
  const config = PROVIDER_CONFIG[provider];
  return isKo ? config.installHintKo : config.installHintEn;
}

function providerLoginHint(provider: CommandBotEntry["provider"], isKo: boolean) {
  const config = PROVIDER_CONFIG[provider];
  return isKo ? config.loginHintKo : config.loginHintEn;
}

function providerLoginCommand(provider: CommandBotEntry["provider"]) {
  return PROVIDER_CONFIG[provider].loginCommand;
}

interface Guild {
  id: string;
  name: string;
  channels: Array<{ id: string; name: string; category_id?: string }>;
}

interface ChecklistItem {
  key: string;
  label: string;
  ok: boolean;
  detail: string;
}

interface CompletionChecklistItem {
  key: string;
  ok: boolean;
  label: string;
  detail: string;
}

interface Props {
  isKo: boolean;
  onComplete: () => void;
}

// ── Agent Templates ───────────────────────────────────

interface TemplateAgent {
  id: string;
  name: string;
  nameEn: string;
  description: string;
  descriptionEn: string;
  prompt: string;
}

interface Template {
  key: string;
  name: string;
  nameEn: string;
  icon: string;
  description: string;
  descriptionEn: string;
  agents: TemplateAgent[];
}

const TEMPLATES: Template[] = [
  {
    key: "delivery",
    name: "전달 스쿼드",
    nameEn: "Delivery Squad",
    icon: "🚀",
    description: "출시와 납품에 집중하는 역할별 실행 팀",
    descriptionEn: "Role-based execution team focused on shipping",
    agents: [
      {
        id: "pm",
        name: "PM",
        nameEn: "PM",
        description: "우선순위, 범위, 일정 조율",
        descriptionEn: "Priorities, scope, and delivery coordination",
        prompt:
          "당신은 제품 전달 스쿼드의 PM입니다. 목표를 작업 단위로 쪼개고, 우선순위와 일정 리스크를 관리하며, 결정 사항과 남은 이슈를 명확히 정리합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 결정과 근거를 짧고 선명하게 전달합니다",
      },
      {
        id: "designer",
        name: "디자이너",
        nameEn: "Designer",
        description: "화면 구조, 흐름, 인터랙션 설계",
        descriptionEn: "Interface structure, flows, and interaction design",
        prompt:
          "당신은 제품 전달 스쿼드의 디자이너입니다. 사용 흐름을 설계하고, 핵심 화면의 정보 구조와 인터랙션을 제안하며, 구현 가능한 수준으로 디자인 의도를 정리합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 디자인 결정의 이유와 사용자 영향을 함께 설명합니다",
      },
      {
        id: "developer",
        name: "개발자",
        nameEn: "Developer",
        description: "기능 구현, 버그 수정, 테스트 보강",
        descriptionEn: "Implementation, bug fixes, and test coverage",
        prompt:
          "당신은 제품 전달 스쿼드의 개발자입니다. 요구사항을 실제 코드 변경으로 옮기고, 테스트와 검증까지 마무리해 배포 가능한 상태를 만듭니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 구현 전제와 리스크를 숨기지 않고 설명합니다",
      },
      {
        id: "qa",
        name: "QA",
        nameEn: "QA",
        description: "회귀 확인, 재현 경로, 릴리스 체크",
        descriptionEn: "Regression checks, repro steps, and release checks",
        prompt:
          "당신은 제품 전달 스쿼드의 QA입니다. 변경 사항을 검증하고, 회귀 위험과 누락된 테스트를 찾으며, 재현 가능한 형태로 품질 이슈를 정리합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 발견 사항은 재현 경로와 영향 범위를 함께 적습니다",
      },
    ],
  },
  {
    key: "operations",
    name: "운영 셀",
    nameEn: "Operations Cell",
    icon: "🛠️",
    description: "반복 업무와 실행 흐름을 안정화하는 운영 팀",
    descriptionEn: "Role-based operations team for recurring workflows",
    agents: [
      {
        id: "ops-lead",
        name: "운영 리드",
        nameEn: "Ops Lead",
        description: "운영 정책, 우선순위, 예외 처리 기준",
        descriptionEn: "Operational policy, priorities, and escalation rules",
        prompt:
          "당신은 운영 셀의 운영 리드입니다. 반복 업무를 표준화하고, 예외 상황을 분류하며, 누가 무엇을 언제 처리해야 하는지 운영 기준을 정리합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 운영 판단은 기준과 우선순위를 함께 제시합니다",
      },
      {
        id: "scheduler",
        name: "스케줄러",
        nameEn: "Scheduler",
        description: "일정 배치, 리마인더, 대기열 정리",
        descriptionEn: "Scheduling, reminders, and queue hygiene",
        prompt:
          "당신은 운영 셀의 스케줄러입니다. 반복 일정과 마감 일정을 정리하고, 충돌을 감지하며, 늦어지는 항목을 먼저 끌어올립니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 시간, 우선순위, 다음 액션을 분리해서 설명합니다",
      },
      {
        id: "support",
        name: "서포트",
        nameEn: "Support",
        description: "문의 응답, 장애 분류, 사용자 커뮤니케이션",
        descriptionEn: "Support triage, incidents, and user communication",
        prompt:
          "당신은 운영 셀의 서포트 담당입니다. 문의를 분류하고, 즉시 답할 수 있는 항목과 에스컬레이션이 필요한 항목을 구분해 안내합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 불확실한 내용은 추측하지 않고 상태를 투명하게 공유합니다",
      },
      {
        id: "records",
        name: "기록 담당",
        nameEn: "Records",
        description: "회의록, 운영 로그, SOP 정리",
        descriptionEn: "Notes, runbooks, and SOP maintenance",
        prompt:
          "당신은 운영 셀의 기록 담당입니다. 회의 내용과 운영 결정을 잃지 않도록 정리하고, 실행 가능한 체크리스트와 SOP로 바꿔 팀에 남깁니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 맥락보다 실행 항목이 먼저 보이도록 정리합니다",
      },
    ],
  },
  {
    key: "insight",
    name: "인사이트 데스크",
    nameEn: "Insight Desk",
    icon: "📚",
    description: "조사, 분석, 문서화를 담당하는 인사이트 팀",
    descriptionEn: "Role-based research and analysis team",
    agents: [
      {
        id: "researcher",
        name: "리서처",
        nameEn: "Researcher",
        description: "자료 조사, 출처 수집, 사실 확인",
        descriptionEn: "Research, source collection, and fact checks",
        prompt:
          "당신은 인사이트 데스크의 리서처입니다. 문제와 관련된 자료를 빠르게 찾고, 신뢰할 수 있는 출처와 함께 정리해 후속 분석이 가능하도록 만듭니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 출처와 확인 시점을 함께 남깁니다",
      },
      {
        id: "analyst",
        name: "애널리스트",
        nameEn: "Analyst",
        description: "패턴 분석, 비교, 핵심 인사이트 도출",
        descriptionEn: "Pattern analysis, comparison, and insight synthesis",
        prompt:
          "당신은 인사이트 데스크의 애널리스트입니다. 수집된 자료를 구조화하고, 의미 있는 비교와 패턴을 뽑아 다음 의사결정에 바로 쓸 수 있는 인사이트를 만듭니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 숫자와 근거를 먼저 제시합니다",
      },
      {
        id: "strategist",
        name: "전략가",
        nameEn: "Strategist",
        description: "옵션 평가, 우선순위, 실행 방향 제안",
        descriptionEn: "Options, prioritization, and strategic recommendations",
        prompt:
          "당신은 인사이트 데스크의 전략가입니다. 분석 결과를 바탕으로 선택지를 정리하고, 비용과 리스크를 비교해 실행 방향을 제안합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 추천안과 보류안을 분명히 구분합니다",
      },
      {
        id: "writer",
        name: "라이터",
        nameEn: "Writer",
        description: "보고서, 브리프, 공유용 문서 정리",
        descriptionEn: "Reports, briefs, and shareable writeups",
        prompt:
          "당신은 인사이트 데스크의 라이터입니다. 조사와 분석 결과를 팀이 바로 읽고 행동할 수 있는 브리프, 보고서, 회의 자료로 압축합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 길이보다 전달력을 우선합니다",
      },
    ],
  },
];

// ── Helper: Tooltip ───────────────────────────────────

function Tip({ text }: { text: string }) {
  return (
    <span className="relative group inline-block ml-1 cursor-help">
      <span
        className="inline-flex items-center justify-center w-4 h-4 rounded-full text-xs font-bold"
        style={{ backgroundColor: "rgba(148,163,184,0.2)", color: "var(--th-text-muted)" }}
      >
        ?
      </span>
      <span className="absolute hidden group-hover:block bottom-full left-0 mb-2 px-3 py-2 text-xs rounded-lg whitespace-pre-wrap w-72 z-50 shadow-lg"
        style={{ backgroundColor: "#1e293b", color: "#e2e8f0", border: "1px solid rgba(148,163,184,0.3)" }}
      >
        {text}
      </span>
    </span>
  );
}

function ChecklistPanel({ title, items }: { title: string; items: ChecklistItem[] | CompletionChecklistItem[] }) {
  return (
    <div
      className="rounded-xl border p-4 space-y-2"
      style={{ borderColor: "rgba(148,163,184,0.16)", backgroundColor: "rgba(15,23,42,0.36)" }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
        {title}
      </div>
      {items.map((item) => (
        <div
          key={item.key}
          className="rounded-lg border px-3 py-2 text-sm"
          style={{
            borderColor: item.ok ? "rgba(16,185,129,0.22)" : "rgba(248,113,113,0.24)",
            backgroundColor: item.ok ? "rgba(16,185,129,0.08)" : "rgba(127,29,29,0.18)",
          }}
        >
          <div className="flex items-center gap-2">
            <span style={{ color: item.ok ? "#86efac" : "#fca5a5" }}>{item.ok ? "✓" : "!"}</span>
            <span style={{ color: "var(--th-text-primary)" }}>{item.label}</span>
          </div>
          <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {item.detail}
          </div>
        </div>
      ))}
    </div>
  );
}

interface StepStatusItem {
  step: number;
  label: string;
  status: "complete" | "active" | "blocked" | "pending";
}

function StepStatusRail({
  items,
  isKo,
  setItemRef,
}: {
  items: StepStatusItem[];
  isKo: boolean;
  setItemRef: (step: number, node: HTMLDivElement | null) => void;
}) {
  const statusMeta = (status: StepStatusItem["status"]) => {
    switch (status) {
      case "complete":
        return {
          icon: "✓",
          label: isKo ? "완료" : "Complete",
          borderColor: "rgba(16,185,129,0.24)",
          backgroundColor: "rgba(16,185,129,0.08)",
          iconColor: "#86efac",
        };
      case "active":
        return {
          icon: "•",
          label: isKo ? "진행 중" : "Active",
          borderColor: "rgba(99,102,241,0.34)",
          backgroundColor: "rgba(99,102,241,0.12)",
          iconColor: "#c4b5fd",
        };
      case "blocked":
        return {
          icon: "!",
          label: isKo ? "보완 필요" : "Needs attention",
          borderColor: "rgba(248,113,113,0.24)",
          backgroundColor: "rgba(127,29,29,0.18)",
          iconColor: "#fca5a5",
        };
      default:
        return {
          icon: "○",
          label: isKo ? "대기" : "Pending",
          borderColor: "rgba(148,163,184,0.18)",
          backgroundColor: "rgba(15,23,42,0.32)",
          iconColor: "var(--th-text-muted)",
        };
    }
  };

  return (
    <div className="space-y-2">
      <div className="relative">
        <div className="pointer-events-none absolute inset-y-0 left-0 w-5 bg-gradient-to-r from-[color:var(--th-bg-surface)] to-transparent sm:hidden" />
        <div className="pointer-events-none absolute inset-y-0 right-0 w-8 bg-gradient-to-l from-[color:var(--th-bg-surface)] to-transparent sm:hidden" />
        <div className="flex gap-2 overflow-x-auto pb-1" role="list" aria-label={isKo ? "온보딩 단계" : "Onboarding steps"}>
      {items.map((item) => (
        <div
          key={item.step}
          ref={(node) => setItemRef(item.step, node)}
          role="listitem"
          aria-current={item.status === "active" ? "step" : undefined}
          className="min-w-[7.25rem] rounded-xl border px-3 py-2 sm:min-w-[8.5rem]"
          style={{
            borderColor: statusMeta(item.status).borderColor,
            backgroundColor: statusMeta(item.status).backgroundColor,
          }}
        >
          <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
            Step {item.step}
          </div>
          <div className="mt-1 flex items-center gap-2">
            <span aria-hidden="true" style={{ color: statusMeta(item.status).iconColor }}>
              {statusMeta(item.status).icon}
            </span>
            <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
              {item.label}
            </span>
          </div>
          <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
            {statusMeta(item.status).label}
          </div>
        </div>
      ))}
        </div>
      </div>
      <div className="text-[11px] sm:hidden" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "가로 스크롤로 전체 단계를 확인할 수 있습니다." : "Swipe horizontally to see every step."}
      </div>
    </div>
  );
}

// ── Main Component ────────────────────────────────────

export default function OnboardingWizard({ isKo, onComplete }: Props) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  const initialDraftRef = useRef<OnboardingDraft | null>(null);
  if (initialDraftRef.current === null) {
    initialDraftRef.current = readOnboardingDraft();
  }
  const suppressNextServerDraftSyncRef = useRef(false);
  const initialDraft = initialDraftRef.current;
  const railItemRefs = useRef<Record<number, HTMLDivElement | null>>({});
  const stepHeadingRef = useRef<HTMLHeadingElement | null>(null);

  // Step control
  const [step, setStep] = useState(initialDraft?.step ?? 1);
  const TOTAL_STEPS = 5;

  // Step 1: Bot tokens
  const [commandBots, setCommandBots] = useState<CommandBotEntry[]>(
    initialDraft?.commandBots.length
      ? initialDraft.commandBots
      : [{ provider: "claude", token: "", botInfo: null }],
  );
  const [announceToken, setAnnounceToken] = useState(initialDraft?.announceToken ?? "");
  const [notifyToken, setNotifyToken] = useState(initialDraft?.notifyToken ?? "");
  const [announceBotInfo, setAnnounceBotInfo] = useState<BotInfo | null>(initialDraft?.announceBotInfo ?? null);
  const [notifyBotInfo, setNotifyBotInfo] = useState<BotInfo | null>(initialDraft?.notifyBotInfo ?? null);
  const [validating, setValidating] = useState(false);

  // Step 2: Provider verification
  const [providerStatuses, setProviderStatuses] = useState<Record<string, ProviderStatus>>(initialDraft?.providerStatuses ?? {});
  const [checkingProviders, setCheckingProviders] = useState(false);

  // Step 3: Agent selection
  const [selectedTemplate, setSelectedTemplate] = useState<string | null>(initialDraft?.selectedTemplate ?? null);
  const [agents, setAgents] = useState<AgentDef[]>(initialDraft?.agents ?? []);
  const [customName, setCustomName] = useState(initialDraft?.customName ?? "");
  const [customDesc, setCustomDesc] = useState(initialDraft?.customDesc ?? "");
  const [customNameEn, setCustomNameEn] = useState(initialDraft?.customNameEn ?? "");
  const [customDescEn, setCustomDescEn] = useState(initialDraft?.customDescEn ?? "");
  const [generatingPrompt, setGeneratingPrompt] = useState(false);
  const [expandedAgent, setExpandedAgent] = useState<string | null>(initialDraft?.expandedAgent ?? null);

  // Step 4: Channel setup
  const [guilds, setGuilds] = useState<Guild[]>([]);
  const [selectedGuild, setSelectedGuild] = useState(initialDraft?.selectedGuild ?? "");
  const [channelAssignments, setChannelAssignments] = useState<ChannelAssignment[]>(initialDraft?.channelAssignments ?? []);

  // Step 5: Owner
  const [ownerId, setOwnerId] = useState(initialDraft?.ownerId ?? "");
  const [hasExistingSetup, setHasExistingSetup] = useState(initialDraft?.hasExistingSetup ?? false);
  const [confirmRerunOverwrite, setConfirmRerunOverwrite] = useState(initialDraft?.confirmRerunOverwrite ?? false);
  const [draftNoticeVisible, setDraftNoticeVisible] = useState(Boolean(initialDraft));
  const [completing, setCompleting] = useState(false);
  const [completionChecklist, setCompletionChecklist] = useState<CompletionChecklistItem[] | null>(null);
  const [error, setError] = useState("");
  const [draftSyncReady, setDraftSyncReady] = useState(false);
  const [resumeState, setResumeState] = useState<OnboardingResumeState>("none");

  // Get primary provider from first command bot
  const primaryProvider = commandBots[0]?.provider ?? "claude";
  const uniqueProviders = [...new Set(commandBots.map((bot) => bot.provider))];
  const selectedTemplateInfo = TEMPLATES.find((template) => template.key === selectedTemplate) ?? null;
  const validatedCommandCount = commandBots.filter((bot) => bot.botInfo?.valid).length;
  const commandBotsReady =
    commandBots.length > 0 &&
    commandBots.every((bot) => Boolean(bot.token.trim()) && Boolean(bot.botInfo?.valid));
  const announceReady = Boolean(announceToken.trim()) && Boolean(announceBotInfo?.valid);
  const notifyReady = !notifyToken.trim() || Boolean(notifyBotInfo?.valid);
  const providersReady =
    uniqueProviders.length > 0 &&
    uniqueProviders.every(
      (provider) => providerStatuses[provider]?.installed && providerStatuses[provider]?.logged_in,
    );
  const customAgents = agents.filter((agent) => agent.custom);
  const agentsReady =
    agents.length > 0 &&
    agents.every((agent) => Boolean(agent.prompt.trim()));
  const customAgentsReady = customAgents.every((agent) => Boolean(agent.description.trim()));
  const hasSelectedGuild = Boolean(selectedGuild.trim());
  const channelAssignmentsReady =
    agents.length > 0 &&
    channelAssignments.length === agents.length &&
    channelAssignments.every(
      (assignment) =>
        Boolean((assignment.channelId || assignment.channelName).trim()) &&
        Boolean((assignment.channelName || assignment.recommendedName).trim()),
    );
  const newChannelCount = channelAssignments.filter((assignment) => !assignment.channelId).length;
  const ownerIdValid = !ownerId.trim() || /^\d{17,20}$/.test(ownerId.trim());
  const overwriteAcknowledged = !hasExistingSetup || confirmRerunOverwrite;
  const completionReady =
    commandBotsReady &&
    announceReady &&
    providersReady &&
    agentsReady &&
    hasSelectedGuild &&
    channelAssignmentsReady &&
    ownerIdValid &&
    notifyReady &&
    overwriteAcknowledged;

  const setRailItemRef = useCallback((stepNumber: number, node: HTMLDivElement | null) => {
    railItemRefs.current[stepNumber] = node;
  }, []);

  const goToStep = useCallback((nextStep: number) => {
    setStep(nextStep);
    setError("");
  }, []);

  const applyDraft = useCallback((draft: OnboardingDraft) => {
    initialDraftRef.current = draft;
    setStep(draft.step);
    setCommandBots(
      draft.commandBots.length
        ? draft.commandBots
        : [{ provider: "claude", token: "", botInfo: null }],
    );
    setAnnounceToken(draft.announceToken);
    setNotifyToken(draft.notifyToken);
    setAnnounceBotInfo(draft.announceBotInfo);
    setNotifyBotInfo(draft.notifyBotInfo);
    setProviderStatuses(draft.providerStatuses);
    setSelectedTemplate(draft.selectedTemplate);
    setAgents(draft.agents);
    setCustomName(draft.customName);
    setCustomDesc(draft.customDesc);
    setCustomNameEn(draft.customNameEn);
    setCustomDescEn(draft.customDescEn);
    setExpandedAgent(draft.expandedAgent);
    setSelectedGuild(draft.selectedGuild);
    setChannelAssignments(draft.channelAssignments);
    setOwnerId(draft.ownerId);
    setHasExistingSetup(draft.hasExistingSetup);
    setConfirmRerunOverwrite(draft.confirmRerunOverwrite);
    setCompletionChecklist(null);
    setError("");
    setDraftNoticeVisible(true);
  }, []);

  const resetDraft = useCallback(() => {
    clearOnboardingDraft();
    initialDraftRef.current = null;
    setStep(1);
    setCommandBots([{ provider: "claude", token: "", botInfo: null }]);
    setAnnounceToken("");
    setNotifyToken("");
    setAnnounceBotInfo(null);
    setNotifyBotInfo(null);
    setProviderStatuses({});
    setSelectedTemplate(null);
    setAgents([]);
    setCustomName("");
    setCustomDesc("");
    setCustomNameEn("");
    setCustomDescEn("");
    setExpandedAgent(null);
    setGuilds([]);
    setSelectedGuild("");
    setChannelAssignments([]);
    setOwnerId("");
    setConfirmRerunOverwrite(false);
    setCompletionChecklist(null);
    setError("");
    setResumeState("none");
    setDraftNoticeVisible(false);
    void fetch("/api/onboarding/draft", {
      method: "DELETE",
      credentials: "include",
    }).catch(() => {});
  }, []);

  // Load existing config and the latest server draft, then restore the newer draft.
  useEffect(() => {
    let cancelled = false;

    async function loadInitialState() {
      try {
        const [statusResponse, draftResponse] = await Promise.all([
          fetch("/api/onboarding/status", { credentials: "include" }),
          fetch("/api/onboarding/draft", { credentials: "include" }),
        ]);
        const statusData = (await statusResponse.json()) as OnboardingStatusResponse;
        const draftData = draftResponse.ok
          ? ((await draftResponse.json()) as ServerOnboardingDraftResponse)
          : null;
        if (cancelled) return;

        const serverDraft = serverDraftToLocalDraft(draftData?.draft);
        const serverHasExistingSetup = statusData.setup_mode
          ? statusData.setup_mode === "rerun"
          : Boolean(
              statusData.owner_id ||
                statusData.guild_id ||
                statusData.bot_tokens?.command ||
                statusData.bot_tokens?.announce ||
                statusData.bot_tokens?.notify ||
                statusData.bot_tokens?.command2,
            );
        const preferredDraft = pickPreferredOnboardingDraft(
          initialDraftRef.current,
          serverDraft,
        );
        const nextResumeState =
          draftData?.resume_state ?? statusData.resume_state ?? "none";

        setHasExistingSetup(serverHasExistingSetup);
        setResumeState(nextResumeState);

        if (preferredDraft) {
          suppressNextServerDraftSyncRef.current = preferredDraft === serverDraft;
          applyDraft({
            ...preferredDraft,
            hasExistingSetup: serverHasExistingSetup || preferredDraft.hasExistingSetup,
          });
          setHasExistingSetup(serverHasExistingSetup || preferredDraft.hasExistingSetup);
          return;
        }

        suppressNextServerDraftSyncRef.current = true;
        if (statusData.owner_id) setOwnerId(statusData.owner_id);
        if (statusData.guild_id) setSelectedGuild(statusData.guild_id);
        const commandToken = statusData.bot_tokens?.command;
        const command2Token = statusData.bot_tokens?.command2;
        if (!serverHasExistingSetup && commandToken) {
          setCommandBots((prev) => {
            const copy = [...prev];
            copy[0] = {
              ...copy[0],
              provider: statusData.bot_providers?.command ?? copy[0].provider,
              token: commandToken,
            };
            return copy;
          });
        }
        if (!serverHasExistingSetup && command2Token) {
          setCommandBots((prev) => [
            ...prev,
            {
              provider:
                statusData.bot_providers?.command2 ??
                COMMAND_PROVIDERS.find((provider) => provider !== prev[0].provider) ??
                "codex",
              token: command2Token,
              botInfo: null,
            },
          ]);
        }
        if (!serverHasExistingSetup && statusData.bot_tokens?.announce) {
          setAnnounceToken(statusData.bot_tokens.announce);
        }
        if (!serverHasExistingSetup && statusData.bot_tokens?.notify) {
          setNotifyToken(statusData.bot_tokens.notify);
        }
        if (nextResumeState === "partial_apply") {
          setError(
            tr(
              "이전 온보딩 적용이 중간에 멈췄습니다. 같은 설정으로 다시 완료를 실행하면 기존 채널을 재사용합니다.",
              "A previous onboarding apply stopped mid-flight. Re-running completion with the same setup will reuse the existing channels.",
            ),
          );
        }
      } catch {
        // Ignore initial hydration failures and keep the local state fallback.
      } finally {
        if (!cancelled) {
          setDraftSyncReady(true);
        }
      }
    }

    void loadInitialState();
    return () => {
      cancelled = true;
    };
  }, [applyDraft, isKo]);

  useEffect(() => {
    if (!draftSyncReady || completionChecklist) {
      return;
    }
    const nextDraft: OnboardingDraft = {
      version: 1,
      updatedAtMs: Date.now(),
      step,
      commandBots,
      announceToken,
      notifyToken,
      announceBotInfo,
      notifyBotInfo,
      providerStatuses,
      selectedTemplate,
      agents,
      customName,
      customDesc,
      customNameEn,
      customDescEn,
      expandedAgent,
      selectedGuild,
      channelAssignments,
      ownerId,
      hasExistingSetup,
      confirmRerunOverwrite,
    };
    const controller = new AbortController();

    if (suppressNextServerDraftSyncRef.current) {
      suppressNextServerDraftSyncRef.current = false;
      if (isMeaningfulOnboardingDraft(nextDraft)) {
        initialDraftRef.current = nextDraft;
        writeOnboardingDraft(nextDraft);
      } else {
        initialDraftRef.current = null;
        clearOnboardingDraft();
      }
      return () => controller.abort();
    }

    if (!isMeaningfulOnboardingDraft(nextDraft)) {
      initialDraftRef.current = null;
      clearOnboardingDraft();
      void fetch("/api/onboarding/draft", {
        method: "DELETE",
        credentials: "include",
        signal: controller.signal,
      }).catch(() => {});
      return () => controller.abort();
    }

    initialDraftRef.current = nextDraft;
    writeOnboardingDraft(nextDraft);
    const timer = window.setTimeout(() => {
      void fetch("/api/onboarding/draft", {
        method: "PUT",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          version: nextDraft.version,
          updated_at_ms: nextDraft.updatedAtMs,
          step: nextDraft.step,
          command_bots: nextDraft.commandBots.map((bot) => ({
            provider: bot.provider,
            token: bot.token,
            bot_info: bot.botInfo,
          })),
          announce_token: nextDraft.announceToken,
          notify_token: nextDraft.notifyToken,
          announce_bot_info: nextDraft.announceBotInfo,
          notify_bot_info: nextDraft.notifyBotInfo,
          provider_statuses: nextDraft.providerStatuses,
          selected_template: nextDraft.selectedTemplate,
          agents: nextDraft.agents.map((agent) => ({
            id: agent.id,
            name: agent.name,
            name_en: agent.nameEn ?? null,
            description: agent.description,
            description_en: agent.descriptionEn ?? null,
            prompt: agent.prompt,
            custom: Boolean(agent.custom),
          })),
          custom_name: nextDraft.customName,
          custom_desc: nextDraft.customDesc,
          custom_name_en: nextDraft.customNameEn,
          custom_desc_en: nextDraft.customDescEn,
          expanded_agent: nextDraft.expandedAgent,
          selected_guild: nextDraft.selectedGuild,
          channel_assignments: nextDraft.channelAssignments.map((assignment) => ({
            agent_id: assignment.agentId,
            agent_name: assignment.agentName,
            recommended_name: assignment.recommendedName,
            channel_id: assignment.channelId,
            channel_name: assignment.channelName,
          })),
          owner_id: nextDraft.ownerId,
          has_existing_setup: nextDraft.hasExistingSetup,
          confirm_rerun_overwrite: nextDraft.confirmRerunOverwrite,
        }),
        signal: controller.signal,
      }).catch(() => {});
    }, 300);

    return () => {
      controller.abort();
      window.clearTimeout(timer);
    };
  }, [
    agents,
    announceBotInfo,
    announceToken,
    channelAssignments,
    commandBots,
    completionChecklist,
    confirmRerunOverwrite,
    customDesc,
    customDescEn,
    customName,
    customNameEn,
    expandedAgent,
    hasExistingSetup,
    notifyBotInfo,
    notifyToken,
    ownerId,
    providerStatuses,
    selectedGuild,
    selectedTemplate,
    step,
    draftSyncReady,
  ]);

  useEffect(() => {
    railItemRefs.current[step]?.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });

    window.requestAnimationFrame(() => {
      stepHeadingRef.current?.focus();
    });
  }, [step]);

  // ── API helpers ───────────────────────────────────

  const validateBotToken = async (tkn: string): Promise<BotInfo> => {
    const r = await fetch("/api/onboarding/validate-token", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token: tkn }),
    });
    return r.json();
  };

  const validateStep1 = async () => {
    setValidating(true);
    setError("");
    try {
      // Validate all command bots
      for (let i = 0; i < commandBots.length; i++) {
        if (!commandBots[i].token) {
          setError(tr(`실행 봇 ${i + 1}의 토큰을 입력하세요.`, `Enter token for Command Bot ${i + 1}.`));
          setValidating(false);
          return;
        }
        const info = await validateBotToken(commandBots[i].token);
        setCommandBots((prev) => {
          const copy = [...prev];
          copy[i] = { ...copy[i], botInfo: info };
          return copy;
        });
        if (!info.valid) {
          setError(tr(`실행 봇 ${i + 1} 토큰이 유효하지 않습니다.`, `Command Bot ${i + 1} token is invalid.`));
          setValidating(false);
          return;
        }
      }

      // Validate announce bot
      if (!announceToken) {
        setError(tr("통신 봇 토큰을 입력하세요.", "Enter communication bot token."));
        setValidating(false);
        return;
      }
      const annInfo = await validateBotToken(announceToken);
      setAnnounceBotInfo(annInfo);
      if (!annInfo.valid) {
        setError(tr("통신 봇 토큰이 유효하지 않습니다.", "Communication bot token is invalid."));
        setValidating(false);
        return;
      }

      // Validate notify bot if provided
      if (notifyToken) {
        const ntfInfo = await validateBotToken(notifyToken);
        setNotifyBotInfo(ntfInfo);
        if (!ntfInfo.valid) {
          setError(tr("알림 봇 토큰이 유효하지 않습니다.", "Notification bot token is invalid."));
          setValidating(false);
          return;
        }
      }

      // Don't auto-advance — let user invite bots first
    } catch {
      setError(tr("검증 실패", "Validation failed"));
    }
    setValidating(false);
  };

  const checkProviders = useCallback(async () => {
    setCheckingProviders(true);
    const providers = [...new Set(commandBots.map((b) => b.provider))];
    const statuses: Record<string, ProviderStatus> = {};
    for (const p of providers) {
      try {
        const r = await fetch("/api/onboarding/check-provider", {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ provider: p }),
        });
        statuses[p] = await r.json();
      } catch {
        statuses[p] = { installed: false, logged_in: false };
      }
    }
    setProviderStatuses(statuses);
    setCheckingProviders(false);
  }, [commandBots]);

  useEffect(() => {
    if (step === 2) void checkProviders();
  }, [step, checkProviders]);

  const fetchChannels = async () => {
    const token = announceToken || commandBots[0]?.token;
    if (!token) return;
    try {
      const r = await fetch("/api/onboarding/channels", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token }),
      });
      const d = await r.json();
      setGuilds(d.guilds || []);
      if (d.guilds?.length === 1) setSelectedGuild(d.guilds[0].id);
    } catch {
      setError(tr("채널 조회 실패", "Failed to fetch channels"));
    }
  };

  useEffect(() => {
    if (step === 4 && guilds.length === 0) void fetchChannels();
  }, [step]);

  // When agents change or guild changes, update channel assignments
  useEffect(() => {
    if (agents.length > 0) {
      const suffix = providerSuffix(primaryProvider);
      setChannelAssignments((prev) => {
        const previousByAgent = new Map(prev.map((assignment) => [assignment.agentId, assignment]));
        return agents.map((agent) => {
          const existing = previousByAgent.get(agent.id);
          const recommendedName = `${agent.id}-${suffix}`;
          return {
            agentId: agent.id,
            agentName: agent.name,
            recommendedName,
            channelId: existing?.channelId ?? "",
            channelName: existing?.channelName || existing?.recommendedName || recommendedName,
          };
        });
      });
    } else {
      setChannelAssignments([]);
    }
  }, [agents, primaryProvider]);

  const selectTemplate = (key: string) => {
    const tpl = TEMPLATES.find((t) => t.key === key);
    if (!tpl) return;
    setSelectedTemplate(key);
    setAgents(tpl.agents.map((a) => ({ ...a, custom: false })));
  };

  const addCustomAgent = () => {
    if (!customName.trim()) return;
    const name = customName.trim();
    const desc = customDesc.trim();
    const nameEn = customNameEn.trim() || name;
    const descEn = customDescEn.trim() || desc || nameEn;
    const id = name
      .toLowerCase()
      .replace(/[^a-z0-9가-힣]/g, "-")
      .replace(/-+/g, "-")
      .replace(/^-|-$/g, "")
      || `agent-${agents.length + 1}`;
    // Generate prompt in the same format as templates
    const prompt = `당신은 ${name}입니다. ${desc || name + "의 역할을 수행합니다"}.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 간결하고 명확하게 답변합니다\n- 필요시 확인 질문을 합니다`;
    setAgents((prev) => [
      ...prev,
      {
        id,
        name,
        nameEn,
        description: desc,
        descriptionEn: descEn,
        prompt,
        custom: true,
      },
    ]);
    setExpandedAgent(id);
    setCustomName("");
    setCustomDesc("");
    setCustomNameEn("");
    setCustomDescEn("");
  };

  const removeAgent = (id: string) => {
    setAgents((prev) => prev.filter((a) => a.id !== id));
  };

  const generateAiPrompt = async (agentId: string) => {
    const agent = agents.find((a) => a.id === agentId);
    if (!agent) return;
    setGeneratingPrompt(true);
    try {
      const r = await fetch("/api/onboarding/generate-prompt", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          name: agent.name,
          description: agent.description,
          provider: primaryProvider,
        }),
      });
      const d = await r.json();
      if (d.prompt) {
        setAgents((prev) =>
          prev.map((a) => (a.id === agentId ? { ...a, prompt: d.prompt } : a)),
        );
      }
    } catch {
      setError(tr("프롬프트 생성 실패", "Failed to generate prompt"));
    }
    setGeneratingPrompt(false);
  };

  const handleComplete = async () => {
    if (!completionReady) {
      setError(tr("완료 전 체크리스트의 실패 항목을 먼저 해결하세요.", "Resolve the failed checklist items before completing setup."));
      return;
    }

    setCompleting(true);
    setCompletionChecklist(null);
    setError("");
    try {
      const r = await fetch("/api/onboarding/complete", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          token: commandBots[0]?.token || "",
          announce_token: announceToken || null,
          notify_token: notifyToken || null,
          command_token_2: commandBots.length > 1 ? commandBots[1].token : null,
          command_provider_2: commandBots.length > 1 ? commandBots[1].provider : null,
          guild_id: selectedGuild,
          owner_id: ownerId || null,
          provider: primaryProvider,
          template: selectedTemplate || null,
          rerun_policy: hasExistingSetup && confirmRerunOverwrite ? "replace_existing" : "reuse_existing",
          channels: channelAssignments.map((ca) => ({
            channel_id: ca.channelId || ca.channelName,
            channel_name: ca.channelName,
            role_id: ca.agentId,
            description: agents.find((a) => a.id === ca.agentId)?.description || null,
            system_prompt: agents.find((a) => a.id === ca.agentId)?.prompt || null,
          })),
        }),
      });
      const d = await r.json();
      if (d.ok) {
        setResumeState("none");
        if (Array.isArray(d.checklist)) {
          clearOnboardingDraft();
          setDraftNoticeVisible(false);
          setCompletionChecklist(d.checklist);
        } else {
          clearOnboardingDraft();
          onComplete();
        }
      } else {
        const retryHint = d.partial_apply || d.completion_state?.partial_apply
          ? tr(
              "일부 적용이 남았습니다. 같은 payload로 다시 실행하면 기존 Discord 채널을 재사용합니다.",
              "Setup was partially applied. Retrying with the same payload will reuse the existing Discord channels.",
            )
          : "";
        setError([d.error || tr("설정 저장 실패", "Failed to save"), retryHint].filter(Boolean).join(" "));
      }
    } catch {
      setError(tr("완료 실패", "Failed to complete"));
    }
    setCompleting(false);
  };

  // ── Invite link helpers ──────────────────────────────

  // Discord permission bit values
  const PERMS = {
    // Command bot: Send Messages + Read Message History + Manage Messages
    //   + Create Public Threads + Send Messages in Threads
    command: (2048 + 65536 + 8192 + 17179869184 + 274877906944).toString(),
    // Announce bot: Administrator (simplest — covers channel creation, role management, etc.)
    announce: "8",
    // Notify bot: Send Messages only
    notify: "2048",
  };

  const makeInviteUrl = (botId: string, permissions: string) =>
    `https://discord.com/oauth2/authorize?client_id=${botId}&scope=bot&permissions=${permissions}`;

  // ── Styles ──────────────────────────────────────────

  const stepBox = "rounded-2xl border p-6 space-y-5";
  const inputStyle = "w-full rounded-xl px-4 py-3 text-sm bg-surface-subtle border";
  const btnPrimary =
    "px-6 py-3 rounded-xl text-sm font-medium bg-emerald-600 text-white hover:bg-emerald-500 disabled:opacity-50 transition-colors";
  const btnSecondary =
    "px-6 py-3 rounded-xl text-sm font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 disabled:opacity-50 transition-[opacity,color]";
  const btnSmall =
    "px-3 py-1.5 rounded-lg text-xs font-medium border bg-surface-subtle text-th-text-secondary hover:text-th-text-primary hover:opacity-100 transition-[opacity,color]";
  const labelStyle = "text-xs font-medium block mb-1";
  const actionRow = "flex flex-col sm:flex-row gap-3 pt-2";
  const borderLight = "rgba(148,163,184,0.2)";
  const borderInput = "rgba(148,163,184,0.24)";

  const guild = guilds.find((g) => g.id === selectedGuild);
  const step1Checklist: ChecklistItem[] = [
    {
      key: "command-bots",
      label: tr("실행 봇 검증", "Command bots validated"),
      ok: commandBotsReady,
      detail: tr(
        `${validatedCommandCount}/${commandBots.length}개 실행 봇 토큰이 검증되었습니다.`,
        `${validatedCommandCount}/${commandBots.length} command bot tokens are validated.`,
      ),
    },
    {
      key: "announce-bot",
      label: tr("통신 봇 검증", "Communication bot validated"),
      ok: announceReady,
      detail: announceReady
        ? tr("채널 생성과 권한 설정에 사용할 통신 봇이 준비되었습니다.", "Communication bot is ready for channel creation and permissions.")
        : tr("통신 봇이 없으면 실제 Discord 채널 생성이 진행되지 않습니다.", "Without the communication bot, real Discord channel setup cannot run."),
    },
    {
      key: "notify-bot",
      label: tr("알림 봇 상태", "Notification bot status"),
      ok: notifyReady,
      detail: notifyToken.trim()
        ? tr("알림 봇 토큰이 검증되었습니다.", "Notification bot token is validated.")
        : tr("선택 사항입니다. 비워두면 알림 봇 없이 진행합니다.", "Optional. Leave blank to continue without a notification bot."),
    },
  ];
  const step2Checklist: ChecklistItem[] = uniqueProviders.map((provider) => {
    const status = providerStatuses[provider];
    const installed = Boolean(status?.installed);
    const loggedIn = Boolean(status?.installed && status?.logged_in);
    return {
      key: provider,
      label: tr(`${providerCliName(provider)} 준비`, `${providerCliName(provider)} ready`),
      ok: installed && loggedIn,
      detail: !status
        ? tr("아직 확인 전입니다. 다시 확인을 눌러 상태를 읽어오세요.", "Not checked yet. Re-run the provider check.")
        : installed && loggedIn
          ? tr("CLI 설치와 로그인 상태가 모두 확인되었습니다.", "CLI installation and login are both confirmed.")
          : !installed
            ? providerInstallHint(provider, isKo)
            : `${tr("로그인 필요:", "Login required:")} ${providerLoginCommand(provider)}`,
    };
  });
  const step3Checklist: ChecklistItem[] = [
    {
      key: "preset",
      label: tr("역할 프리셋 또는 커스텀 팀 구성", "Role preset or custom team selected"),
      ok: agents.length > 0,
      detail: selectedTemplateInfo
        ? tr(
            `${selectedTemplateInfo.name} 프리셋을 기준으로 ${agents.length}개 에이전트를 구성했습니다.`,
            `${selectedTemplateInfo.nameEn} preset selected with ${agents.length} agents.`,
          )
        : tr(
            `${agents.length}개 커스텀 에이전트를 직접 구성했습니다.`,
            `${agents.length} custom agents configured manually.`,
          ),
    },
    {
      key: "prompts",
      label: tr("모든 에이전트 프롬프트 준비", "All agent prompts prepared"),
      ok: agentsReady,
      detail: agentsReady
        ? tr("각 에이전트에 시스템 프롬프트가 채워져 있습니다.", "Every agent has a system prompt.")
        : tr("비어 있는 시스템 프롬프트가 있으면 완료할 수 없습니다.", "Blank system prompts block completion."),
    },
    {
      key: "custom-guidance",
      label: tr("커스텀 에이전트 설명 준비", "Custom agent descriptions ready"),
      ok: customAgentsReady,
      detail: customAgents.length === 0
        ? tr("현재는 프리셋 에이전트만 사용 중입니다.", "Only preset agents are in use right now.")
        : customAgentsReady
          ? tr("설명 기반으로 AI 프롬프트 초안을 생성할 준비가 되었습니다.", "Descriptions are ready for AI prompt generation.")
          : tr("커스텀 에이전트의 이름과 설명을 채워야 AI 초안이 더 정확해집니다.", "Fill in custom agent names and descriptions for better AI prompt drafts."),
    },
  ];
  const step4Checklist: ChecklistItem[] = [
    {
      key: "guild",
      label: tr("Discord 서버 선택", "Discord server selected"),
      ok: hasSelectedGuild,
      detail: hasSelectedGuild
        ? tr("이 서버에 채널 생성/재사용을 적용합니다.", "Channel creation and reuse will target this server.")
        : tr("실제 채널 생성을 위해 Discord 서버 선택이 필수입니다.", "Selecting a Discord server is required for real channel setup."),
    },
    {
      key: "assignments",
      label: tr("에이전트별 채널 매핑", "Agent-to-channel mapping ready"),
      ok: channelAssignmentsReady,
      detail: channelAssignmentsReady
        ? tr(
            `${channelAssignments.length}개 에이전트 채널 매핑이 준비되었습니다.`,
            `${channelAssignments.length} agent channel mappings are ready.`,
          )
        : tr("모든 에이전트에 채널 이름 또는 기존 채널을 지정해야 합니다.", "Each agent needs a channel name or existing channel."),
    },
    {
      key: "new-channels",
      label: tr("새 채널 생성 준비", "New channel creation ready"),
      ok: newChannelCount === 0 || announceReady,
      detail:
        newChannelCount === 0
          ? tr("모든 에이전트가 기존 채널에 연결됩니다.", "All agents are mapped to existing channels.")
          : tr(
              `${newChannelCount}개 채널은 완료 시 자동 생성됩니다.`,
              `${newChannelCount} channels will be created automatically on completion.`,
            ),
    },
  ];
  const step5Checklist: ChecklistItem[] = [
    {
      key: "owner-id",
      label: tr("소유자 ID 형식", "Owner ID format"),
      ok: ownerIdValid,
      detail: ownerId.trim()
        ? tr("17~20자리 Discord 사용자 ID 형식인지 확인했습니다.", "Checked that the value matches a Discord user ID format.")
        : tr("비워두면 첫 메시지 발신자가 자동 소유자가 됩니다.", "Leave blank to make the first message sender the owner."),
    },
    {
      key: "apply-ready",
      label: tr("실제 세팅 적용 준비", "Ready to apply real setup"),
      ok: completionReady,
      detail: completionReady
        ? tr("완료 시 Discord 채널, 설정 파일, 파이프라인 검증까지 서버에서 진행합니다.", "Completion will apply Discord channels, settings, and pipeline verification on the server.")
        : tr("이전 단계의 실패 항목이 남아 있어 아직 완료를 실행할 수 없습니다.", "A previous step is still failing, so completion is blocked."),
    },
    ...(hasExistingSetup
      ? [
          {
            key: "rerun-overwrite",
            label: tr("재실행 덮어쓰기 확인", "Rerun overwrite acknowledgement"),
            ok: overwriteAcknowledged,
            detail: overwriteAcknowledged
              ? tr(
                  "기존 role_id 기반 에이전트와 채널 매핑을 다시 적용할 수 있다는 점을 확인했습니다.",
                  "You acknowledged that existing role-based agents and channel mappings may be applied again.",
                )
              : tr(
                  "현재 API는 기존 에이전트 구성을 프리필하지 않습니다. 같은 role_id를 다시 적용할 수 있다는 점을 확인해야 완료할 수 있습니다.",
                  "The current API does not prefill the existing agent layout. Completion requires acknowledging that the same role IDs may be applied again.",
                ),
          },
        ]
      : []),
  ];
  const stepStatusFor = (stepNumber: number, ok: boolean): StepStatusItem["status"] => {
    if (step === stepNumber) return "active";
    if (step > stepNumber) return ok ? "complete" : "blocked";
    return "pending";
  };
  const stepStatusItems: StepStatusItem[] = [
    { step: 1, label: tr("봇", "Bots"), status: stepStatusFor(1, step1Checklist.every((item) => item.ok)) },
    { step: 2, label: tr("프로바이더", "Providers"), status: stepStatusFor(2, step2Checklist.every((item) => item.ok)) },
    { step: 3, label: tr("에이전트", "Agents"), status: stepStatusFor(3, step3Checklist.every((item) => item.ok)) },
    { step: 4, label: tr("채널", "Channels"), status: stepStatusFor(4, step4Checklist.every((item) => item.ok)) },
    {
      step: 5,
      label: tr("적용", "Apply"),
      status: stepStatusFor(5, (completionChecklist ?? step5Checklist).every((item) => item.ok)),
    },
  ];
  const applySummary = [
    {
      key: "channels",
      label: tr("Discord 채널", "Discord channels"),
      detail: hasSelectedGuild
        ? tr(
            `${channelAssignments.length}개 에이전트 채널 매핑을 적용하고, 새 채널 ${newChannelCount}개는 완료 시 실제 생성합니다. 네트워크 오류 뒤 재시도 전에는 Discord에 일부 채널이 먼저 생겼는지 확인하는 편이 안전합니다.`,
            `Applies ${channelAssignments.length} agent channel mappings and creates ${newChannelCount} new channels on completion. After a network error, check whether some channels were already created in Discord before retrying.`,
          )
        : tr(
            "서버를 선택해야 실제 채널 생성과 기존 채널 연결을 확정할 수 있습니다.",
            "Select a server before real channel creation and existing-channel reuse can be finalized.",
          ),
    },
    {
      key: "settings",
      label: tr("설정 저장", "Settings write"),
      detail: tr(
        "owner ID, command/communication bot, provider 조합을 서버에 실제로 기록합니다.",
        "Writes the owner ID, command/communication bot, and provider wiring to the server.",
      ),
    },
    {
      key: "pipeline",
      label: tr("기본 운영 파이프라인", "Default operating pipeline"),
      detail: tr(
        "기본 채널/카테고리와 함께 초기 파이프라인/설정 재생성을 같은 완료 작업에서 처리합니다.",
        "Rebuilds the initial pipeline and baseline settings alongside the default channels and categories.",
      ),
    },
    {
      key: "verification",
      label: tr("완료 후 검증", "Post-apply verification"),
      detail: tr(
        "성공 응답은 설정 산출물과 기본 파이프라인 파일 검증이 끝난 뒤에만 내려옵니다. 현재 체크리스트는 read-back 비교가 아니라 생성/검증 결과 요약입니다.",
        "A success response arrives only after settings artifacts and the default pipeline file are verified. The current checklist is a summary of creation/verification, not a read-back diff.",
      ),
    },
  ];
  const draftNoticeTitle =
    resumeState === "partial_apply"
      ? tr("중간에 멈춘 온보딩 상태를 복원했습니다.", "Restored the onboarding state from a partial apply.")
      : tr("저장된 온보딩 진행 상태를 복원했습니다.", "Restored your saved onboarding progress.");
  const draftNoticeDetail =
    resumeState === "partial_apply"
      ? tr(
          "이전 적용이 중간에 멈췄습니다. 같은 설정으로 다시 완료를 실행하면 이미 만든 Discord 채널과 draft를 기준으로 이어서 진행합니다.",
          "A previous apply stopped mid-flight. Re-running completion with the same setup will continue from the saved draft and any Discord channels that were already created.",
        )
      : tr(
          "브라우저를 바꾸거나 새로고침해도 서버에 저장된 draft를 기준으로 이어서 진행할 수 있습니다. 처음부터 다시 하려면 임시 저장을 비우세요.",
          "You can resume from the server-side draft even after switching browsers or refreshing. Clear the draft if you want to start over.",
        );

  // ── Render ──────────────────────────────────────────

  return (
    <div className="mx-auto w-full max-w-2xl min-w-0 space-y-6 p-4 sm:p-8">
      {draftNoticeVisible && (
        <div
          className="rounded-xl border px-4 py-3"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
            background: "color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent)",
          }}
        >
          <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
            {draftNoticeTitle}
          </div>
          <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-secondary)" }}>
            {draftNoticeDetail}
          </div>
          <div className="mt-3 flex flex-wrap gap-2">
            <button type="button"
              onClick={() => setDraftNoticeVisible(false)}
              className={btnSmall}
              style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-secondary)" }}
            >
              {tr("계속 진행", "Keep going")}
            </button>
            <button type="button"
              onClick={resetDraft}
              className={btnSmall}
              style={{ borderColor: "rgba(248,113,113,0.3)", color: "#fca5a5" }}
            >
              {tr("임시 저장 비우기", "Clear draft")}
            </button>
          </div>
        </div>
      )}

      {/* Header */}
      <div className="text-center space-y-2">
        <h1 className="text-2xl font-bold" style={{ color: "var(--th-text-heading)" }}>
          {tr("AgentDesk 설정", "AgentDesk Setup")}
        </h1>
        <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
          Step {step}/{TOTAL_STEPS}
        </p>
        <div className="flex gap-1 justify-center">
          {Array.from({ length: TOTAL_STEPS }, (_, i) => i + 1).map((s) => (
            <div
              key={s}
              className="h-1.5 rounded-full transition-all"
              style={{
                width: s <= step ? 40 : 20,
                backgroundColor: s <= step ? "var(--th-accent-primary)" : "rgba(148,163,184,0.3)",
              }}
            />
          ))}
        </div>
      </div>

      <StepStatusRail items={stepStatusItems} isKo={isKo} setItemRef={setRailItemRef} />

      {/* Error banner */}
      {error && (
        <div
          className="rounded-xl px-4 py-3 text-sm border"
          style={{ borderColor: "rgba(248,113,113,0.4)", color: "#fca5a5", backgroundColor: "rgba(127,29,29,0.2)" }}
        >
          {error}
        </div>
      )}

      {/* ──────────────── Step 1: Bot Connection ──────────────── */}
      {step === 1 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
              {tr("Discord 봇 연결", "Connect Discord Bots")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "AgentDesk는 Discord 봇을 통해 AI 에이전트를 운영합니다. 각 봇의 역할이 다르므로 최소 2개(실행 봇 + 통신 봇)가 필요합니다.",
                "AgentDesk runs AI agents through Discord bots. You need at least 2 bots (Command + Communication).",
              )}
            </p>
          </div>

          {/* How to get tokens */}
          <div
            className="rounded-xl p-4 text-sm space-y-2"
            style={{
              background: "color-mix(in srgb, var(--th-accent-primary-soft) 72%, transparent)",
              border: "1px solid color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
            }}
          >
            <div className="font-medium" style={{ color: "var(--th-text-primary)" }}>
              {tr("봇 토큰을 얻는 방법", "How to get bot tokens")}
            </div>
            <ol className="list-decimal list-inside space-y-1" style={{ color: "var(--th-text-secondary)" }}>
              <li>
                <a href="https://discord.com/developers/applications" target="_blank" rel="noopener noreferrer" className="text-emerald-300 hover:text-emerald-200 underline">
                  Discord Developer Portal
                </a>
                {tr("에서 New Application 클릭", " → Click New Application")}
              </li>
              <li>{tr("왼쪽 Bot 탭 → Reset Token → 토큰 복사", "Left Bot tab → Reset Token → Copy token")}</li>
              <li>
                {tr(
                  "같은 Bot 탭 → Privileged Gateway Intents에서 MESSAGE CONTENT Intent를 활성화",
                  "On the same Bot tab → Privileged Gateway Intents → enable MESSAGE CONTENT Intent",
                )}
                <span className="block ml-4 mt-0.5" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    "이 설정이 없으면 봇이 메시지 내용을 읽지 못해 정상 동작하지 않습니다",
                    "Without this, the bot cannot read message content and will not function properly",
                  )}
                </span>
              </li>
              <li>{tr("아래에 토큰을 붙여넣고 검증하면, 서버 초대 링크가 자동 생성됩니다", "Paste tokens below and validate — invite links are generated automatically")}</li>
            </ol>
          </div>

          {/* Command Bots */}
          <div className="space-y-3">
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("실행 봇", "Command Bot")}
              </span>
              <Tip text={tr(
                "에이전트의 AI 세션을 실행하는 봇입니다.\nDiscord에서 메시지를 받으면 이 봇이\nClaude Code, Codex CLI, Gemini CLI, OpenCode, 또는 Qwen Code를 실행하여\n에이전트가 작업합니다.",
                "Runs AI sessions for agents.\nWhen a message arrives, this bot\nlaunches Claude Code, Codex CLI, Gemini CLI, OpenCode, or Qwen Code.",
              )} />
            </div>

            {commandBots.map((bot, i) => (
              <div key={i} className="rounded-xl p-4 border space-y-2" style={{ borderColor: borderLight }}>
                <div className="flex items-center gap-3">
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr(`실행 봇 ${i + 1}`, `Command Bot ${i + 1}`)}
                  </span>
                  <div className="flex rounded-lg overflow-hidden border" style={{ borderColor: "rgba(148,163,184,0.3)" }}>
                    {COMMAND_PROVIDERS.map((p) => (
                      <button type="button"
                        key={p}
                        onClick={() => {
                          setCommandBots((prev) => {
                            const copy = [...prev];
                            copy[i] = { ...copy[i], provider: p };
                            return copy;
                          });
                        }}
                        className="px-3 py-1 text-xs transition-colors"
                        style={{
                          backgroundColor: bot.provider === p ? "color-mix(in srgb, var(--th-accent-primary-soft) 84%, transparent)" : "transparent",
                          color: bot.provider === p ? "var(--th-text-primary)" : "var(--th-text-muted)",
                        }}
                      >
                        {providerLabel(p)}
                      </button>
                    ))}
                  </div>
                  {commandBots.length > 1 && (
                    <button type="button"
                      onClick={() => setCommandBots((prev) => prev.filter((_, j) => j !== i))}
                      className="ml-auto text-xs text-red-400 hover:text-red-300"
                    >
                      {tr("제거", "Remove")}
                    </button>
                  )}
                </div>
                <input
                  type="password"
                  placeholder={tr("봇 토큰 붙여넣기", "Paste bot token")}
                  value={bot.token}
                  onChange={(e) => {
                    setCommandBots((prev) => {
                      const copy = [...prev];
                      copy[i] = { ...copy[i], token: e.target.value };
                      return copy;
                    });
                  }}
                  className={inputStyle}
                  style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                />
                {bot.botInfo?.valid && (
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-emerald-400">✓ {bot.botInfo.bot_name}</span>
                    <a
                      href={makeInviteUrl(bot.botInfo.bot_id!, PERMS.command)}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="text-xs px-2 py-0.5 rounded-md border transition-colors hover:opacity-100"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
                        background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                        color: "var(--th-text-primary)",
                      }}
                    >
                      {tr("서버에 초대 →", "Invite to server →")}
                    </a>
                  </div>
                )}
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    `자동 설정 권한: Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads`,
                    `Auto-configured: Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads`,
                  )}
                </div>
              </div>
            ))}

            {commandBots.length < 2 && (
              <button type="button"
                onClick={() => {
                  const used = new Set(commandBots.map((bot) => bot.provider));
                  const other = COMMAND_PROVIDERS.find((provider) => !used.has(provider)) ?? "claude";
                  setCommandBots((prev) => [...prev, { provider: other, token: "", botInfo: null }]);
                }}
                className={btnSmall}
                style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-secondary)" }}
              >
                + {tr("두 번째 실행 봇 추가 (듀얼 프로바이더)", "Add second command bot (dual provider)")}
              </button>
            )}
          </div>

          {/* Announce Bot */}
          <div className="space-y-2">
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("통신 봇", "Communication Bot")}
              </span>
              <Tip text={tr(
                "에이전트들이 서로 메시지를 보낼 때 사용하는 봇입니다.\n에이전트 A가 에이전트 B에게 작업을 요청하거나\n결과를 회신할 때 이 봇을 통해 전송합니다.\n\n또한 온보딩 시 Discord 채널을 자동 생성하고,\n다른 봇들의 채널 접근 권한을 설정합니다.\n(별도의 봇이어야 메시지 충돌이 방지됩니다)",
                "Used for agent-to-agent communication.\nAgent A sends tasks to Agent B through this bot.\n\nAlso creates Discord channels during onboarding\nand manages channel permissions for other bots.\n(Must be a separate bot to prevent conflicts)",
              )} />
              <span className="text-xs px-1.5 py-0.5 rounded bg-red-500/20 text-red-300 font-medium">
                {tr("필수", "Required")}
              </span>
            </div>
            <input
              type="password"
              placeholder={tr("통신 봇 토큰 붙여넣기", "Paste communication bot token")}
              value={announceToken}
              onChange={(e) => setAnnounceToken(e.target.value)}
              className={inputStyle}
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            />
            {announceBotInfo?.valid && (
              <div className="flex items-center gap-2">
                <span className="text-xs text-emerald-400">✓ {announceBotInfo.bot_name}</span>
                <a
                  href={makeInviteUrl(announceBotInfo.bot_id!, PERMS.announce)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="text-xs px-2 py-0.5 rounded-md border transition-colors hover:opacity-100"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
                    background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  {tr("서버에 초대 (관리자 권한) →", "Invite to server (Admin) →")}
                </a>
              </div>
            )}
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "관리자(Administrator) 권한으로 초대됩니다. 채널 생성, 봇 권한 설정 등을 자동으로 처리합니다.",
                "Invited with Administrator permission. Handles channel creation and bot permission setup automatically.",
              )}
            </div>
          </div>

          {/* Notify Bot */}
          <div className="space-y-2">
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("알림 봇", "Notification Bot")}
              </span>
              <Tip text={tr(
                "시스템 상태, 오류, 경고 등 정보 전달에만 사용됩니다.\n이 봇의 메시지에는 에이전트가 반응하지 않습니다.\n없어도 기본 기능에 지장은 없습니다.",
                "Only for system status and error notifications.\nAgents don't respond to this bot's messages.\nOptional — core features work without it.",
              )} />
              <span
                className="text-xs px-1.5 py-0.5 rounded font-medium"
                style={{
                  color: "var(--th-text-muted)",
                  background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
                }}
              >
                {tr("선택", "Optional")}
              </span>
            </div>
            <input
              type="password"
              placeholder={tr("알림 봇 토큰 (선택)", "Notification bot token (optional)")}
              value={notifyToken}
              onChange={(e) => setNotifyToken(e.target.value)}
              className={inputStyle}
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            />
            {notifyBotInfo?.valid && (
              <div className="flex items-center gap-2">
                <span className="text-xs text-emerald-400">✓ {notifyBotInfo.bot_name}</span>
                <a
                  href={makeInviteUrl(notifyBotInfo.bot_id!, PERMS.notify)}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="text-xs px-2 py-0.5 rounded-md border transition-colors hover:opacity-100"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)",
                    background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent)",
                    color: "var(--th-text-primary)",
                  }}
                >
                  {tr("서버에 초대 →", "Invite to server →")}
                </a>
              </div>
            )}
            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr("자동 설정 권한: Send Messages", "Auto-configured: Send Messages")}
            </div>
          </div>

          {/* Actions */}
          <ChecklistPanel title={tr("Step 1 체크리스트", "Step 1 checklist")} items={step1Checklist} />

          <div className={actionRow}>
            <button type="button"
              onClick={() => void validateStep1()}
              disabled={!commandBots[0]?.token || !announceToken || validating}
              className={btnSecondary}
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)",
                color: "var(--th-text-primary)",
              }}
            >
              {validating ? tr("검증 중...", "Validating...") : tr("토큰 검증", "Validate Tokens")}
            </button>
            {/* "다음" only after all required bots are validated */}
            {commandBots[0]?.botInfo?.valid && announceBotInfo?.valid ? (
              <button type="button" onClick={() => goToStep(2)} className={btnPrimary}>
                {tr("다음", "Next")}
              </button>
            ) : (
              <button type="button" onClick={() => goToStep(2)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
                {tr("나중에 입력", "Skip for now")}
              </button>
            )}
          </div>
          {!commandBotsReady || !announceReady ? (
            <p className="text-xs leading-5" style={{ color: "#fde68a" }}>
              {tr(
                "이 단계 검증을 건너뛰면 Step 5에서 완료가 막힙니다. 최소한 실행 봇과 통신 봇 검증, 서버 초대까지 끝내는 편이 안전합니다.",
                "Skipping validation here will block completion in Step 5. It is safer to finish command/communication bot validation and server invites first.",
              )}
            </p>
          ) : null}
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {tr("토큰은 나중에 설정 파일에서 직접 입력할 수 있습니다: ", "Tokens can be set later in: ")}
            <code
              className="text-xs px-1 py-0.5 rounded"
              style={{ background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)" }}
            >
              ~/.adk/release/agentdesk.yaml
            </code>
          </p>
        </div>
      )}

      {/* ──────────────── Step 2: Provider Verification ──────────────── */}
      {step === 2 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
              {tr("AI 프로바이더 확인", "AI Provider Verification")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "에이전트가 작업하려면 터미널에서 AI 프로바이더에 로그인되어 있어야 합니다.",
                "Agents need the AI provider CLI to be installed and logged in on this machine.",
              )}
            </p>
          </div>

          <div className="space-y-3">
            {[...new Set(commandBots.map((b) => b.provider))].map((provider) => {
              const status = providerStatuses[provider];
              const name = providerCliName(provider);
              return (
                <div key={provider} className="rounded-xl p-4 border space-y-2" style={{ borderColor: borderLight }}>
                  <div className="flex items-center gap-3">
                    <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>{name}</span>
                    {checkingProviders && (
                      <span className="text-xs animate-pulse" style={{ color: "var(--th-text-muted)" }}>
                        {tr("확인 중...", "Checking...")}
                      </span>
                    )}
                  </div>

                  {status && !checkingProviders && (
                    <div className="space-y-1">
                      <div className="flex items-center gap-2 text-sm">
                        <span>{status.installed ? "✅" : "❌"}</span>
                        <span style={{ color: status.installed ? "#86efac" : "#fca5a5" }}>
                          {status.installed
                            ? tr("설치됨", "Installed") + (status.version ? ` (${status.version})` : "")
                            : tr("설치되지 않음", "Not installed")}
                        </span>
                      </div>
                      {status.installed && (
                        <div className="flex items-center gap-2 text-sm">
                          <span>{status.logged_in ? "✅" : "⚠️"}</span>
                          <span style={{ color: status.logged_in ? "#86efac" : "#fde68a" }}>
                            {status.logged_in
                              ? tr("로그인됨", "Logged in")
                              : tr("로그인 필요", "Login required")}
                          </span>
                        </div>
                      )}
                    </div>
                  )}

                  {status && !status.installed && (
                    <div className="rounded-lg p-3 text-xs space-y-1" style={{ backgroundColor: "rgba(251,191,36,0.08)" }}>
                      <div style={{ color: "#fde68a" }}>
                        {providerInstallHint(provider, isKo)}
                      </div>
                      <div style={{ color: "var(--th-text-muted)" }}>
                        {providerLoginHint(provider, isKo)}
                      </div>
                    </div>
                  )}

                  {status && status.installed && !status.logged_in && (
                    <div className="rounded-lg p-3 text-xs" style={{ backgroundColor: "rgba(251,191,36,0.08)" }}>
                      <div style={{ color: "#fde68a" }}>
                        {tr("터미널에서 로그인하세요:", "Login in terminal:")}
                        <code
                          className="ml-2 px-1.5 py-0.5 rounded"
                          style={{ background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)" }}
                        >
                          {providerLoginCommand(provider)}
                        </code>
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </div>

          <ChecklistPanel title={tr("Step 2 체크리스트", "Step 2 checklist")} items={step2Checklist} />

          <div className={actionRow}>
            <button type="button" onClick={() => goToStep(1)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button type="button" onClick={() => void checkProviders()} disabled={checkingProviders} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("다시 확인", "Re-check")}
            </button>
            <button type="button"
              onClick={() => goToStep(3)}
              className={providersReady ? btnPrimary : btnSecondary}
              style={providersReady ? undefined : { borderColor: "rgba(148,163,184,0.3)" }}
            >
              {providersReady ? tr("다음", "Next") : tr("확인 없이 계속", "Continue anyway")}
            </button>
          </div>
          {!providersReady ? (
            <p className="text-xs leading-5" style={{ color: "#fde68a" }}>
              {tr(
                "CLI 설치나 로그인이 안 된 상태로 넘어가면 Step 5에서 완료할 수 없습니다. 지금은 역할 구성만 먼저 이어서 준비하는 용도입니다.",
                "If installation or login is still missing, Step 5 cannot complete. Continue only if you want to prepare the rest of the setup first.",
              )}
            </p>
          ) : null}
        </div>
      )}

      {/* ──────────────── Step 3: Agent Selection ──────────────── */}
      {step === 3 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
              {tr("역할 프리셋과 에이전트 구성", "Role Presets & Agents")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "역할별 프리셋으로 팀을 빠르게 시작하거나, 필요한 에이전트를 직접 추가할 수 있습니다.",
                "Start from a role-based preset or add the exact agents you need.",
              )}
            </p>
          </div>

          {/* Template cards */}
          <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
            {TEMPLATES.map((tpl) => (
              <button type="button"
                key={tpl.key}
                onClick={() => selectTemplate(tpl.key)}
                className="rounded-xl p-4 border text-left transition-all hover:scale-[1.02]"
                style={{
                  borderColor: selectedTemplate === tpl.key ? "var(--th-accent-primary)" : borderLight,
                  backgroundColor:
                    selectedTemplate === tpl.key
                      ? "color-mix(in srgb, var(--th-accent-primary-soft) 82%, transparent)"
                      : "transparent",
                }}
              >
                <div className="text-2xl mb-2">{tpl.icon}</div>
                <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>{tr(tpl.name, tpl.nameEn)}</div>
                <div className="text-xs mt-1" style={{ color: "var(--th-text-muted)" }}>{tr(tpl.description, tpl.descriptionEn)}</div>
                <div className="text-xs mt-2" style={{ color: "var(--th-text-muted)" }}>
                  {tpl.agents.map((a) => tr(a.name, a.nameEn)).join(", ")}
                </div>
              </button>
            ))}
          </div>

          <div
            className="rounded-xl border p-4 space-y-2"
            style={{ borderColor: "rgba(99,102,241,0.2)", backgroundColor: "rgba(99,102,241,0.08)" }}
          >
            <div className="text-sm font-medium" style={{ color: "#c7d2fe" }}>
              {tr("커스텀 에이전트의 AI 프롬프트 초안 만들기", "Create AI prompt drafts for custom agents")}
            </div>
            <div className="text-xs space-y-1" style={{ color: "var(--th-text-secondary)" }}>
              <div>{tr("1. 이름과 한줄 설명을 적고 에이전트를 추가합니다.", "1. Add an agent with a name and one-line description.")}</div>
              <div>{tr("2. 카드 펼치기 → `AI 초안 생성`으로 시스템 프롬프트 뼈대를 만듭니다.", "2. Expand the card and click `AI Draft` to build the first system prompt draft.")}</div>
              <div>{tr("3. 담당 업무, 금지사항, 말투를 직접 보정하면 품질이 크게 올라갑니다.", "3. Refine responsibilities, guardrails, and tone for a much better final prompt.")}</div>
            </div>
          </div>

          {/* Agent list (from template or custom) */}
          {agents.length > 0 && (
            <div className="space-y-2">
              <div className="text-xs font-medium" style={{ color: "var(--th-text-secondary)" }}>
                {tr(`${agents.length}개 에이전트`, `${agents.length} agents`)}
              </div>
              {agents.map((agent) => (
                <div key={agent.id} className="rounded-xl border overflow-hidden" style={{ borderColor: borderLight }}>
                  <div
                    className="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-surface-subtle"
                    onClick={() => setExpandedAgent(expandedAgent === agent.id ? null : agent.id)}
                  >
                    <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                      {tr(agent.name, agent.nameEn || agent.name)}
                    </span>
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {tr(agent.description, agent.descriptionEn || agent.description)}
                    </span>
                    <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {expandedAgent === agent.id ? "▲" : "▼"}
                    </span>
                    {agent.custom && (
                      <button type="button"
                        onClick={(e) => { e.stopPropagation(); removeAgent(agent.id); }}
                        className="text-xs text-red-400 hover:text-red-300"
                      >
                        {tr("삭제", "Del")}
                      </button>
                    )}
                  </div>
                  {expandedAgent === agent.id && (
                    <div className="px-4 pb-3 space-y-2 border-t" style={{ borderColor: borderLight }}>
                      <div className="flex items-center gap-2 pt-2">
                        <label className={labelStyle} style={{ color: "var(--th-text-secondary)" }}>
                          {tr("시스템 프롬프트", "System Prompt")}
                        </label>
                        {agent.custom && (
                          <button type="button"
                            onClick={() => void generateAiPrompt(agent.id)}
                            disabled={generatingPrompt}
                            className={btnSmall}
                            style={{
                              borderColor: "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)",
                              color: "var(--th-text-primary)",
                            }}
                          >
                            {generatingPrompt ? tr("생성 중...", "Generating...") : tr("AI 초안 생성", "AI Draft")}
                          </button>
                        )}
                      </div>
                      <textarea
                        value={agent.prompt}
                        onChange={(e) => {
                          setAgents((prev) =>
                            prev.map((a) => (a.id === agent.id ? { ...a, prompt: e.target.value } : a)),
                          );
                        }}
                        rows={6}
                        className="w-full rounded-lg px-3 py-2 text-xs bg-surface-subtle border resize-y"
                        style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                        placeholder={tr("에이전트의 역할과 행동 규칙을 정의합니다", "Define the agent's role and behavior")}
                      />
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}

          {/* Custom agent creation — single row */}
          <div className="space-y-2">
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
              <input
                type="text"
                placeholder={tr("에이전트 이름", "Agent name")}
                value={customName}
                onChange={(e) => setCustomName(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
              <input
                type="text"
                placeholder={tr("한줄 설명", "Brief description")}
                value={customDesc}
                onChange={(e) => setCustomDesc(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
            </div>
            <div className="grid grid-cols-1 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] gap-2">
              <input
                type="text"
                placeholder={tr("영문 이름 (선택)", "English name (optional)")}
                value={customNameEn}
                onChange={(e) => setCustomNameEn(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
              <input
                type="text"
                placeholder={tr("영문 설명 (선택)", "English description (optional)")}
                value={customDescEn}
                onChange={(e) => setCustomDescEn(e.target.value)}
                className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
              />
              <button type="button"
                onClick={addCustomAgent}
                disabled={!customName.trim()}
                className="w-full sm:w-auto px-4 py-2 rounded-lg text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-40 transition-colors whitespace-nowrap"
              >
                + {tr("추가", "Add")}
              </button>
            </div>
            <p className="text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "영문 필드를 비워두면 현재 입력값을 그대로 사용합니다. 다국어 대시보드에서 별도 표기가 필요할 때만 채우면 됩니다.",
                "Leave the English fields empty to reuse the current values. Fill them only when you need separate wording in English mode.",
              )}
            </p>
          </div>

          <ChecklistPanel title={tr("Step 3 체크리스트", "Step 3 checklist")} items={step3Checklist} />

          <div className={actionRow}>
            <button type="button" onClick={() => goToStep(2)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button type="button" onClick={() => goToStep(4)} disabled={agents.length === 0} className={btnPrimary}>
              {tr("다음", "Next")} ({agents.length}{tr("개 에이전트", " agents")})
            </button>
          </div>
        </div>
      )}

      {/* ──────────────── Step 4: Channel Setup ──────────────── */}
      {step === 4 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
              {tr("채널 설정", "Channel Setup")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "각 에이전트가 사용할 Discord 채널을 실제 서버 기준으로 설정합니다. 기존 채널을 재사용하거나, 추천 이름으로 새 채널을 만들 수 있습니다.",
                "Configure real Discord channels for each agent. Reuse existing channels or create new ones from the recommended names.",
              )}
            </p>
          </div>

          {/* Guild selection */}
          {guilds.length > 0 && (
            <div>
              <label className={labelStyle} style={{ color: "var(--th-text-secondary)" }}>
                {tr("Discord 서버", "Discord Server")}
              </label>
              {guilds.length === 1 ? (
                <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                  {guilds[0].name}
                </div>
              ) : (
                <select
                  value={selectedGuild}
                  onChange={(e) => setSelectedGuild(e.target.value)}
                  className={inputStyle}
                  style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                >
                  <option value="">{tr("서버 선택", "Select server")}</option>
                  {guilds.map((g) => (
                    <option key={g.id} value={g.id}>{g.name}</option>
                  ))}
                </select>
              )}
            </div>
          )}

          {guilds.length === 0 && (
            <div className="rounded-xl p-4 text-sm" style={{ backgroundColor: "rgba(251,191,36,0.08)", border: "1px solid rgba(251,191,36,0.2)" }}>
              <div style={{ color: "#fde68a" }}>
                {tr(
                  "통신 봇이 어떤 서버에도 초대되지 않았거나 토큰 검증이 끝나지 않았습니다. 실제 채널 생성 보장을 위해 Step 1로 돌아가 봇 초대부터 완료하세요.",
                  "The communication bot is not invited to any server yet, or token validation is incomplete. Go back to Step 1 and finish the invite before continuing.",
                )}
              </div>
            </div>
          )}

          {/* Channel assignments */}
          <div className="space-y-2">
            {channelAssignments.map((ca, i) => (
              <div key={ca.agentId} className="rounded-xl p-3 border space-y-2" style={{ borderColor: "rgba(148,163,184,0.15)" }}>
                <div className="flex items-center gap-2">
                  <span className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>{ca.agentName}</span>
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>→</span>
                </div>
                <div className="flex flex-col sm:flex-row gap-2">
                  {guild && guild.channels.length > 0 ? (
                    <select
                      value={ca.channelId}
                      onChange={(e) => {
                        const ch = guild.channels.find((c) => c.id === e.target.value);
                        setChannelAssignments((prev) => {
                          const copy = [...prev];
                          copy[i] = {
                            ...ca,
                            channelId: e.target.value,
                            channelName: ch?.name || ca.recommendedName,
                          };
                          return copy;
                        });
                      }}
                      className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                      style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                    >
                      <option value="">{tr(`새 채널: #${ca.recommendedName}`, `New: #${ca.recommendedName}`)}</option>
                      {guild.channels.map((ch) => (
                        <option key={ch.id} value={ch.id}>#{ch.name}</option>
                      ))}
                    </select>
                  ) : (
                    <input
                      type="text"
                      value={ca.channelName || ca.recommendedName}
                      readOnly
                      className="flex-1 rounded-lg px-3 py-2 text-sm bg-surface-subtle border"
                      style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
                      placeholder={ca.recommendedName}
                    />
                  )}
                </div>
              </div>
            ))}
          </div>

          {guild && (
            <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "\"새 채널\"을 선택하면 통신 봇이 온보딩 완료 시 해당 채널을 자동 생성합니다.",
                "Selecting \"New\" makes the communication bot create that channel automatically during onboarding.",
              )}
            </p>
          )}

          {!guild && (
            <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "서버를 선택하면 각 추천 채널명을 기존 채널에 연결하거나 새 채널로 생성할 수 있습니다.",
                "Once a server is selected, each recommended channel can be linked to an existing channel or created as a new one.",
              )}
            </p>
          )}

          <ChecklistPanel title={tr("Step 4 체크리스트", "Step 4 checklist")} items={step4Checklist} />

          <div className={actionRow}>
            <button type="button" onClick={() => goToStep(3)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button type="button" onClick={() => goToStep(5)} disabled={!hasSelectedGuild || !channelAssignmentsReady} className={btnPrimary}>
              {tr("다음", "Next")}
            </button>
          </div>
        </div>
      )}

      {/* ──────────────── Step 5: Owner + Confirm ──────────────── */}
      {step === 5 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 ref={stepHeadingRef} tabIndex={-1} className="text-lg font-semibold outline-none" style={{ color: "var(--th-text-heading)" }}>
              {tr("소유자 설정 및 확인", "Owner Setup & Confirm")}
            </h2>
          </div>

          {/* Owner section with detailed explanation */}
          <div className="rounded-xl p-4 border space-y-3" style={{ borderColor: borderLight }}>
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                {tr("Discord 소유자 ID", "Discord Owner ID")}
              </span>
              <Tip text={tr(
                "소유자는 에이전트에게 직접 명령할 수 있고,\n관리자 기능에 접근할 수 있습니다.\n비워두면 처음 메시지를 보내는 사람이\n자동으로 소유자가 됩니다.",
                "The owner can command agents directly\nand access admin features.\nLeave blank to auto-register\nthe first message sender.",
              )} />
            </div>

            <input
              type="text"
              placeholder="123456789012345678"
              value={ownerId}
              onChange={(e) => setOwnerId(e.target.value)}
              className={inputStyle}
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            />

            <div
              className="rounded-lg p-3 text-xs space-y-2"
              style={{ background: "color-mix(in srgb, var(--th-accent-primary-soft) 64%, transparent)" }}
            >
              <div className="font-medium" style={{ color: "var(--th-text-primary)" }}>
                {tr("Discord 사용자 ID 찾는 방법", "How to find your Discord User ID")}
              </div>
              <ol className="list-decimal list-inside space-y-1" style={{ color: "var(--th-text-secondary)" }}>
                <li>{tr("Discord 앱 하단 ⚙️ 설정 → 고급 → 개발자 모드 활성화", "Discord Settings → Advanced → Enable Developer Mode")}</li>
                <li>{tr("왼쪽 사용자 목록에서 내 이름을 우클릭", "Right-click your name in the member list")}</li>
                <li>{tr("\"사용자 ID 복사\" 클릭 → 위 입력란에 붙여넣기", "Click \"Copy User ID\" → Paste above")}</li>
              </ol>
              <div className="mt-1" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "17~20자리 숫자입니다 (예: 123456789012345678)",
                  "It's a 17-20 digit number (e.g., 123456789012345678)",
                )}
              </div>
            </div>

            <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "소유자로 등록되면: 에이전트에게 직접 명령 가능 · 관리자 권한 활성화 · 시스템 알림 수신",
                "As owner: Direct commands to agents · Admin access · System notifications",
              )}
            </div>
          </div>

          {/* Summary */}
          <div className="rounded-xl p-4 border space-y-3" style={{ borderColor: borderLight }}>
            <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
              {tr("설정 요약", "Setup Summary")}
            </div>
            <div className="space-y-2 text-sm" style={{ color: "var(--th-text-primary)" }}>
              {/* Bots */}
              <div className="flex items-center gap-2">
                <span style={{ color: "var(--th-text-muted)" }}>{tr("실행 봇", "Command")}</span>
                <span>
                  {commandBots.map((b) => `${providerLabel(b.provider)}${b.botInfo?.bot_name ? ` (${b.botInfo.bot_name})` : ""}`).join(", ")}
                </span>
              </div>
              <div className="flex items-center gap-2">
                <span style={{ color: "var(--th-text-muted)" }}>{tr("통신 봇", "Comm")}</span>
                <span>{announceBotInfo?.bot_name || (announceToken ? tr("설정됨", "Set") : tr("미설정", "Not set"))}</span>
              </div>
              {notifyToken && (
                <div className="flex items-center gap-2">
                  <span style={{ color: "var(--th-text-muted)" }}>{tr("알림 봇", "Notify")}</span>
                  <span>{tr("설정됨", "Set")}</span>
                </div>
              )}

              {/* Guild */}
              {selectedGuild && (
                <div className="flex items-center gap-2">
                  <span style={{ color: "var(--th-text-muted)" }}>{tr("서버", "Server")}</span>
                  <span>{guilds.find((g) => g.id === selectedGuild)?.name || selectedGuild}</span>
                </div>
              )}

              {/* Agents & Channels */}
              <div className="border-t pt-2 mt-2" style={{ borderColor: borderLight }}>
                <div className="text-xs font-medium mb-1" style={{ color: "var(--th-text-muted)" }}>
                  {tr(`에이전트 → 채널 (${channelAssignments.length}개)`, `Agents → Channels (${channelAssignments.length})`)}
                </div>
                {channelAssignments.map((ca) => (
                  <div key={ca.agentId} className="text-xs py-0.5" style={{ color: "var(--th-text-secondary)" }}>
                    {ca.agentName} → #{ca.channelName || ca.recommendedName}
                  </div>
                ))}
              </div>
            </div>
          </div>

          <div className="rounded-xl p-4 border space-y-3" style={{ borderColor: borderLight }}>
            <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
              {tr("완료 시 실제로 적용되는 항목", "What completion actually applies")}
            </div>
            <div className="space-y-2">
              {applySummary.map((item) => (
                <div
                  key={item.key}
                  className="rounded-lg border px-3 py-2"
                  style={{ borderColor: "rgba(148,163,184,0.16)", backgroundColor: "rgba(15,23,42,0.28)" }}
                >
                  <div className="text-sm font-medium" style={{ color: "var(--th-text-primary)" }}>
                    {item.label}
                  </div>
                  <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {item.detail}
                  </div>
                </div>
              ))}
            </div>
          </div>

          {hasExistingSetup && (
            <div
              className="rounded-xl border p-4 space-y-3"
              style={{ borderColor: "rgba(251,191,36,0.24)", backgroundColor: "rgba(251,191,36,0.08)" }}
            >
              <div className="text-sm font-medium" style={{ color: "#fde68a" }}>
                {tr("기존 구성이 감지된 상태에서 다시 실행 중입니다.", "You are re-running onboarding on top of an existing setup.")}
              </div>
              <p className="text-xs leading-5" style={{ color: "var(--th-text-secondary)" }}>
                {tr(
                  "현재 API는 기존 에이전트/채널 구성을 이 화면에 프리필하지 않습니다. 같은 role_id를 다시 적용하면 기존 agent row와 채널 매핑이 갱신될 수 있으니, 아래 요약을 먼저 확인한 뒤 실행하세요.",
                  "The current API does not prefill the existing agent/channel layout in this screen. Re-applying the same role IDs can update existing agent rows and channel mappings, so review the summary before running completion.",
                )}
              </p>
              <label className="flex items-start gap-3 text-sm" style={{ color: "var(--th-text-primary)" }}>
                <input
                  type="checkbox"
                  checked={confirmRerunOverwrite}
                  onChange={(event) => setConfirmRerunOverwrite(event.target.checked)}
                  className="mt-0.5 h-4 w-4 rounded border"
                />
                <span>
                  {tr(
                    "기존 role_id 에이전트와 채널 매핑이 다시 적용될 수 있다는 점을 이해했고, 현재 요약 기준으로 재실행합니다.",
                    "I understand that existing role-based agents and channel mappings may be applied again, and I want to re-run onboarding with the current summary.",
                  )}
                </span>
              </label>
            </div>
          )}

          <ChecklistPanel
            title={
              completionChecklist
                ? tr("실제 적용 결과", "Applied setup result")
                : tr("Step 5 체크리스트", "Step 5 checklist")
            }
            items={completionChecklist ?? step5Checklist}
          />

          {completionChecklist && (
            <div className="text-xs leading-5" style={{ color: "#86efac" }}>
              {tr(
                "설정이 실제로 저장되었습니다. 체크리스트를 검토한 뒤 직접 대시보드로 돌아가면 됩니다.",
                "Setup has been applied. Review the checklist, then return to the dashboard when you are ready.",
              )}
            </div>
          )}

          <div className={actionRow}>
            {completionChecklist ? (
              <button type="button"
                onClick={() => {
                  clearOnboardingDraft();
                  onComplete();
                }}
                className={btnPrimary}
              >
                {tr("대시보드로 돌아가기", "Return to dashboard")}
              </button>
            ) : (
              <>
                <button type="button" onClick={() => goToStep(4)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
                  {tr("이전", "Back")}
                </button>
                <button type="button" onClick={() => void handleComplete()} disabled={completing || !completionReady} className={btnPrimary}>
                  {completing ? tr("설정 중...", "Setting up...") : tr("설정 완료", "Complete Setup")}
                </button>
              </>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
