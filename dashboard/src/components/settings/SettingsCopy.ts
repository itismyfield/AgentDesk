// User-facing settings copy in one place: labels, hints and status words shown on the settings page.
import type { SettingsPanel } from "./SettingsModel";

type Tr = (ko: string, en: string) => string;

export interface SettingCopy {
  labelKo: string;
  labelEn: string;
  hintKo?: string;
  hintEn?: string;
}

// Server labels for /api/settings/config are internal; these replace them on screen.
const CONFIG_KEY_COPY: Record<string, SettingCopy> = {
  kanban_manager_channel_id: {
    labelKo: "칸반 알림 채널 ID",
    labelEn: "Kanban alert channel ID",
    hintKo: "카드 상태 변경과 자동화 명령을 주고받는 Discord 채널이에요.",
    hintEn: "Discord channel for card status changes and automation commands.",
  },
  deadlock_manager_channel_id: {
    labelKo: "막힘 감지 알림 채널 ID",
    labelEn: "Stuck-work alert channel ID",
    hintKo: "작업이 멈추거나 서로 기다리며 막혔을 때 알려 줄 Discord 채널이에요.",
    hintEn: "Discord channel that hears about stalled or deadlocked work.",
  },
  review_enabled: {
    labelKo: "리뷰 단계 사용",
    labelEn: "Use the review step",
    hintKo: "작업이 끝나면 다른 에이전트가 결과를 검토하게 해요.",
    hintEn: "Another agent reviews finished work before it is done.",
  },
  max_review_rounds: {
    labelKo: "리뷰 최대 반복 횟수",
    labelEn: "Max review rounds",
    hintKo: "한 작업을 고쳐서 다시 검토받을 수 있는 최대 횟수예요.",
    hintEn: "How many times one task can go back for another review.",
  },
  requested_timeout_min: {
    labelKo: "접수 후 대기 경고 (분)",
    labelEn: "Waiting-to-start alert (min)",
    hintKo: "접수된 카드가 이 시간 넘게 시작되지 않으면 알려요.",
    hintEn: "Alerts when an accepted card has not started for this long.",
  },
  in_progress_stale_min: {
    labelKo: "진행 멈춤 경고 (분)",
    labelEn: "Stalled-work alert (min)",
    hintKo: "진행 중인 카드가 이 시간 동안 변화가 없으면 알려요.",
    hintEn: "Alerts when an in-progress card shows no change for this long.",
  },
  max_chain_depth: {
    labelKo: "연쇄 작업 최대 단계",
    labelEn: "Max follow-up depth",
    hintKo: "한 작업이 다른 작업을 연달아 만들 수 있는 최대 단계예요.",
    hintEn: "How many levels of follow-up work one task may create.",
  },
  context_compact_percent: {
    labelKo: "대화 요약 시작 시점 (%)",
    labelEn: "Start summarizing at (%)",
    hintKo: "대화가 이만큼 차면 앞부분을 요약해 공간을 비워요. Claude는 따로 정하지 않으면 이 값을 따라요.",
    hintEn: "When a conversation fills up this much, earlier parts are summarized. Claude follows this unless set separately.",
  },
  context_compact_percent_codex: {
    labelKo: "Codex 대화 요약 시작 시점 (%)",
    labelEn: "Codex: start summarizing at (%)",
    hintKo: "Codex 대화에만 적용되는 기준이에요.",
    hintEn: "Applies to Codex conversations only.",
  },
  context_compact_percent_claude: {
    labelKo: "Claude 대화 요약 시작 시점 (%)",
    labelEn: "Claude: start summarizing at (%)",
    hintKo: "Claude 대화에만 적용돼요. 비워 두면 위 공통 기준을 따라요.",
    hintEn: "Applies to Claude conversations only. Leave empty to follow the shared value.",
  },
  context_compact_lower_bound_tokens: {
    labelKo: "요약을 시작할 최소 길이 (토큰)",
    labelEn: "Minimum length before summarizing (tokens)",
    hintKo: "대화가 이 길이보다 짧으면 비율과 상관없이 요약하지 않아요.",
    hintEn: "Conversations shorter than this are never summarized.",
  },
  server_port: {
    labelKo: "서버 포트",
    labelEn: "Server port",
    hintKo: "대시보드와 에이전트가 접속하는 포트예요. 서버 설정 파일에서 정해요.",
    hintEn: "Port the dashboard and agents connect to. Set in the server config file.",
  },
};

export function configKeyCopy(key: string): SettingCopy | undefined {
  return CONFIG_KEY_COPY[key];
}

export interface RuntimeField extends SettingCopy {
  key: string;
  panel: SettingsPanel;
  section: string;
  inputKind: "range" | "toggle";
  unit?: string;
  min?: number;
  max?: number;
  step?: number;
}

