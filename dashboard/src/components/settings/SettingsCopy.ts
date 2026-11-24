// User-facing settings copy in one place: the catalog's categories and known-key notes, plus provider and connector words.

type Tr = (ko: string, en: string) => string;

export const SETTINGS_CATEGORIES = [
  { id: "workflow", ko: "작업 흐름", en: "Workflow" },
  { id: "review", ko: "리뷰", en: "Review" },
  { id: "context", ko: "컨텍스트", en: "Context" },
  { id: "usage", ko: "사용량", en: "Usage" },
  { id: "alerts", ko: "알림 채널", en: "Alert channels" },
  { id: "system", ko: "시스템", en: "System" },
  { id: "connections", ko: "연결", en: "Connections" },
  { id: "other", ko: "기타", en: "Other" },
] as const;
export type SettingsCategory = (typeof SETTINGS_CATEGORIES)[number]["id"];

// The deadlock manager is retired (11-19); its server key stays, the catalog never lists it.
export const HIDDEN_SETTING_KEYS = new Set(["deadlock_manager_channel_id"]);

export interface RelatedSetting {
  key: string;
  noteKo?: string;
  noteEn?: string;
}

export interface SettingKeyMeta {
  category: SettingsCategory;
  /** Runtime keys and status rows only; config keys show the server's label_ko. */
  labelKo?: string;
  labelEn?: string;
  descKo?: string;
  descEn?: string;
  input?: "toggle" | "number" | "text";
  min?: number;
  max?: number;
  related?: RelatedSetting[];
  /** Saving "" makes the server read this other key instead. */
  inherits?: string;
}

