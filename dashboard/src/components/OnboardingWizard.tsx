import { useState, useEffect, useCallback } from "react";

// ── Types ─────────────────────────────────────────────

interface BotInfo {
  valid: boolean;
  bot_id?: string;
  bot_name?: string;
  error?: string;
}

interface CommandBotEntry {
  provider: "claude" | "codex" | "gemini";
  token: string;
  botInfo: BotInfo | null;
}

const COMMAND_PROVIDERS = ["claude", "codex", "gemini"] as const;

function providerSuffix(provider: CommandBotEntry["provider"]) {
  switch (provider) {
    case "claude":
      return "cc";
    case "codex":
      return "cdx";
    case "gemini":
      return "gm";
  }
}

function providerLabel(provider: CommandBotEntry["provider"]) {
  switch (provider) {
    case "claude":
      return "Claude";
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
  }
}

function providerCliName(provider: CommandBotEntry["provider"]) {
  switch (provider) {
    case "claude":
      return "Claude Code";
    case "codex":
      return "Codex CLI";
    case "gemini":
      return "Gemini CLI";
  }
}

function providerInstallHint(provider: CommandBotEntry["provider"], isKo: boolean) {
  switch (provider) {
    case "claude":
      return isKo ? "설치: npm install -g @anthropic-ai/claude-code" : "Install: npm install -g @anthropic-ai/claude-code";
    case "codex":
      return isKo ? "설치: npm install -g @openai/codex" : "Install: npm install -g @openai/codex";
    case "gemini":
      return isKo ? "설치: npm install -g @google/gemini-cli" : "Install: npm install -g @google/gemini-cli";
  }
}

function providerLoginHint(provider: CommandBotEntry["provider"], isKo: boolean) {
  switch (provider) {
    case "claude":
      return isKo ? "로그인: claude login" : "Login: claude login";
    case "codex":
      return isKo ? "로그인: codex login" : "Login: codex login";
    case "gemini":
      return isKo ? "로그인: gemini" : "Login: gemini";
  }
}

interface AgentDef {
  id: string;
  name: string;
  nameEn?: string;
  description: string;
  descriptionEn?: string;
  prompt: string;
  custom?: boolean;
}

interface ChannelAssignment {
  agentId: string;
  agentName: string;
  recommendedName: string;
  channelId: string;
  channelName: string;
}

interface Guild {
  id: string;
  name: string;
  channels: Array<{ id: string; name: string; category_id?: string }>;
}

interface ProviderStatus {
  installed: boolean;
  logged_in: boolean;
  version?: string;
}