// /api/settings/runtime-config keys, placed where people look for them.
export const RUNTIME_FIELDS: RuntimeField[] = [
  {
    key: "maxRetries",
    panel: "workflow",
    section: "retries",
    labelKo: "작업 재시도 횟수",
    labelEn: "Task retries",
    hintKo: "실패한 작업을 자동으로 다시 시도하는 최대 횟수예요.",
    hintEn: "How many times a failed task is retried automatically.",
    inputKind: "range",
    min: 1,
    max: 10,
    step: 1,
  },
  {
    key: "maxEntryRetries",
    panel: "workflow",
    section: "retries",
    labelKo: "자동 대기열 재시도 횟수",
    labelEn: "Auto-queue retries",
    hintKo: "자동 대기열에서 실패한 작업을 몇 번까지 다시 시도할지 정해요. 넘으면 실패로 표시해요.",
    hintEn: "How many times the auto-queue retries a failed item before marking it failed.",
    inputKind: "range",
    min: 1,
    max: 10,
    step: 1,
  },
  {
    key: "dispatchRateLimitGateEnabled",
    panel: "workflow",
    section: "autoProgress",
    labelKo: "사용량이 많으면 자동 진행 멈추기",
    labelEn: "Pause auto-progress at high usage",
    hintKo: "AI 사용량이 아래 기준을 넘으면 자동 대기열이 새 작업을 보내지 않고 기다려요.",
    hintEn: "While AI usage is above the limit below, the auto-queue waits before sending new work.",
    inputKind: "toggle",
  },
  {
    key: "dispatchRateLimitGateDangerPct",
    panel: "workflow",
    section: "autoProgress",
    labelKo: "자동 진행을 멈출 사용량 (%)",
    labelEn: "Pause auto-progress at (%)",
    hintKo: "AI 사용량이 이 비율 이상이면 새 작업을 잠시 보류해요.",
    hintEn: "New work is held while AI usage is at or above this percentage.",
    inputKind: "range",
    unit: "%",
    min: 80,
    max: 100,
    step: 1,
  },
  {
    key: "rateLimitWarningPct",
    panel: "ai",
    section: "usage",
    labelKo: "사용량 주의 표시 (%)",
    labelEn: "Usage warning at (%)",
    hintKo: "AI 사용량이 이 비율을 넘으면 주의로 표시해요.",
    hintEn: "AI usage above this percentage shows as a warning.",
    inputKind: "range",
    unit: "%",
    min: 50,
    max: 99,
    step: 1,
  },
  {
    key: "rateLimitDangerPct",
    panel: "ai",
    section: "usage",
    labelKo: "사용량 위험 표시 (%)",
    labelEn: "Usage danger at (%)",
    hintKo: "AI 사용량이 이 비율을 넘으면 위험으로 표시해요.",
    hintEn: "AI usage above this percentage shows as danger.",
    inputKind: "range",
    unit: "%",
    min: 60,
    max: 100,
    step: 1,
  },
  {
    key: "rateLimitStaleSec",
    panel: "ai",
    section: "advanced",
    labelKo: "사용량 정보 유효 시간 (초)",
    labelEn: "Usage data freshness (s)",
    hintKo: "이 시간이 지난 사용량 정보는 오래된 것으로 보고 다시 받아요.",
    hintEn: "Usage data older than this is treated as stale and fetched again.",
    inputKind: "range",
    unit: "s",
    min: 30,
    max: 1800,
    step: 30,
  },
];

export interface SectionCopy {
  titleKo: string;
  titleEn: string;
  descriptionKo?: string;
  descriptionEn?: string;
}