// Category and description per known key until the server sends its own meta (#8249). Each description restates
// behavior read in policies/ or src/; a key whose reader was not found (maxRetries) keeps an empty description.
const SETTING_KEY_META: Record<string, SettingKeyMeta> = {
  requested_timeout_min: {
    category: "workflow", input: "number", min: 1,
    descKo: "요청 상태 카드가 이 시간 넘게 시작되지 않으면 작업 지시를 실패로 돌리고 다시 시도해요. 재시도를 다 쓰면 사람 판단 대기로 넘겨요.",
    descEn: "A requested card that has not started in this many minutes has its dispatch failed and retried. Once retries run out, it waits for a human decision.",
  },
  in_progress_stale_min: {
    category: "workflow", input: "number", min: 1,
    descKo: "진행 중 카드에 이 시간 동안 활동이 없으면 사람 판단 대기로 넘겨요. 막힌 사유가 있는 카드는 빼요.",
    descEn: "An in-progress card with no activity for this many minutes waits for a human decision. Blocked cards are skipped.",
  },
  maxRetries: { category: "workflow", labelKo: "작업 재시도 횟수", labelEn: "Task retries", input: "number", min: 1, max: 10 },
  maxEntryRetries: {
    category: "workflow", labelKo: "자동 대기열 재시도 횟수", labelEn: "Auto-queue retries", input: "number", min: 1, max: 10,
    descKo: "자동 대기열 항목이 전달에 이 횟수만큼 실패하면 더 시도하지 않고 실패로 둬요.",
    descEn: "An auto-queue item that fails delivery this many times is marked failed.",
  },
  staleDispatchedGraceMin: {
    category: "workflow", labelKo: "멈춘 전달 판정 시간 (분)", labelEn: "Stuck delivery grace (min)", input: "number", min: 1,
    descKo: "전달된 자동 대기열 항목이 이 시간 넘게 그대로면 멈춘 것으로 보고 전달 실패 한 번으로 세요.",
    descEn: "A delivered auto-queue item unchanged for this many minutes counts as one failed delivery.",
    related: [{ key: "maxEntryRetries", noteKo: "실패가 이 횟수에 닿으면 항목을 실패로 둬요", noteEn: "Items are marked failed at this many failures" }],
  },
  staleDispatchedTerminalStatuses: {
    category: "workflow", labelKo: "멈춤으로 볼 전달 상태", labelEn: "Stuck delivery statuses", input: "text",
    descKo: "전달 기록이 이 상태(쉼표로 구분)로 끝났는데 항목이 그대로면 멈춘 것으로 봐요.",
    descEn: "Items whose delivery ended in one of these comma-separated statuses count as stuck.",
    related: [{ key: "staleDispatchedGraceMin" }],
  },
  staleDispatchedRecoverNullDispatch: {
    category: "workflow", labelKo: "전달 기록 없는 항목도 복구", labelEn: "Recover items without a delivery", input: "toggle",
    descKo: "켜 두면 전달 기록이 없는 항목도 멈춘 것으로 봐요.",
    descEn: "When on, items with no delivery record also count as stuck.",
  },
  staleDispatchedRecoverMissingDispatch: {
    category: "workflow", labelKo: "사라진 전달 기록도 복구", labelEn: "Recover missing deliveries", input: "toggle",
    descKo: "켜 두면 가리키는 전달 기록이 지워진 항목도 멈춘 것으로 봐요.",
    descEn: "When on, items pointing at a deleted delivery record also count as stuck.",
  },
  review_enabled: {
    category: "review", input: "toggle",
    descKo: "꺼 두면 리뷰 단계에 들어온 카드를 리뷰 없이 바로 끝 상태로 넘겨요.",
    descEn: "When off, cards entering review go straight to their final state.",
  },
  max_review_rounds: {
    category: "review", input: "number", min: 1,
    descKo: "한 카드를 고쳐서 다시 리뷰받을 수 있는 최대 횟수예요. 넘으면 사람 판단 대기로 넘겨요.",
    descEn: "How many times one card can go back for review after fixes. Past this, it waits for a human decision.",
    related: [{ key: "review_enabled", noteKo: "꺼져 있으면 이 값은 쓰이지 않아요", noteEn: "Unused while review is off" }],
  },
  context_compact_percent: {
    category: "context", input: "number", min: 0, max: 100,
    descKo: "Claude·Codex 밖의 프로바이더가 쓰는 압축 기준이에요. Claude는 자기 값이 없으면 이 값을 따라요.",
    descEn: "Compaction threshold for providers other than Claude and Codex. Claude follows it when its own value is empty.",
    related: [{ key: "context_compact_percent_claude" }],
  },
  context_compact_percent_codex: {
    category: "context", input: "number", min: 0, max: 100,
    descKo: "Codex 대화에 쓰는 압축 기준이에요. 공통 값을 따르지 않아요.",
    descEn: "Compaction threshold for Codex conversations. It does not follow the shared value.",
  },
  context_compact_percent_claude: {
    category: "context", input: "number", min: 0, max: 100,
    descKo: "Claude 대화에 쓰는 압축 기준이에요. 비워 두면 설정 파일의 Claude 값을 쓰고 그 값도 없으면 공통 값을 따라요. 0이면 자동 압축을 꺼요.",
    descEn: "Compaction threshold for Claude conversations. Empty uses the config file's Claude value, else the shared value; 0 turns auto compaction off.",
    related: [{ key: "context_compact_percent" }, { key: "context_compact_lower_bound_tokens" }],
    inherits: "context_compact_percent",
  },
  context_compact_lower_bound_tokens: {
    category: "context", input: "number", min: 0,
    descKo: "Claude는 비율로 낸 압축 기준이 이 토큰 수보다 작으면 이 값을 써요. 그래도 창 크기에서 6만 4천 토큰을 뺀 안전 상한은 넘지 않아요. 20만 토큰 창이면 이 값이 30만이어도 13만 6천 토큰에서 압축해요.",
    descEn: "Claude uses this many tokens when its percentage lands lower, but never above the window minus a 64K safety reserve. A 200K window compacts at 136K even with a 300K floor.",
  },
  rateLimitWarningPct: {
    category: "usage", labelKo: "사용량 주의 표시 (%)", labelEn: "Usage warning at (%)", input: "number", min: 50, max: 99,
    descKo: "AI 계정 사용량 막대를 이 비율부터 주의 색으로 칠해요.",
    descEn: "AI account usage bars turn to the warning color from this percentage.",
  },
  rateLimitDangerPct: {
    category: "usage", labelKo: "사용량 위험 표시 (%)", labelEn: "Usage danger at (%)", input: "number", min: 60, max: 100,
    descKo: "AI 계정 사용량 막대를 이 비율부터 위험 색으로 칠해요.",
    descEn: "AI account usage bars turn to the danger color from this percentage.",
    related: [{ key: "dispatchRateLimitGateDangerPct", noteKo: "자동 진행을 멈추는 기준은 따로 정해요", noteEn: "The auto-progress pause has its own threshold" }],
  },
  rateLimitStaleSec: {
    category: "usage", labelKo: "사용량 정보 유효 시간 (초)", labelEn: "Usage data freshness (s)", input: "number", min: 30, max: 1800,
    descKo: "이 시간보다 오래된 사용량 정보는 자동 진행을 멈출지 정할 때 쓰지 않아요.",
    descEn: "Usage data older than this is ignored when deciding whether to pause auto-progress.",
  },
  dispatchRateLimitGateEnabled: {
    category: "usage", labelKo: "사용량이 많으면 자동 진행 멈추기", labelEn: "Pause auto-progress at high usage", input: "toggle",
    descKo: "켜 두면 사용률이 기준에 닿은 프로바이더에는 자동 대기열이 새 작업을 보내지 않고 기다려요.",
    descEn: "When on, the auto-queue holds new work for providers at or above the usage threshold.",
    related: [{ key: "dispatchRateLimitGateDangerPct" }, { key: "rateLimitStaleSec" }],
  },
  dispatchRateLimitGateDangerPct: {
    category: "usage", labelKo: "자동 진행을 멈출 사용량 (%)", labelEn: "Pause auto-progress at (%)", input: "number", min: 1, max: 100,
    descKo: "자동 대기열이 새 작업을 미루는 사용률 기준이에요. 위험 표시 기준과는 따로예요.",
    descEn: "Usage at which the auto-queue holds new work. Separate from the danger color threshold.",
    related: [{ key: "dispatchRateLimitGateEnabled", noteKo: "꺼져 있으면 이 값은 쓰이지 않아요", noteEn: "Unused while the pause is off" }],
  },
  agent_quality_monitoring_channel_id: {
    category: "alerts", input: "text",
    descKo: "카드는 끝났는데 GitHub 이슈가 열려 있으면 이 Discord 채널로 알려요. ADK_GITHUB_SYNC_ALERT_CHANNEL 환경 변수가 있으면 그쪽을 먼저 써요.",
    descEn: "Discord channel told when a card is done but its GitHub issue is still open. ADK_GITHUB_SYNC_ALERT_CHANNEL wins when set.",
  },
  language: {
    category: "system", labelKo: "언어", labelEn: "Language",
    descKo: "대시보드 화면에 쓰는 언어예요. 저장하면 바로 바뀌어요.",
    descEn: "Language of the dashboard. Changes as soon as you save.",
  },
  theme: {
    category: "system", labelKo: "기본 테마", labelEn: "Default theme",
    descKo: "테마를 고른 적 없는 브라우저가 처음 쓰는 테마예요. 저장하면 이 브라우저에도 바로 적용돼요.",
    descEn: "Theme a browser uses until it picks its own. Saving also applies it here right away.",
  },
  server_port: {
    category: "system",
    descKo: "대시보드와 에이전트가 접속하는 포트예요. 서버 설정 파일에서 정해요.",
    descEn: "Port the dashboard and agents connect to. Set in the server config file.",
  },
  kanban_relay_alert_threshold: {
    category: "system", input: "number", min: 0,
    descKo: "릴레이 누락 신호가 한 시간에 이 횟수 이상이면 서버에 경고 기록을 남겨요. 비우거나 0이면 신호마다 정한 기본값을 써요.",
    descEn: "The server logs a warning when a relay-loss signal reaches this count in an hour. Empty or 0 uses each signal's default.",
  },
  prompt_manifest_retention: {
    category: "system", labelKo: "프롬프트 보관 기간", labelEn: "Prompt retention",
    descKo: "에이전트에 보낸 프롬프트의 본문을 이 기간만 남겨요. 지나면 본문만 지우고 기록 정보는 둬요.",
    descEn: "Full prompt text sent to agents is kept this long. After that only the text is cleared; the record stays.",
  },
  "health.status": {
    category: "connections", labelKo: "서버 상태", labelEn: "Server health",
    descKo: "서버가 스스로 판정한 상태예요. 주의 이상인 신호를 함께 보여 줘요.",
    descEn: "The server's own health verdict, with every signal at warning level or above.",
  },
  "health.providers": {
    category: "connections",
    descKo: "Discord에 붙은 프로바이더 봇의 연결 상태예요.",
    descEn: "Connection state of the provider bots on Discord.",
  },
  "onboarding.guild_id": { category: "connections", descKo: "처음 설정할 때 연결한 Discord 서버예요.", descEn: "The Discord server connected during setup." },
  "onboarding.owner_id": { category: "connections", descKo: "처음 설정할 때 정한 소유자 Discord 계정이에요.", descEn: "The owner's Discord account chosen during setup." },
  "onboarding.bot_providers": { category: "connections", descKo: "처음 설정할 때 고른 봇 프로바이더예요.", descEn: "The bot providers chosen during setup." },
  "health.dispatch_outbox": {
    category: "connections", labelKo: "전달 대기", labelEn: "Delivery outbox",
    descKo: "작업 지시를 Discord로 알리기 전에 쌓아 두는 대기열이에요.",
    descEn: "Queue of dispatch notices waiting to go out to Discord.",
  },
  obsidian_agent_prompts: {
    category: "connections", labelKo: "Obsidian 에이전트 프롬프트", labelEn: "Obsidian agent prompts",
    descKo: "에이전트 프롬프트를 Obsidian 보관함에서 읽어요.", descEn: "Reads agent prompts from your Obsidian vault.",
  },
  obsidian_skill_root: {
    category: "connections", labelKo: "Obsidian 스킬 폴더", labelEn: "Obsidian skill folder",
    descKo: "스킬(SKILL.md)을 Obsidian 보관함에서 읽어요.", descEn: "Reads skills (SKILL.md) from your Obsidian vault.",
  },
  kakao_scheduled_delivery: {
    category: "connections", labelKo: "Kakao 메시지 보내기", labelEn: "Kakao messages",
    descKo: "정해진 시간에 Kakao 메시지로 알림을 보내요.", descEn: "Sends scheduled notices as Kakao messages.",
  },
};