interface OnboardingStatusResponse {
  owner_id?: string;
  guild_id?: string;
  bot_tokens?: {
    command?: string;
    announce?: string;
    notify?: string;
    command2?: string;
  };
  bot_providers?: {
    command?: CommandBotEntry["provider"];
    command2?: CommandBotEntry["provider"];
  };
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
    key: "household",
    name: "가사 및 일정 도우미",
    nameEn: "Household & Schedule",
    icon: "🏠",
    description: "가정생활을 돕는 AI 에이전트 팀",
    descriptionEn: "AI agent team for household management",
    agents: [
      {
        id: "scheduler",
        name: "일정봇",
        nameEn: "Scheduler",
        description: "가족 일정 관리, 리마인더, 약속 조율",
        descriptionEn: "Family scheduling, reminders, appointments",
        prompt:
          "당신은 가족 일정 관리 도우미입니다. 가족 구성원의 일정을 조율하고, 중요한 약속을 리마인드합니다. 충돌하는 일정이 있으면 미리 알려주고 대안을 제안합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 일정 변경은 반드시 확인 후 반영합니다",
      },
      {
        id: "household",
        name: "가사봇",
        nameEn: "Household",
        description: "장보기 목록, 청소 루틴, 가사 분담 관리",
        descriptionEn: "Shopping lists, cleaning routines, chore management",
        prompt:
          "당신은 가사 관리 도우미입니다. 장보기 목록을 관리하고, 청소 루틴을 설정하며, 가족 간 가사 분담을 조율합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 실용적이고 구체적인 제안을 합니다",
      },
      {
        id: "cooking",
        name: "요리봇",
        nameEn: "Cooking",
        description: "레시피 추천, 식단 계획, 영양 관리",
        descriptionEn: "Recipe suggestions, meal planning, nutrition",
        prompt:
          "당신은 요리 및 식단 관리 도우미입니다. 가족의 식성과 영양 균형을 고려한 레시피를 추천하고, 주간 식단을 계획합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 재료와 조리법을 명확하게 안내합니다",
      },
      {
        id: "health",
        name: "건강봇",
        nameEn: "Health",
        description: "건강 관리, 운동 추천, 복약 알림",
        descriptionEn: "Health tracking, exercise, medication reminders",
        prompt:
          "당신은 가족 건강 관리 도우미입니다. 운동 루틴을 추천하고, 복약 시간을 알려주며, 건강 관련 정보를 제공합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 의학적 판단은 전문가 상담을 권유합니다",
      },
      {
        id: "shopping",
        name: "쇼핑봇",
        nameEn: "Shopping",
        description: "가격 비교, 쿠폰 검색, 온라인 구매 도우미",
        descriptionEn: "Price comparison, coupons, online shopping",
        prompt:
          "당신은 스마트 쇼핑 도우미입니다. 제품 가격을 비교하고, 할인 정보와 쿠폰을 찾아주며, 효율적인 구매를 돕습니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 가성비 중심으로 추천합니다",
      },
    ],
  },
  {
    key: "startup",
    name: "소규모 스타트업",
    nameEn: "Small Startup",
    icon: "🚀",
    description: "스타트업 팀을 위한 AI 에이전트 팀",
    descriptionEn: "AI agent team for startup teams",
    agents: [
      {
        id: "pm",
        name: "PM",
        nameEn: "PM",
        description: "프로젝트 관리, 스프린트 계획, 이슈 추적",
        descriptionEn: "Project management, sprint planning, issue tracking",
        prompt:
          "당신은 프로젝트 매니저입니다. 스프린트를 계획하고, 이슈를 추적하며, 팀원 간 작업을 조율합니다. 우선순위를 관리하고 데드라인을 지키도록 돕습니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 결정과 근거를 명확히 전달합니다",
      },
      {
        id: "developer",
        name: "개발자",
        nameEn: "Developer",
        description: "코드 작성, 버그 수정, 코드 리뷰",
        descriptionEn: "Coding, bug fixes, code review",
        prompt:
          "당신은 소프트웨어 개발자입니다. 기능 구현, 버그 수정, 코드 리뷰를 수행합니다. 클린 코드와 테스트를 중시합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 코드 변경은 이유와 함께 설명합니다",
      },
      {
        id: "designer",
        name: "디자이너",
        nameEn: "Designer",
        description: "UI/UX 디자인, 프로토타입, 디자인 시스템",
        descriptionEn: "UI/UX design, prototyping, design system",
        prompt:
          "당신은 UI/UX 디자이너입니다. 사용자 중심의 인터페이스를 설계하고, 프로토타입을 제작하며, 일관된 디자인 시스템을 유지합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 디자인 결정의 근거를 설명합니다",
      },
      {
        id: "qa",
        name: "QA",
        nameEn: "QA",
        description: "테스트 자동화, 품질 관리, 버그 리포트",
        descriptionEn: "Test automation, quality assurance, bug reports",
        prompt:
          "당신은 QA 엔지니어입니다. 테스트 케이스를 작성하고, 자동화 테스트를 구축하며, 발견된 버그를 체계적으로 보고합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 재현 경로를 정확히 기술합니다",
      },
      {
        id: "marketing",
        name: "마케팅",
        nameEn: "Marketing",
        description: "마케팅 전략, SNS 관리, 콘텐츠 제작",
        descriptionEn: "Marketing strategy, social media, content creation",
        prompt:
          "당신은 마케팅 담당자입니다. 마케팅 전략을 수립하고, SNS 콘텐츠를 제작하며, 고객 분석과 캠페인을 관리합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 데이터 기반으로 의사결정합니다",
      },
    ],
  },
  {
    key: "office",
    name: "사무업무",
    nameEn: "Office Work",
    icon: "🏢",
    description: "사무 업무를 자동화하는 AI 에이전트 팀",
    descriptionEn: "AI agent team for office automation",
    agents: [
      {
        id: "schedule-mgr",
        name: "일정관리",
        nameEn: "Schedule Manager",
        description: "회의 일정 조율, 캘린더 관리, 알림",
        descriptionEn: "Meeting scheduling, calendar management, reminders",
        prompt:
          "당신은 일정 관리 비서입니다. 회의 일정을 조율하고, 캘린더를 관리하며, 중요한 일정을 리마인드합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 일정 변경 시 참석자 전원에게 알립니다",
      },
      {
        id: "email-asst",
        name: "이메일",
        nameEn: "Email",
        description: "이메일 작성, 분류, 자동 응답",
        descriptionEn: "Email drafting, sorting, auto-reply",
        prompt:
          "당신은 이메일 관리 비서입니다. 이메일을 작성하고, 수신 메일을 분류하며, 일상적인 문의에 자동 응답을 작성합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 비즈니스 메일은 격식체로 작성합니다",
      },
      {
        id: "doc-writer",
        name: "문서작성",
        nameEn: "Document Writer",
        description: "보고서, 제안서, 회의록 작성",
        descriptionEn: "Reports, proposals, meeting notes",
        prompt:
          "당신은 문서 작성 전문가입니다. 보고서, 제안서, 회의록 등 비즈니스 문서를 깔끔하고 전문적으로 작성합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 명확한 구조와 간결한 문장을 사용합니다",
      },
      {
        id: "researcher",
        name: "리서치",
        nameEn: "Researcher",
        description: "시장 조사, 자료 수집, 경쟁사 분석",
        descriptionEn: "Market research, data collection, competitive analysis",
        prompt:
          "당신은 리서치 전문가입니다. 시장 동향을 조사하고, 경쟁사를 분석하며, 의사결정에 필요한 자료를 수집하고 정리합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 출처를 명시하고 객관적으로 분석합니다",
      },
      {
        id: "data-analyst",
        name: "데이터분석",
        nameEn: "Data Analyst",
        description: "데이터 분석, 시각화, 인사이트 도출",
        descriptionEn: "Data analysis, visualization, insights",
        prompt:
          "당신은 데이터 분석가입니다. 비즈니스 데이터를 분석하고, 시각화 자료를 제작하며, 실행 가능한 인사이트를 도출합니다.\n\n## 소통 원칙\n- 한국어로 소통합니다\n- 수치와 근거를 명확히 제시합니다",
      },
    ],
  },
];