export const SECTION_COPY: Record<string, SectionCopy> = {
  "general.identity": {
    titleKo: "기본 정보",
    titleEn: "Basics",
    descriptionKo: "대시보드에 보이는 이름과 화면 환경이에요.",
    descriptionEn: "Names and display preferences shown across the dashboard.",
  },
  "workflow.timeouts": {
    titleKo: "경고 시간",
    titleEn: "Alert timing",
    descriptionKo: "카드가 오래 멈춰 있으면 알려 주는 기준이에요.",
    descriptionEn: "When to alert about cards that sit still for too long.",
  },
  "workflow.retries": {
    titleKo: "다시 시도",
    titleEn: "Retries",
    descriptionKo: "실패한 작업을 자동으로 몇 번 다시 시도할지 정해요.",
    descriptionEn: "How often failed work is retried automatically.",
  },
  "workflow.autoProgress": {
    titleKo: "자동 진행",
    titleEn: "Auto-progress",
    descriptionKo: "AI 사용량이 많을 때 자동 대기열을 잠시 멈출지 정해요.",
    descriptionEn: "Whether the auto-queue pauses while AI usage is high.",
  },
  "workflow.review": {
    titleKo: "리뷰",
    titleEn: "Review",
    descriptionKo: "작업 결과를 검토하는 단계를 정해요.",
    descriptionEn: "How finished work gets reviewed.",
  },
  "workflow.other": {
    titleKo: "기타",
    titleEn: "Other",
  },
  "workflow.editor": {
    titleKo: "세부 흐름 편집기",
    titleEn: "Detailed workflow editor",
    descriptionKo: "카드가 거치는 상태와 전환을 직접 바꿔요. 대부분은 바꿀 필요가 없어요.",
    descriptionEn: "Change the states and transitions cards go through. Most setups never need this.",
  },
  "ai.accounts": {
    titleKo: "계정",
    titleEn: "Accounts",
    descriptionKo: "기본 계정과 추가 계정의 사용량을 봐요.",
    descriptionEn: "Usage of the default and additional accounts.",
  },
  "ai.usage": {
    titleKo: "사용량 경고 기준",
    titleEn: "Usage warnings",
    descriptionKo: "AI 사용량이 얼마나 차면 경고할지 정해요.",
    descriptionEn: "How full AI usage gets before a warning shows.",
  },
  "ai.context": {
    titleKo: "대화 요약 기준",
    titleEn: "Conversation summaries",
    descriptionKo: "대화가 길어지면 앞부분을 요약해 이어 가요. 언제 요약할지 정해요.",
    descriptionEn: "Long conversations are summarized to keep going. Choose when that happens.",
  },
  "ai.advanced": {
    titleKo: "고급",
    titleEn: "Advanced",
    descriptionKo: "보통은 바꿀 필요가 없어요.",
    descriptionEn: "Usually no need to change these.",
  },
  "connections.status": {
    titleKo: "연결 상태",
    titleEn: "Connection status",
    descriptionKo: "처음 설정할 때 연결한 Discord 봇, 서버, 소유자, AI 서비스 상태예요.",
    descriptionEn: "The Discord bot, server, owner and AI services connected during setup.",
  },
  "connections.channels": {
    titleKo: "Discord 알림 채널",
    titleEn: "Discord alert channels",
    descriptionKo: "Discord에서 채널을 오른쪽 클릭한 뒤 'ID 복사하기'로 얻은 숫자를 넣어요. 개발자 모드가 켜져 있어야 보여요.",
    descriptionEn: "Paste the number from right-clicking a channel and choosing “Copy Channel ID” (needs Developer Mode).",
  },
  "connections.connectors": {
    titleKo: "커넥터",
    titleEn: "Connectors",
    descriptionKo: "Obsidian, Kakao처럼 골라서 쓰는 외부 연결이에요. 쓰지 않으면 꺼 둬도 괜찮아요.",
    descriptionEn: "Optional outside connections such as Obsidian and Kakao. Fine to leave off if unused.",
  },
  "machine.server": {
    titleKo: "서버",
    titleEn: "Server",
  },
};

export interface PanelCopy {
  titleKo: string;
  titleEn: string;
  detailKo: string;
  detailEn: string;
  keywords: string;
}

export const PANEL_COPY: Record<SettingsPanel, PanelCopy> = {
  general: {
    titleKo: "일반",
    titleEn: "General",
    detailKo: "이름, 언어, 테마를 정해요.",
    detailEn: "Names, language and theme.",
    keywords: "회사 내 이름 언어 테마 company name language theme",
  },
  workflow: {
    titleKo: "작업 흐름",
    titleEn: "Workflow",
    detailKo: "경고 시간, 재시도, 자동 진행처럼 작업이 흘러가는 규칙을 정해요.",
    detailEn: "Alert timing, retries and auto-progress rules for work.",
    keywords: "파이프라인 편집기 리뷰 재시도 자동 대기열 경고 pipeline editor review retry queue",
  },
  ai: {
    titleKo: "AI 계정",
    titleEn: "AI accounts",
    detailKo: "AI 서비스 계정과 사용량 경고, 대화 요약 기준을 관리해요.",
    detailEn: "AI service accounts, usage warnings and conversation summaries.",
    keywords: "프로바이더 계정 사용량 요약 provider account usage claude codex gemini qwen",
  },
  connections: {
    titleKo: "연결",
    titleEn: "Connections",
    detailKo: "Discord 봇·서버·알림 채널과 외부 커넥터를 확인해요.",
    detailEn: "Discord bot, server, alert channels and connectors.",
    keywords: "디스코드 봇 서버 소유자 채널 커넥터 초기 설정 온보딩 discord bot server owner channel connector obsidian kakao onboarding",
  },
  machine: {
    titleKo: "머신",
    titleEn: "Machines",
    detailKo: "Hub·Runner 장치 상태와 서버 포트를 확인해요.",
    detailEn: "Hub and Runner device status and the server port.",
    keywords: "장치 서버 포트 메모리 디스크 hub runner cpu memory disk port",
  },
};