export function settingKeyMeta(key: string): SettingKeyMeta | undefined {
  return SETTING_KEY_META[key];
}

const KEY_ORDER = Object.keys(SETTING_KEY_META);

/** Position in the table above, so rows keep a reading order inside a category; unknown keys go last. */
export function settingKeyOrder(key: string): number {
  const index = KEY_ORDER.indexOf(key);
  return index < 0 ? KEY_ORDER.length : index;
}

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

/** "5h" → "5시간 한도", "7d Fable" / "7일 Fable" → "7일 한도 · 상위 모델 (Fable)". */
export function usageBucketLabel(name: string, tr: Tr): string {
  const match = /^(\d+)\s*(?:([hd])\b|(시간|일))\s*(.*)$/i.exec(name.trim());
  if (!match) return name;
  const [, amount, unitEn, unitKo, model] = match;
  const isHour = unitEn ? unitEn.toLowerCase() === "h" : unitKo === "시간";
  const window = isHour
    ? tr(`${amount}시간 한도`, `${amount}-hour limit`)
    : tr(`${amount}일 한도`, `${amount}-day limit`);
  const modelName = model.trim();
  if (!modelName) return window;
  if (/^fable\b/i.test(modelName)) {
    return tr(`${window} · 상위 모델 (Fable)`, `${window} · frontier model (Fable)`);
  }
  return tr(`${window} · ${modelName} 모델`, `${window} · ${modelName} model`);
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

type ConnectorSetupInput = { id: string; state: string; reason: string | null; env_var: string; source: string | null; setup_actions: string[] };
const OBSIDIAN_STUBS = "scripts/operator-init-portable.py --with-obsidian-stubs";

/** Korean setup steps for the reason codes this screen knows. `translated` is false when the server's English steps are shown as they are. */
export function connectorSetupSteps(connector: ConnectorSetupInput, isKo: boolean): { steps: string[]; translated: boolean } {
  const steps = isKo ? koreanSetupSteps(connector) : null;
  return steps ? { steps, translated: true } : { steps: connector.setup_actions, translated: false };
}

function koreanSetupSteps({ id, state, reason, env_var: env, source }: ConnectorSetupInput): string[] | null {
  if (id === "kakao_scheduled_delivery") {
    if (reason === "disabled") return ["Kakao 인증 정보를 넣은 뒤 AGENTDESK_KAKAO_ENABLED=true로 켜세요."];
    if (reason === "missing_credentials") return [
      "KAKAO_REST_API_KEY와 KAKAO_REFRESH_TOKEN을 넣으세요(권장). 잠깐만 쓸 거라면 KAKAO_ACCESS_TOKEN을 넣어도 돼요.",
      "Kakao 환경 변수를 바꾼 뒤 AgentDesk를 다시 시작하세요.",
    ];
    if (state === "invalid_config") return [
      "AGENTDESK_KAKAO_ACCOUNTS, AGENTDESK_KAKAO_DEFAULT_ACCOUNT, AGENTDESK_KAKAO_LANDING_URL 값을 확인하세요.",
      "고친 뒤 AgentDesk를 다시 시작하세요.",
    ];
    return null;
  }
  if (reason === "home_unavailable") return [`${env}를 이미 있는 폴더로 지정하세요.`];
  if (reason === "missing_path" && source) return [`지정한 폴더를 만드세요: ${source}`, `${env}를 지우거나 이미 있는 폴더로 바꾸세요.`];
  if (reason === "missing_skill_files" && source) return [`${source} 아래에 SKILL.md가 든 스킬 폴더를 하나 이상 넣으세요.`];
  if (reason === "missing_config" && id === "obsidian_agent_prompts") return [`${env}를 이미 있는 프롬프트 폴더로 지정하세요. 시작용 폴더가 필요하면 ${OBSIDIAN_STUBS}를 실행하세요.`];
  if (reason === "missing_config" && id === "obsidian_skill_root") return [`${env}를 <스킬>/SKILL.md가 하나 이상 든 폴더로 지정하세요. 실제 스킬을 동기화하기 전이라면 ${OBSIDIAN_STUBS}를 실행하세요.`];
  return null;
}