// ── Helper: Tooltip ───────────────────────────────────

function Tip({ text }: { text: string }) {
  return (
    <span className="relative group inline-block ml-1 cursor-help">
      <span
        className="inline-flex items-center justify-center w-4 h-4 rounded-full text-[10px] font-bold"
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

// ── Main Component ────────────────────────────────────

export default function OnboardingWizard({ isKo, onComplete }: Props) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  // Step control
  const [step, setStep] = useState(1);
  const TOTAL_STEPS = 5;

  // Step 1: Bot tokens
  const [commandBots, setCommandBots] = useState<CommandBotEntry[]>([
    { provider: "claude", token: "", botInfo: null },
  ]);
  const [announceToken, setAnnounceToken] = useState("");
  const [notifyToken, setNotifyToken] = useState("");
  const [announceBotInfo, setAnnounceBotInfo] = useState<BotInfo | null>(null);
  const [notifyBotInfo, setNotifyBotInfo] = useState<BotInfo | null>(null);
  const [validating, setValidating] = useState(false);

  // Step 2: Provider verification
  const [providerStatuses, setProviderStatuses] = useState<Record<string, ProviderStatus>>({});
  const [checkingProviders, setCheckingProviders] = useState(false);

  // Step 3: Agent selection
  const [selectedTemplate, setSelectedTemplate] = useState<string | null>(null);
  const [agents, setAgents] = useState<AgentDef[]>([]);
  const [customName, setCustomName] = useState("");
  const [customDesc, setCustomDesc] = useState("");
  const [generatingPrompt, setGeneratingPrompt] = useState(false);
  const [expandedAgent, setExpandedAgent] = useState<string | null>(null);

  // Step 4: Channel setup
  const [guilds, setGuilds] = useState<Guild[]>([]);
  const [selectedGuild, setSelectedGuild] = useState("");
  const [channelAssignments, setChannelAssignments] = useState<ChannelAssignment[]>([]);

  // Step 5: Owner
  const [ownerId, setOwnerId] = useState("");
  const [completing, setCompleting] = useState(false);
  const [error, setError] = useState("");

  // Get primary provider from first command bot
  const primaryProvider = commandBots[0]?.provider ?? "claude";

  // Load existing config for pre-fill
  useEffect(() => {
    void fetch("/api/onboarding/status", { credentials: "include" })
      .then((r) => r.json())
      .then((d: OnboardingStatusResponse) => {
        if (d.owner_id) setOwnerId(d.owner_id);
        if (d.guild_id) setSelectedGuild(d.guild_id);
        const commandToken = d.bot_tokens?.command;
        const command2Token = d.bot_tokens?.command2;
        if (commandToken) {
          setCommandBots((prev) => {
            const copy = [...prev];
            copy[0] = {
              ...copy[0],
              provider: d.bot_providers?.command ?? copy[0].provider,
              token: commandToken,
            };
            return copy;
          });
        }
        if (command2Token) {
          setCommandBots((prev) => [
            ...prev,
            {
              provider: d.bot_providers?.command2 ?? COMMAND_PROVIDERS.find((provider) => provider !== prev[0].provider) ?? "codex",
              token: command2Token,
              botInfo: null,
            },
          ]);
        }
        if (d.bot_tokens?.announce) setAnnounceToken(d.bot_tokens.announce);
        if (d.bot_tokens?.notify) setNotifyToken(d.bot_tokens.notify);
      })
      .catch(() => {});
  }, []);

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
    const token = commandBots[0]?.token || announceToken;
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
      setChannelAssignments(
        agents.map((a) => ({
          agentId: a.id,
          agentName: a.name,
          recommendedName: `${a.id}-${suffix}`,
          channelId: "",
          channelName: `${a.id}-${suffix}`,
        })),
      );
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
        description: desc,
        prompt,
        custom: true,
      },
    ]);
    setCustomName("");
    setCustomDesc("");
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
    setCompleting(true);
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
        onComplete();
      } else {
        setError(d.error || tr("설정 저장 실패", "Failed to save"));
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
  const inputStyle = "w-full rounded-xl px-4 py-3 text-sm bg-white/5 border";
  const btnPrimary =
    "px-6 py-3 rounded-xl text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-50 transition-colors";
  const btnSecondary =
    "px-6 py-3 rounded-xl text-sm font-medium border text-white/70 hover:text-white transition-colors";
  const btnSmall =
    "px-3 py-1.5 rounded-lg text-xs font-medium border transition-colors";
  const labelStyle = "text-xs font-medium block mb-1";
  const borderLight = "rgba(148,163,184,0.2)";
  const borderInput = "rgba(148,163,184,0.24)";

  const guild = guilds.find((g) => g.id === selectedGuild);

  // ── Render ──────────────────────────────────────────

  return (
    <div className="max-w-2xl mx-auto p-4 sm:p-8 space-y-6">
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
                backgroundColor: s <= step ? "#818cf8" : "rgba(148,163,184,0.3)",
              }}
            />
          ))}
        </div>
      </div>

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
            <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
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
          <div className="rounded-xl p-4 text-sm space-y-2" style={{ backgroundColor: "rgba(99,102,241,0.08)", border: `1px solid rgba(99,102,241,0.2)` }}>
            <div className="font-medium" style={{ color: "#a5b4fc" }}>
              {tr("봇 토큰을 얻는 방법", "How to get bot tokens")}
            </div>
            <ol className="list-decimal list-inside space-y-1" style={{ color: "var(--th-text-secondary)" }}>
              <li>
                <a href="https://discord.com/developers/applications" target="_blank" rel="noopener noreferrer" className="text-indigo-400 hover:text-indigo-300 underline">
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
                "에이전트의 AI 세션을 실행하는 봇입니다.\nDiscord에서 메시지를 받으면 이 봇이\nClaude Code, Codex CLI, 또는 Gemini CLI를 실행하여\n에이전트가 작업합니다.",
                "Runs AI sessions for agents.\nWhen a message arrives, this bot\nlaunches Claude Code, Codex CLI, or Gemini CLI.",
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
                      <button
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
                          backgroundColor: bot.provider === p ? "rgba(99,102,241,0.3)" : "transparent",
                          color: bot.provider === p ? "#a5b4fc" : "var(--th-text-muted)",
                        }}
                      >
                        {providerLabel(p)}
                      </button>
                    ))}
                  </div>
                  {commandBots.length > 1 && (
                    <button
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
                      className="text-[11px] px-2 py-0.5 rounded-md bg-indigo-600/30 text-indigo-300 hover:bg-indigo-600/50 transition-colors"
                    >
                      {tr("서버에 초대 →", "Invite to server →")}
                    </a>
                  </div>
                )}
                <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  {tr(
                    `자동 설정 권한: Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads`,
                    `Auto-configured: Send Messages, Read Message History, Manage Messages, Create Public Threads, Send Messages in Threads`,
                  )}
                </div>
              </div>
            ))}

            {commandBots.length < 2 && (
              <button
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
              <span className="text-[10px] px-1.5 py-0.5 rounded bg-red-500/20 text-red-300 font-medium">
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
                  className="text-[11px] px-2 py-0.5 rounded-md bg-indigo-600/30 text-indigo-300 hover:bg-indigo-600/50 transition-colors"
                >
                  {tr("서버에 초대 (관리자 권한) →", "Invite to server (Admin) →")}
                </a>
              </div>
            )}
            <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
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
              <span className="text-[10px] px-1.5 py-0.5 rounded bg-white/10 font-medium" style={{ color: "var(--th-text-muted)" }}>
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
                  className="text-[11px] px-2 py-0.5 rounded-md bg-indigo-600/30 text-indigo-300 hover:bg-indigo-600/50 transition-colors"
                >
                  {tr("서버에 초대 →", "Invite to server →")}
                </a>
              </div>
            )}
            <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
              {tr("자동 설정 권한: Send Messages", "Auto-configured: Send Messages")}
            </div>
          </div>

          {/* Actions */}
          <div className="flex gap-3 pt-2">
            <button
              onClick={() => void validateStep1()}
              disabled={!commandBots[0]?.token || !announceToken || validating}
              className={btnSecondary}
              style={{ borderColor: "rgba(99,102,241,0.4)", color: "#a5b4fc" }}
            >
              {validating ? tr("검증 중...", "Validating...") : tr("토큰 검증", "Validate Tokens")}
            </button>
            {/* "다음" only after all required bots are validated */}
            {commandBots[0]?.botInfo?.valid && announceBotInfo?.valid ? (
              <button onClick={() => setStep(2)} className={btnPrimary}>
                {tr("다음", "Next")}
              </button>
            ) : (
              <button onClick={() => setStep(2)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
                {tr("나중에 입력", "Skip for now")}
              </button>
            )}
          </div>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {tr("토큰은 나중에 설정 파일에서 직접 입력할 수 있습니다: ", "Tokens can be set later in: ")}
            <code className="text-[11px] px-1 py-0.5 rounded bg-white/10">~/.adk/release/agentdesk.yaml</code>
          </p>
        </div>
      )}

      {/* ──────────────── Step 2: Provider Verification ──────────────── */}
      {step === 2 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
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
                        <code className="ml-2 px-1.5 py-0.5 rounded bg-white/10">{provider === "gemini" ? "gemini" : `${provider} login`}</code>
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </div>

          <div className="flex gap-3 pt-2">
            <button onClick={() => setStep(1)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button onClick={() => void checkProviders()} disabled={checkingProviders} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("다시 확인", "Re-check")}
            </button>
            <button onClick={() => setStep(3)} className={btnPrimary}>
              {tr("다음", "Next")}
            </button>
          </div>
        </div>
      )}

      {/* ──────────────── Step 3: Agent Selection ──────────────── */}
      {step === 3 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {tr("에이전트 선택", "Select Agents")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "용도에 맞는 에이전트 템플릿을 선택하거나, 커스텀 에이전트를 직접 만들 수 있습니다.",
                "Choose an agent template or create custom agents.",
              )}
            </p>
          </div>

          {/* Template cards */}
          <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
            {TEMPLATES.map((tpl) => (
              <button
                key={tpl.key}
                onClick={() => selectTemplate(tpl.key)}
                className="rounded-xl p-4 border text-left transition-all hover:scale-[1.02]"
                style={{
                  borderColor: selectedTemplate === tpl.key ? "#818cf8" : borderLight,
                  backgroundColor: selectedTemplate === tpl.key ? "rgba(99,102,241,0.1)" : "transparent",
                }}
              >
                <div className="text-2xl mb-2">{tpl.icon}</div>
                <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>{tr(tpl.name, tpl.nameEn)}</div>
                <div className="text-xs mt-1" style={{ color: "var(--th-text-muted)" }}>{tr(tpl.description, tpl.descriptionEn)}</div>
                <div className="text-[11px] mt-2" style={{ color: "var(--th-text-muted)" }}>
                  {tpl.agents.map((a) => tr(a.name, a.nameEn)).join(", ")}
                </div>
              </button>
            ))}
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
                    className="flex items-center gap-3 px-4 py-3 cursor-pointer hover:bg-white/5"
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
                      <button
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
                          <button
                            onClick={() => void generateAiPrompt(agent.id)}
                            disabled={generatingPrompt}
                            className={btnSmall}
                            style={{ borderColor: "rgba(99,102,241,0.4)", color: "#a5b4fc" }}
                          >
                            {generatingPrompt ? tr("생성 중...", "Generating...") : tr("AI 생성", "AI Generate")}
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
                        className="w-full rounded-lg px-3 py-2 text-xs bg-white/5 border resize-y"
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
          <div className="flex items-center gap-2">
            <input
              type="text"
              placeholder={tr("에이전트 이름", "Agent name")}
              value={customName}
              onChange={(e) => setCustomName(e.target.value)}
              className="flex-1 rounded-lg px-3 py-2 text-sm bg-white/5 border"
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            />
            <input
              type="text"
              placeholder={tr("한줄 설명", "Brief description")}
              value={customDesc}
              onChange={(e) => setCustomDesc(e.target.value)}
              className="flex-1 rounded-lg px-3 py-2 text-sm bg-white/5 border"
              style={{ borderColor: borderInput, color: "var(--th-text-primary)" }}
            />
            <button
              onClick={addCustomAgent}
              disabled={!customName.trim()}
              className="px-4 py-2 rounded-lg text-sm font-medium bg-indigo-600 text-white hover:bg-indigo-500 disabled:opacity-40 transition-colors whitespace-nowrap"
            >
              + {tr("추가", "Add")}
            </button>
          </div>

          <div className="flex gap-3 pt-2">
            <button onClick={() => setStep(2)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button onClick={() => setStep(4)} disabled={agents.length === 0} className={btnPrimary}>
              {tr("다음", "Next")} ({agents.length}{tr("개 에이전트", " agents")})
            </button>
          </div>
        </div>
      )}

      {/* ──────────────── Step 4: Channel Setup ──────────────── */}
      {step === 4 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {tr("채널 설정", "Channel Setup")}
            </h2>
            <p className="text-sm mt-1" style={{ color: "var(--th-text-muted)" }}>
              {tr(
                "각 에이전트가 사용할 Discord 채널을 설정합니다. 추천 이름이 미리 채워져 있으며, 기존 채널을 선택하거나 새 이름을 입력할 수 있습니다.",
                "Set up Discord channels for each agent. Recommended names are pre-filled. Select existing channels or enter new names.",
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
                  "봇이 서버에 초대되지 않았거나, 봇 토큰이 입력되지 않았습니다. 이전 단계에서 봇을 설정하거나, 아래에서 채널 이름을 직접 입력하세요.",
                  "Bot not invited to any server, or no bot token set. Set up bots in previous step, or enter channel names manually below.",
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
                <div className="flex gap-2">
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
                      className="flex-1 rounded-lg px-3 py-2 text-sm bg-white/5 border"
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
                      value={ca.channelName}
                      onChange={(e) => {
                        setChannelAssignments((prev) => {
                          const copy = [...prev];
                          copy[i] = { ...ca, channelName: e.target.value };
                          return copy;
                        });
                      }}
                      className="flex-1 rounded-lg px-3 py-2 text-sm bg-white/5 border"
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
                "\"새 채널\"을 선택하면 해당 이름으로 Discord에서 채널을 직접 생성해야 합니다.",
                "Selecting \"New\" means you'll need to create that channel in Discord manually.",
              )}
            </p>
          )}

          <div className="flex gap-3 pt-2">
            <button onClick={() => setStep(3)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button onClick={() => setStep(5)} className={btnPrimary}>
              {tr("다음", "Next")}
            </button>
          </div>
        </div>
      )}

      {/* ──────────────── Step 5: Owner + Confirm ──────────────── */}
      {step === 5 && (
        <div className={stepBox} style={{ borderColor: borderLight }}>
          <div>
            <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
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

            <div className="rounded-lg p-3 text-xs space-y-2" style={{ backgroundColor: "rgba(99,102,241,0.06)" }}>
              <div className="font-medium" style={{ color: "#a5b4fc" }}>
                {tr("Discord 사용자 ID 찾는 방법", "How to find your Discord User ID")}
              </div>
              <ol className="list-decimal list-inside space-y-1" style={{ color: "var(--th-text-secondary)" }}>
                <li>{tr("Discord 앱 하단 ⚙️ 설정 → 고급 → 개발자 모드 활성화", "Discord Settings → Advanced → Enable Developer Mode")}</li>
                <li>{tr("왼쪽 사용자 목록에서 내 이름을 우클릭", "Right-click your name in the member list")}</li>
                <li>{tr("\"사용자 ID 복사\" 클릭 → 위 입력란에 붙여넣기", "Click \"Copy User ID\" → Paste above")}</li>
              </ol>
              <div className="mt-1" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "18~19자리 숫자입니다 (예: 123456789012345678)",
                  "It's an 18-19 digit number (e.g., 123456789012345678)",
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

          <div className="flex gap-3 pt-2">
            <button onClick={() => setStep(4)} className={btnSecondary} style={{ borderColor: "rgba(148,163,184,0.3)" }}>
              {tr("이전", "Back")}
            </button>
            <button onClick={() => void handleComplete()} disabled={completing} className={btnPrimary}>
              {completing ? tr("설정 중...", "Setting up...") : tr("설정 완료", "Complete Setup")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