export function restartBehaviorNote(behavior: string | null | undefined, isKo: boolean): string | null {
  const notes: Record<string, [string, string]> = {
    "reseed-from-yaml": ["다시 시작하면 설정 파일(agentdesk.yaml)의 값으로 돌아가요.", "Restart restores the value from agentdesk.yaml."],
    "persist-live-override": ["다시 시작해도 여기서 바꾼 값이 유지돼요.", "The value set here stays after a restart."],
    "reset-to-baseline": ["다시 시작하면 기본값으로 돌아가요.", "Restart resets this to the default."],
    "clear-on-restart": ["다시 시작하면 여기서 바꾼 값이 지워져요.", "Restart clears the value set here."],
    "config-only": ["서버 설정 파일에서 정하는 값이라 여기서는 바꿀 수 없어요.", "Set in the server config file, so it cannot be changed here."],
  };
  const note = behavior ? notes[behavior] : undefined;
  return note ? note[isKo ? 0 : 1] : null;
}

export const RUNTIME_APPLY_NOTE = {
  ko: "저장하면 다시 시작하지 않아도 곧 적용돼요.",
  en: "Applies shortly after saving; no restart needed.",
};

const PROVIDER_NAMES: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  gemini: "Gemini",
  qwen: "Qwen",
  opencode: "OpenCode",
  antigravity: "Antigravity",
};

export function providerDisplayName(id: string): string {
  return PROVIDER_NAMES[id.toLowerCase()] ?? id;
}

/** "5h" → "5시간 한도", "7d Fable" → "7일 한도 · Fable 모델". */
export function usageBucketLabel(name: string, tr: Tr): string {
  const match = /^(\d+)\s*([hd])\b\s*(.*)$/i.exec(name.trim());
  if (!match) return name;
  const [, amount, unit, model] = match;
  const window = unit.toLowerCase() === "h"
    ? tr(`${amount}시간 한도`, `${amount}-hour limit`)
    : tr(`${amount}일 한도`, `${amount}-day limit`);
  const modelName = model.trim();
  return modelName ? tr(`${window} · ${modelName} 모델`, `${window} · ${modelName} model`) : window;
}

const CONNECTOR_NAMES: Record<string, SettingCopy> = {
  obsidian_agent_prompts: {
    labelKo: "Obsidian 에이전트 프롬프트",
    labelEn: "Obsidian agent prompts",
    hintKo: "에이전트 프롬프트를 Obsidian 보관함에서 읽어요.",
    hintEn: "Reads agent prompts from your Obsidian vault.",
  },
  obsidian_skill_root: {
    labelKo: "Obsidian 스킬 폴더",
    labelEn: "Obsidian skill folder",
    hintKo: "스킬(SKILL.md)을 Obsidian 보관함에서 읽어요.",
    hintEn: "Reads skills (SKILL.md) from your Obsidian vault.",
  },
  kakao_scheduled_delivery: {
    labelKo: "Kakao 메시지 보내기",
    labelEn: "Kakao messages",
    hintKo: "정해진 시간에 Kakao 메시지로 알림을 보내요.",
    hintEn: "Sends scheduled notices as Kakao messages.",
  },
};

export function connectorCopy(id: string, fallbackName: string): SettingCopy {
  return CONNECTOR_NAMES[id] ?? { labelKo: fallbackName, labelEn: fallbackName };
}

const CONNECTOR_REASONS: Record<string, [string, string]> = {
  disabled: ["꺼져 있어요.", "Turned off."],
  missing_credentials: ["Kakao 인증 정보가 없어요.", "Kakao credentials are missing."],
  home_unavailable: ["홈 폴더를 찾지 못했어요.", "The home folder could not be found."],
  missing_path: ["지정한 폴더가 없어요.", "The configured folder does not exist."],
  missing_config: ["아직 경로를 정하지 않았어요.", "No path is configured yet."],
  missing_skill_files: ["폴더에 스킬 파일(SKILL.md)이 없어요.", "The folder has no SKILL.md files."],
  invalid_configuration: ["설정 값이 올바르지 않아요.", "The configuration is invalid."],
  unknown_account: ["Kakao 계정을 찾지 못했어요.", "The Kakao account was not found."],
  reauthorization_required: ["Kakao에 다시 로그인해야 해요.", "Sign in to Kakao again."],
  consent_required: ["Kakao 메시지 권한 동의가 필요해요.", "Kakao messaging consent is required."],
};

export function connectorReason(reason: string, tr: Tr): string {
  const copy = CONNECTOR_REASONS[reason];
  return copy ? tr(copy[0], copy[1]) : reason;
}
