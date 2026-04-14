import { Suspense, lazy, useEffect, useState, type CSSProperties, type ReactNode } from "react";
import type { CompanySettings } from "../types";
import * as api from "../api";
import {
  SettingsCallout,
  SettingsCard,
  SettingsEmptyState,
  SettingsFieldCard,
  SettingsSection,
  SettingsSubsection,
} from "./common/SettingsPrimitives";

const OnboardingWizard = lazy(() => import("./OnboardingWizard"));

interface SettingsViewProps {
  settings: CompanySettings;
  onSave: (patch: Record<string, unknown>) => Promise<void>;
  isKo: boolean;
}

interface ConfigField {
  key: string;
  labelKo: string;
  labelEn: string;
  descriptionKo: string;
  descriptionEn: string;
  unit: string;
  min: number;
  max: number;
  step: number;
}

type ConfigEntry = {
  key: string;
  value: string | null;
  category: string;
  label_ko: string;
  label_en: string;
  default?: string | null;
  baseline?: string | null;
  baseline_source?: string | null;
  override_active?: boolean;
  editable?: boolean;
  restart_behavior?: string | null;
};

type ConfigEditValue = string | boolean;
type AuditNoteStatus = "read-only" | "managed-elsewhere" | "backend-contract" | "typed-only" | "backend-followup";

interface AuditNote {
  id: string;
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  keys: string[];
  status: AuditNoteStatus;
}

const CATEGORIES: Array<{
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  fields: ConfigField[];
}> = [
  {
    titleKo: "폴링 & 타이머",
    titleEn: "Polling & Timers",
    descriptionKo: "백엔드 동기화와 배치 작업이 얼마나 자주 실행되는지 조정합니다.",
    descriptionEn: "Controls how often backend sync and batch jobs run.",
    fields: [
      {
        key: "dispatchPollSec",
        labelKo: "디스패치 폴링 주기",
        labelEn: "Dispatch poll interval",
        descriptionKo: "새 디스패치를 읽어오는 간격입니다.",
        descriptionEn: "How often new dispatches are polled.",
        unit: "s",
        min: 5,
        max: 300,
        step: 5,
      },
      {
        key: "agentSyncSec",
        labelKo: "에이전트 상태 동기화 주기",
        labelEn: "Agent status sync interval",
        descriptionKo: "에이전트 상태를 다시 수집하는 간격입니다.",
        descriptionEn: "How often agent status is refreshed.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "githubIssueSyncSec",
        labelKo: "GitHub 이슈 동기화 주기",
        labelEn: "GitHub issue sync interval",
        descriptionKo: "GitHub 이슈 데이터를 다시 가져오는 간격입니다.",
        descriptionEn: "How often GitHub issue data is refreshed.",
        unit: "s",
        min: 300,
        max: 7200,
        step: 60,
      },
      {
        key: "claudeRateLimitPollSec",
        labelKo: "Claude Rate Limit 폴링",
        labelEn: "Claude rate limit poll",
        descriptionKo: "Claude 사용량/제한 정보를 다시 확인하는 간격입니다.",
        descriptionEn: "Polling interval for Claude rate-limit usage.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "codexRateLimitPollSec",
        labelKo: "Codex Rate Limit 폴링",
        labelEn: "Codex rate limit poll",
        descriptionKo: "Codex 사용량/제한 정보를 다시 확인하는 간격입니다.",
        descriptionEn: "Polling interval for Codex rate-limit usage.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "issueTriagePollSec",
        labelKo: "이슈 트리아지 주기",
        labelEn: "Issue triage interval",
        descriptionKo: "신규 이슈 triage 자동화를 다시 실행하는 간격입니다.",
        descriptionEn: "How often issue triage automation runs.",
        unit: "s",
        min: 60,
        max: 3600,
        step: 60,
      },
    ],
  },
  {
    titleKo: "디스패치 제한",
    titleEn: "Dispatch Limits",
    descriptionKo: "경고 임계값과 자동 재시도 횟수 같은 운영 제한을 조정합니다.",
    descriptionEn: "Adjusts operational limits such as warnings and retries.",
    fields: [
      {
        key: "ceoWarnDepth",
        labelKo: "CEO 경고 깊이",
        labelEn: "CEO warning depth",
        descriptionKo: "체인이 이 깊이를 넘으면 경고를 강화합니다.",
        descriptionEn: "Escalates warnings after this chain depth.",
        unit: "",
        min: 1,
        max: 10,
        step: 1,
      },
      {
        key: "maxRetries",
        labelKo: "최대 재시도 횟수",
        labelEn: "Max retries",
        descriptionKo: "자동 재시도가 허용되는 최대 횟수입니다.",
        descriptionEn: "Maximum number of automatic retries allowed.",
        unit: "",
        min: 1,
        max: 10,
        step: 1,
      },
    ],
  },
  {
    titleKo: "리뷰",
    titleEn: "Review",
    descriptionKo: "리뷰 리마인드와 운영 리듬을 다듬습니다.",
    descriptionEn: "Tunes review reminder cadence.",
    fields: [
      {
        key: "reviewReminderMin",
        labelKo: "리뷰 리마인드 간격",
        labelEn: "Review reminder interval",
        descriptionKo: "리뷰 대기 작업에 다시 알림을 보내는 간격입니다.",
        descriptionEn: "Reminder interval for work waiting in review.",
        unit: "min",
        min: 5,
        max: 120,
        step: 5,
      },
    ],
  },
  {
    titleKo: "알림 임계값",
    titleEn: "Alert Thresholds",
    descriptionKo: "사용량 경고를 얼마나 이르게 띄울지 조절합니다.",
    descriptionEn: "Controls how early usage warnings appear.",
    fields: [
      {
        key: "rateLimitWarningPct",
        labelKo: "Rate Limit 경고 수준",
        labelEn: "Rate limit warning level",
        descriptionKo: "이 비율 이상 사용 시 경고 상태로 표시합니다.",
        descriptionEn: "Shows warning state above this usage percentage.",
        unit: "%",
        min: 50,
        max: 99,
        step: 1,
      },
      {
        key: "rateLimitDangerPct",
        labelKo: "Rate Limit 위험 수준",
        labelEn: "Rate limit danger level",
        descriptionKo: "이 비율 이상 사용 시 위험 상태로 표시합니다.",
        descriptionEn: "Shows danger state above this usage percentage.",
        unit: "%",
        min: 60,
        max: 100,
        step: 1,
      },
    ],
  },
  {
    titleKo: "캐시 TTL",
    titleEn: "Cache TTL",
    descriptionKo: "외부 데이터와 사용량 정보를 얼마나 오래 캐시할지 정합니다.",
    descriptionEn: "Controls how long external data and usage stay cached.",
    fields: [
      {
        key: "githubRepoCacheSec",
        labelKo: "GitHub 레포 캐시",
        labelEn: "GitHub repo cache",
        descriptionKo: "GitHub 레포 메타데이터를 캐시하는 시간입니다.",
        descriptionEn: "Cache TTL for GitHub repository metadata.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
      {
        key: "rateLimitStaleSec",
        labelKo: "Rate Limit stale 판정",
        labelEn: "Rate limit stale threshold",
        descriptionKo: "이 시간 이후 사용량 데이터를 오래된 것으로 봅니다.",
        descriptionEn: "Marks usage data stale after this duration.",
        unit: "s",
        min: 30,
        max: 1800,
        step: 30,
      },
    ],
  },
];


const BOOLEAN_CONFIG_KEYS = new Set([
  "review_enabled",
  "pm_decision_gate_enabled",
  "narrate_progress",
]);

const NUMERIC_CONFIG_KEYS = new Set([
  "max_review_rounds",
  "requested_timeout_min",
  "in_progress_stale_min",
  "max_chain_depth",
  "context_compact_percent",
  "context_compact_percent_codex",
  "context_compact_percent_claude",
  "server_port",
]);

const READ_ONLY_CONFIG_KEYS = new Set(["server_port"]);

const SYSTEM_CONFIG_DESCRIPTIONS: Record<string, { ko: string; en: string }> = {
  kanban_manager_channel_id: {
    ko: "칸반 상태 변경과 자동화 명령을 수신하는 Discord 채널입니다.",
    en: "Discord channel used for kanban state changes and automation commands.",
  },
  deadlock_manager_channel_id: {
    ko: "교착 상태나 멈춤 감지를 보고하는 Discord 채널입니다.",
    en: "Discord channel that receives deadlock and stalled-work alerts.",
  },
  review_enabled: {
    ko: "리뷰 단계를 전체 파이프라인에 적용할지 결정합니다.",
    en: "Controls whether the review step is enforced across the pipeline.",
  },
  max_review_rounds: {
    ko: "한 작업이 반복 리뷰를 수행할 수 있는 최대 횟수입니다.",
    en: "Maximum number of repeated review rounds allowed for one task.",
  },
  pm_decision_gate_enabled: {
    ko: "PM 판단 게이트를 거쳐야 다음 단계로 전환됩니다.",
    en: "Requires PM decision gate approval before the next transition.",
  },
  merge_automation_enabled: {
    ko: "허용된 작성자의 PR을 조건 충족 시 자동 머지합니다.",
    en: "Automatically merges eligible PRs from allowed authors when checks pass.",
  },
  merge_strategy: {
    ko: "자동 머지 시 사용할 GitHub 머지 전략입니다.",
    en: "GitHub merge strategy used by merge automation.",
  },
  merge_strategy_mode: {
    ko: "터미널 카드에서 direct merge를 먼저 시도할지, 항상 PR을 만들지 결정합니다.",
    en: "Chooses whether terminal cards try direct merge first or always open a PR.",
  },
  merge_allowed_authors: {
    ko: "자동 머지를 허용할 작성자 목록입니다. 쉼표로 구분합니다.",
    en: "Comma-separated list of authors allowed for automated merge.",
  },
  server_port: {
    ko: "서버가 실제로 바인딩한 API 포트입니다. 부팅 시 설정 파일에서 다시 동기화되어 읽기 전용으로 취급해야 합니다.",
    en: "The actual API port bound by the server. It is synced from server config on boot and should be treated as read-only.",
  },
  requested_timeout_min: {
    ko: "requested 상태에서 오래 머무는 카드를 경고하는 기준입니다.",
    en: "Timeout threshold for cards stuck in requested state.",
  },
  in_progress_stale_min: {
    ko: "in_progress 상태가 정체로 간주되는 기준 시간입니다.",
    en: "Threshold for considering in-progress work stale.",
  },
  context_compact_percent: {
    ko: "공통 컨텍스트 compact 기준입니다.",
    en: "Global threshold for context compaction.",
  },
  context_compact_percent_codex: {
    ko: "Codex 전용 컨텍스트 compact 기준입니다.",
    en: "Provider-specific context compaction threshold for Codex.",
  },
  context_compact_percent_claude: {
    ko: "Claude 전용 컨텍스트 compact 기준입니다.",
    en: "Provider-specific context compaction threshold for Claude.",
  },
  narrate_progress: {
    ko: "Discord 응답에서 중간 진행 설명을 기본적으로 포함할지 결정합니다.",
    en: "Controls whether Discord replies include progress narration by default.",
  },
};

const SYSTEM_CATEGORY_META = {
  pipeline: {
    titleKo: "파이프라인",
    titleEn: "Pipeline",
    descriptionKo: "칸반 흐름과 상태 전환에 직접 영향을 주는 값입니다.",
    descriptionEn: "Values that directly affect kanban flow and transitions.",
  },
  review: {
    titleKo: "리뷰",
    titleEn: "Review",
    descriptionKo: "리뷰 단계 활성화와 반복 횟수를 정의합니다.",
    descriptionEn: "Defines review enablement and repetition limits.",
  },
  timeout: {
    titleKo: "타임아웃",
    titleEn: "Timeouts",
    descriptionKo: "정체 감지와 자동 알림 시점을 조정합니다.",
    descriptionEn: "Tunes stale detection and automatic alert timing.",
  },
  dispatch: {
    titleKo: "디스패치",
    titleEn: "Dispatch",
    descriptionKo: "작업 fan-out과 체인 깊이 한계를 관리합니다.",
    descriptionEn: "Controls task fan-out and chain-depth limits.",
  },
  context: {
    titleKo: "컨텍스트",
    titleEn: "Context",
    descriptionKo: "세션 compact 임계값처럼 모델별 컨텍스트 정책을 관리합니다.",
    descriptionEn: "Manages model-specific context policies such as compaction thresholds.",
  },
  system: {
    titleKo: "시스템",
    titleEn: "System",
    descriptionKo: "서버와 Discord 라우팅에 연결되는 핵심 시스템 값입니다.",
    descriptionEn: "Core system values tied to server and Discord routing.",
  },
} as const;

const AUDIT_NOTES: AuditNote[] = [
  {
    id: "settings-json-merge",
    titleKo: "회사 설정 JSON은 전체 덮어쓰기 모델",
    titleEn: "Company settings JSON uses full replacement",
    descriptionKo: "`/api/settings`는 patch merge가 아니라 body 전체를 저장합니다. 설정 UI는 반드시 현재 값과 patch를 합친 merged object를 보내야 hidden key 손실을 막을 수 있습니다.",
    descriptionEn: "`/api/settings` stores the full body rather than merging patches. The settings UI must send a merged object to avoid losing hidden keys.",
    keys: ["settings"],
    status: "backend-followup",
  },
  {
    id: "server-port-readonly",
    titleKo: "`server_port`는 사실상 읽기 전용",
    titleEn: "`server_port` is effectively read-only",
    descriptionKo: "`src/server/mod.rs`에서 서버 부팅 시 `config.server.port` 값으로 매번 다시 기록합니다. UI에서 수정 가능한 설정처럼 보이면 오해를 만듭니다.",
    descriptionEn: "`src/server/mod.rs` rewrites it from `config.server.port` on every boot. Treating it as editable in the UI is misleading.",
    keys: ["server_port"],
    status: "read-only",
  },
  {
    id: "context-clear-gap",
    titleKo: "`context_clear_*`는 설명은 있지만 settings API에 없음",
    titleEn: "`context_clear_*` is described but not exposed by settings API",
    descriptionKo: "`SettingsView` 설명에는 등장하지만 `/api/settings/config` whitelist에는 없습니다. dead config인지, 빠진 API 항목인지 ADK 본체 정리가 필요합니다.",
    descriptionEn: "The UI descriptions reference it, but `/api/settings/config` does not expose it. ADK core should decide whether it is dead config or a missing API field.",
    keys: ["context_clear_percent", "context_clear_idle_minutes"],
    status: "backend-followup",
  },
  {
    id: "onboarding-secrets",
    titleKo: "온보딩 관련 설정은 별도 API/DB 전용",
    titleEn: "Onboarding settings are managed through a dedicated API/DB path",
    descriptionKo: "봇 토큰, guild/owner/provider, 보조 command token은 `/api/onboarding/*`와 개별 `kv_meta` 키로 관리됩니다. 일반 설정창에 text input으로 섞기보다 전용 온보딩 흐름으로 유지하는 편이 안전합니다.",
    descriptionEn: "Bot tokens, guild/owner/provider, and secondary command tokens are managed via `/api/onboarding/*` and dedicated `kv_meta` keys. They should stay behind onboarding-specific flows.",
    keys: [
      "onboarding_bot_token",
      "onboarding_guild_id",
      "onboarding_owner_id",
      "onboarding_announce_token",
      "onboarding_notify_token",
      "onboarding_command_token_2",
      "onboarding_provider",
      "onboarding_command_provider_2",
    ],
    status: "managed-elsewhere",
  },
  {
    id: "room-theme-multipath",
    titleKo: "`roomThemes`는 단일 정본이 아님",
    titleEn: "`roomThemes` is not a single-source setting",
    descriptionKo: "`dashboard/src/app/office-workflow-pack.ts`에서 preset room theme와 custom room theme를 합쳐 사용합니다. 단순 일반 설정 필드보다 office/visual 편집 흐름에서 관리하는 편이 맞습니다.",
    descriptionEn: "`dashboard/src/app/office-workflow-pack.ts` merges preset room themes with custom room themes. It fits office/visual editing better than a generic settings form.",
    keys: ["roomThemes"],
    status: "managed-elsewhere",
  },
  {
    id: "typed-only-company-settings",
    titleKo: "타입에는 있지만 현재 소비/편집 경로가 확인되지 않은 회사 설정",
    titleEn: "Company settings that exist in types but have no confirmed editor/consumer path",
    descriptionKo: "현재 audit 기준으로 아래 필드들은 `CompanySettings` 타입에는 있지만 실제 사용처나 편집 화면이 확인되지 않았습니다. 제거/활성화/문서화 중 하나로 정리해야 합니다.",
    descriptionEn: "In the current audit, the following fields exist in `CompanySettings` but have no confirmed editor or runtime consumer. They should be removed, activated, or documented.",
    keys: [
      "autoUpdateEnabled",
      "autoUpdateNoticePending",
      "oauthAutoSwap",
      "officeWorkflowPack",
      "providerModelConfig",
      "messengerChannels",
      "officePackProfiles",
      "officePackHydratedPacks",
    ],
    status: "typed-only",
  },
  {
    id: "merge-automation-gap",
    titleKo: "merge automation 설정은 policy에서 읽지만 UI/API에는 없음",
    titleEn: "Merge automation settings are consumed by policy but absent from UI/API",
    descriptionKo: "`merge_automation_enabled`, `merge_strategy`, `merge_allowed_authors`는 `policies/merge-automation.js`에서 실제로 사용되지만 `/api/settings/config` whitelist와 현재 설정 UI에는 없습니다. 운영자가 dashboard에서 설정을 설명/수정할 수 있도록 ADK 본체 정리가 필요합니다.",
    descriptionEn: "`merge_automation_enabled`, `merge_strategy`, and `merge_allowed_authors` are consumed by `policies/merge-automation.js`, but they are absent from `/api/settings/config` and the current settings UI. ADK core cleanup is needed so the dashboard can truthfully explain and edit them.",
    keys: ["merge_automation_enabled", "merge_strategy", "merge_allowed_authors"],
    status: "backend-followup",
  },
  {
    id: "workspace-fallback-gap",
    titleKo: "`workspace`는 policy fallback에서 읽지만 정본이 아님",
    titleEn: "`workspace` is read as a policy fallback but is not a canonical config surface",
    descriptionKo: "`policies/timeouts.js`는 마지막 fallback으로 `agentdesk.config.get('workspace')`를 읽지만, `agentdesk.config.get()`은 `kv_meta`만 조회합니다. 실제 workspace 정본은 agent/session/runtime 쪽에 퍼져 있어서 일반 설정값처럼 설명하면 오해가 생깁니다.",
    descriptionEn: "`policies/timeouts.js` reads `agentdesk.config.get('workspace')` as a final fallback, but `agentdesk.config.get()` only queries `kv_meta`. The real workspace source-of-truth lives across agent, session, and runtime surfaces, so presenting it as a normal setting would be misleading.",
    keys: ["workspace"],
    status: "backend-followup",
  },
  {
    id: "max-chain-depth-consumer-gap",
    titleKo: "`max_chain_depth`는 노출되지만 실제 소비처가 확인되지 않음",
    titleEn: "`max_chain_depth` is exposed but has no confirmed runtime consumer",
    descriptionKo: "`/api/settings/config` whitelist에는 포함되어 있지만, 현재 코드 검색 기준으로 실제 런타임 소비처는 확인되지 않았습니다. dead config인지 누락된 연결인지 ADK 본체에서 정리해야 합니다.",
    descriptionEn: "It is included in the `/api/settings/config` whitelist, but the current code audit did not find a confirmed runtime consumer. ADK core should decide whether it is dead config or a missing integration.",
    keys: ["max_chain_depth"],
    status: "backend-followup",
  },
];

function isBooleanConfigKey(key: string): boolean {
  return BOOLEAN_CONFIG_KEYS.has(key);
}

function isNumericConfigKey(key: string): boolean {
  return NUMERIC_CONFIG_KEYS.has(key);
}

function isReadOnlyConfigKey(key: string): boolean {
  return READ_ONLY_CONFIG_KEYS.has(key);
}

function hasConfigValue(value: ConfigEditValue | string | null | undefined): boolean {
  if (value === null || value === undefined) return false;
  if (typeof value === "string") return value.trim().length > 0;
  return true;
}

function parseBooleanConfigValue(value: string | boolean | null | undefined): boolean {
  if (typeof value === "boolean") return value;
  const normalized = String(value ?? "").trim().toLowerCase();
  return normalized === "true" || normalized === "1" || normalized === "yes" || normalized === "on";
}

function formatUnit(value: number, unit: string): string {
  if (unit === "s" && value >= 60) {
    const m = Math.floor(value / 60);
    const s = value % 60;
    return s > 0 ? `${m}m${s}s` : `${m}m`;
  }
  if (unit === "min" && value >= 60) {
    const h = Math.floor(value / 60);
    const m = value % 60;
    return m > 0 ? `${h}h${m}m` : `${h}h`;
  }
  return unit ? `${value}${unit}` : `${value}`;
}

function auditStatusLabel(status: AuditNoteStatus, isKo: boolean): string {
  if (isKo) {
    if (status === "read-only") return "읽기 전용";
    if (status === "managed-elsewhere") return "별도 관리";
    if (status === "typed-only") return "타입 전용 후보";
    return "본체 정리 필요";
  }
  if (status === "read-only") return "Read-only";
  if (status === "managed-elsewhere") return "Managed elsewhere";
  if (status === "typed-only") return "Typed-only candidate";
  return "Core cleanup needed";
}

function auditStatusClass(status: AuditNoteStatus): string {
  if (status === "read-only") return "border-slate-400/30 bg-slate-400/10 text-slate-200";
  if (status === "managed-elsewhere") return "border-emerald-400/30 bg-emerald-400/10 text-emerald-200";
  return "border-sky-400/30 bg-sky-400/10 text-sky-100";
}

function configLayerLabel(overrideActive: boolean, isKo: boolean): string {
  if (overrideActive) return isKo ? "실시간 override" : "Live override";
  return isKo ? "기준값" : "Baseline";
}

function configLayerClass(overrideActive: boolean): string {
  if (overrideActive) return "border-amber-400/30 bg-amber-400/10 text-amber-100";
  return "border-emerald-400/30 bg-emerald-400/10 text-emerald-100";
}

function baselineSourceNote(source: string | null | undefined, isKo: boolean): string | null {
  if (source === "yaml") return isKo ? "기준값 출처: agentdesk.yaml" : "Baseline source: agentdesk.yaml";
  if (source === "hardcoded") return isKo ? "기준값 출처: 하드코딩 기본값" : "Baseline source: hardcoded default";
  if (source === "config") return isKo ? "기준값 출처: 서버 설정" : "Baseline source: server config";
  return null;
}

function restartBehaviorNote(behavior: string | null | undefined, isKo: boolean): string | null {
  if (behavior === "reseed-from-yaml") {
    return isKo
      ? "재시작 시 YAML baseline이 다시 적용됩니다."
      : "Restart re-applies the YAML baseline.";
  }
  if (behavior === "persist-live-override") {
    return isKo
      ? "재시작 후에도 현재 live override가 유지됩니다."
      : "The live override persists across restart.";
  }
  if (behavior === "reset-to-baseline") {
    return isKo
      ? "reset flag가 켜져 있어 재시작 시 baseline으로 초기화됩니다."
      : "The reset flag clears this back to baseline on restart.";
  }
  if (behavior === "clear-on-restart") {
    return isKo
      ? "reset flag가 켜져 있어 재시작 시 override가 제거됩니다."
      : "The reset flag removes this override on restart.";
  }
  if (behavior === "config-only") {
    return isKo
      ? "서버 설정에서 직접 읽는 값이라 대시보드에서 바꾸지 않습니다."
      : "This comes directly from server config and is not edited here.";
  }
  return null;
}

interface SectionHeadingProps {
  eyebrow: string;
  title: string;
  description: string;
  badge?: string;
}

function SectionHeading({ eyebrow, title, description, badge }: SectionHeadingProps) {
  return (
    <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
      <div className="flex items-center gap-2 min-w-0">
        <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
          {eyebrow}
        </div>
        <h3 className="text-base font-semibold tracking-tight" style={{ color: "var(--th-text)" }}>
          {title}
        </h3>
        <span className="cursor-help text-xs" style={{ color: "var(--th-text-muted)" }} title={description}>
          ⓘ
        </span>
        {badge && (
          <span
            className="inline-flex shrink-0 items-center rounded-full border px-2 py-0.5 text-[10px] font-medium"
            style={{ borderColor: "rgba(99,102,241,0.32)", background: "rgba(99,102,241,0.12)", color: "#c7d2fe" }}
          >
            {badge}
          </span>
        )}
      </div>
    </div>
  );
}

interface SummaryCardProps {
  label: string;
  value: string;
  description: string;
  accent?: string;
}

function SummaryCard({ label, value, description, accent = "var(--th-accent-primary)" }: SummaryCardProps) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "rgba(148,163,184,0.16)",
        background: `linear-gradient(180deg, color-mix(in srgb, ${accent} 12%, var(--th-card-bg) 88%) 0%, color-mix(in srgb, var(--th-bg-surface) 92%, transparent) 100%)`,
      }}
    >
      <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-2 text-2xl font-semibold" style={{ color: "var(--th-text)" }}>
        {value}
      </div>
      <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {description}
      </p>
    </SettingsCard>
  );
}

interface SurfaceCardProps {
  title: string;
  body: string;
  footer: string;
}

function SurfaceCard({ title, body, footer }: SurfaceCardProps) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{ borderColor: "rgba(148,163,184,0.14)", background: "rgba(15,23,42,0.34)" }}
    >
      <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {title}
      </div>
      <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
        {body}
      </p>
      <div className="mt-3 text-[11px] font-medium uppercase tracking-[0.16em]" style={{ color: "var(--th-text-secondary)" }}>
        {footer}
      </div>
    </SettingsCard>
  );
}

interface InputCardProps {
  label: string;
  description: string;
  children: ReactNode;
}

function InputCard({ label, description, children }: InputCardProps) {
  return (
    <div
      className="rounded-2xl border p-3"
      style={{ borderColor: "rgba(148,163,184,0.18)", background: "rgba(15,23,42,0.28)" }}
    >
      <label className="flex items-center gap-1.5 text-sm font-medium" style={{ color: "var(--th-text)" }}>
        {label}
        <span className="cursor-help text-xs" style={{ color: "var(--th-text-muted)" }} title={description}>
          ⓘ
        </span>
      </label>
      <div className="mt-2">{children}</div>
    </div>
  );
}

interface AuditNoteCardProps {
  note: AuditNote;
  isKo: boolean;
}

function AuditNoteCard({ note, isKo }: AuditNoteCardProps) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}
    >
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
            {isKo ? note.titleKo : note.titleEn}
          </div>
          <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            {isKo ? note.descriptionKo : note.descriptionEn}
          </p>
        </div>
        <span className={`inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[11px] font-medium ${auditStatusClass(note.status)}`}>
          {auditStatusLabel(note.status, isKo)}
        </span>
      </div>
      <div className="mt-3 flex flex-wrap gap-2">
        {note.keys.map((key) => (
          <span
            key={key}
            className="inline-flex items-center rounded-full border px-2.5 py-1 text-[11px]"
            style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-secondary)" }}
          >
            {key}
          </span>
        ))}
      </div>
    </SettingsCard>
  );
}

export default function SettingsView({
  settings,
  onSave,
  isKo,
}: SettingsViewProps) {
  const [companyName, setCompanyName] = useState(settings.companyName);
  const [ceoName, setCeoName] = useState(settings.ceoName);
  const [language, setLanguage] = useState(settings.language);
  const [theme, setTheme] = useState(settings.theme);
  const [saving, setSaving] = useState(false);
  const tr = (ko: string, en: string) => (isKo ? ko : en);

  const [rcValues, setRcValues] = useState<Record<string, number>>({});
  const [rcDefaults, setRcDefaults] = useState<Record<string, number>>({});
  const [rcLoaded, setRcLoaded] = useState(false);
  const [rcSaving, setRcSaving] = useState(false);
  const [rcDirty, setRcDirty] = useState(false);
  const [escalationSettings, setEscalationSettings] = useState<api.EscalationSettings | null>(null);
  const [escalationDefaults, setEscalationDefaults] = useState<api.EscalationSettings | null>(null);
  const [escalationBaseline, setEscalationBaseline] = useState<api.EscalationSettings | null>(null);
  const [escalationSaving, setEscalationSaving] = useState(false);

  const [configEntries, setConfigEntries] = useState<ConfigEntry[]>([]);
  const [configEdits, setConfigEdits] = useState<Record<string, ConfigEditValue>>({});
  const [configSaving, setConfigSaving] = useState(false);
  const [showOnboarding, setShowOnboarding] = useState(false);

  useEffect(() => {
    setCompanyName(settings.companyName);
    setCeoName(settings.ceoName);
    setLanguage(settings.language);
    setTheme(settings.theme);
  }, [settings.companyName, settings.ceoName, settings.language, settings.theme]);

  useEffect(() => {
    void api.getRuntimeConfig()
      .then((data) => {
        setRcValues(data?.current ?? {});
        setRcDefaults(data?.defaults ?? {});
        setRcLoaded(true);
      })
      .catch(() => {
        setRcLoaded(true);
      });

    void fetch("/api/settings/config", { credentials: "include" })
      .then((response) => response.json())
      .then((data: { entries: ConfigEntry[] }) => setConfigEntries(data.entries || []))
      .catch(() => {});
  }, []);

  const companyDirty =
    companyName !== settings.companyName ||
    ceoName !== settings.ceoName ||
    language !== settings.language ||
    theme !== settings.theme;
  const configDirty = Object.keys(configEdits).length > 0;
  const runtimeFieldCount = CATEGORIES.reduce((sum, category) => sum + category.fields.length, 0);

  const inputStyle: CSSProperties = {
    background: "var(--th-bg-surface)",
    border: "1px solid var(--th-border)",
    color: "var(--th-text)",
  };
  const primaryActionClass = "inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl px-5 py-2.5 text-sm font-medium text-white transition-colors disabled:opacity-50";
  const primaryActionStyle: CSSProperties = {
    background: "var(--th-accent-primary)",
  };
  const secondaryActionClass = "inline-flex min-h-[44px] items-center justify-center rounded-2xl border px-5 py-2.5 text-sm font-medium transition-[opacity,color,border-color] hover:opacity-100";
  const secondaryActionStyle: CSSProperties = {
    borderColor: "rgba(148,163,184,0.28)",
    color: "var(--th-text-secondary)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };
  const compactSecondaryActionClass = "inline-flex items-center rounded-full border px-3 py-1 text-[11px] font-medium transition-[opacity,color,border-color] hover:opacity-100";
  const compactSecondaryActionStyle: CSSProperties = {
    borderColor: "rgba(148,163,184,0.2)",
    color: "var(--th-text-muted)",
    background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
  };

  const handleSave = async () => {
    setSaving(true);
    try {
      await onSave({ companyName, ceoName, language, theme });
    } finally {
      setSaving(false);
    }
  };

  const handleRcSave = async () => {
    setRcSaving(true);
    try {
      await api.saveRuntimeConfig(rcValues);
      setRcDirty(false);
    } finally {
      setRcSaving(false);
    }
  };

  const handleRcChange = (key: string, value: number) => {
    setRcValues((prev) => ({ ...prev, [key]: value }));
    setRcDirty(true);
  };

  const handleRcReset = (key: string) => {
    if (rcDefaults[key] !== undefined) {
      setRcValues((prev) => ({ ...prev, [key]: rcDefaults[key] }));
      setRcDirty(true);
    }
  };

  const handleConfigEdit = (key: string, value: ConfigEditValue) => {
    if (isReadOnlyConfigKey(key)) return;
    setConfigEdits((prev) => ({ ...prev, [key]: value }));
  };

  const handleConfigSave = async () => {
    if (!configDirty) return;
    setConfigSaving(true);
    try {
      await fetch("/api/settings/config", {
        method: "PATCH",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(configEdits),
      });
      setConfigEdits({});
      const response = await fetch("/api/settings/config", { credentials: "include" });
      const data = await response.json();
      setConfigEntries(data.entries || []);
    } finally {
      setConfigSaving(false);
    }
  };

  return (
    <div
      className="mx-auto w-full max-w-5xl min-w-0 space-y-6 overflow-x-hidden px-4 py-5 pb-40 sm:h-full sm:overflow-y-auto sm:px-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <section
        className="rounded-[28px] border p-5 sm:p-6"
        style={{
          borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
          background: "radial-gradient(circle at top left, color-mix(in srgb, var(--th-accent-primary-soft) 78%, transparent), color-mix(in srgb, var(--th-card-bg) 92%, transparent) 48%, color-mix(in srgb, var(--th-bg-surface) 94%, transparent) 100%)",
        }}
      >
        <div className="flex flex-col gap-4 xl:flex-row xl:items-start xl:justify-between">
          <div className="min-w-0">
            <div className="text-[11px] font-semibold uppercase tracking-[0.22em]" style={{ color: "var(--th-text-secondary)" }}>
              {tr("설정 제어실", "Settings Control Room")}
            </div>
            <h2 className="mt-2 text-2xl font-semibold tracking-tight sm:text-3xl" style={{ color: "var(--th-text)" }}>
              {tr("AgentDesk 설정창 재정렬", "Reframing AgentDesk settings")}
            </h2>
            <p className="mt-3 max-w-3xl text-sm leading-6" style={{ color: "var(--th-text-secondary)" }}>
              {tr(
                "설정은 단순 입력 폼이 아니라 저장 위치와 적용 범위를 이해해야 안전하게 다룰 수 있습니다. 이 화면은 회사 설정, 즉시 반영 런타임 설정, 파이프라인 정책, 별도 관리 설정을 분리해서 보여줍니다.",
                "Settings are safe only when their storage surface and effect scope are visible. This view separates company settings, live runtime tuning, pipeline policy keys, and settings managed elsewhere.",
              )}
            </p>
          </div>
          <div
            className="rounded-2xl border px-4 py-3 text-sm"
            style={{ borderColor: "rgba(148,163,184,0.2)", background: "rgba(15,23,42,0.38)", color: "var(--th-text-secondary)" }}
          >
            {tr("현재 일반 설정 저장은 merged object로 수행되어 hidden JSON key를 보존합니다.", "General settings now save as a merged object so hidden JSON keys stay intact.")}
          </div>
        </div>

        <div className="mt-6 grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
          <SummaryCard
            label={tr("회사 설정", "Company Settings")}
            value="4"
            description={tr("이 화면에서 직접 편집하는 브랜드/언어/테마 설정", "Brand, language, and theme controls edited directly here")}
          />
          <SummaryCard
            label={tr("런타임 튜닝", "Live Runtime")}
            value={String(runtimeFieldCount)}
            description={tr("재시작 없이 즉시 반영되는 운영 숫자 설정", "Operational tuning values that apply without restart")}
            accent="#22c55e"
          />
          <SummaryCard
            label={tr("정책 키", "Policy Keys")}
            value={String(configEntries.length)}
            description={tr("개별 `kv_meta` 키로 저장되는 파이프라인 정책", "Pipeline policy keys stored as individual `kv_meta` entries")}
            accent="#f59e0b"
          />
          <SummaryCard
            label={tr("정리 대상", "Audit Findings")}
            value={String(AUDIT_NOTES.length)}
            description={tr("별도 관리/읽기 전용/정리 필요로 분류한 설정 이슈", "Settings that are managed elsewhere, read-only, or need core cleanup")}
            accent="#fb7185"
          />
        </div>
      </section>

      <SettingsSection
          eyebrow={tr("저장 경로", "Storage Surfaces")}
          title={tr("어디에 저장되는지 먼저 보이게", "Make storage surfaces explicit")}
          description={tr(
            "설정이 한 곳에만 있지 않아서, 저장 경로를 이해하지 못하면 UI가 쉽게 거짓말을 하게 됩니다. 아래 카드는 현재 AgentDesk 설정의 실제 저장면을 요약합니다.",
            "Settings do not live in one place, so the UI becomes misleading unless the storage path is explicit. These cards summarize the current storage surfaces.",
          )}
        >

        <div className="mt-5 grid gap-3 lg:grid-cols-2 2xl:grid-cols-4">
          <SurfaceCard
            title={tr("회사 설정 JSON", "Company settings JSON")}
            body={tr(
              "`/api/settings`가 `kv_meta['settings']` 전체 JSON을 저장합니다. 부분 patch가 아니라 full replace라서, 저장할 때 기존 값을 합친 merged object가 필요합니다.",
              "`/api/settings` stores the full `kv_meta['settings']` JSON. It is a full replace rather than a patch merge, so callers must send a merged object.",
            )}
            footer={tr("source: kv_meta['settings']", "source: kv_meta['settings']")}
          />
          <SurfaceCard
            title={tr("런타임 설정", "Runtime config")}
            body={tr(
              "폴링 주기와 cache TTL 같은 숫자 설정은 `kv_meta['runtime-config']`에 저장되고 재시작 없이 반영됩니다.",
              "Polling intervals and cache TTL values live in `kv_meta['runtime-config']` and apply without restart.",
            )}
            footer={tr("source: kv_meta['runtime-config']", "source: kv_meta['runtime-config']")}
          />
          <SurfaceCard
            title={tr("정책/파이프라인 키", "Policy and pipeline keys")}
            body={tr(
              "리뷰, 타임아웃, context compact 같은 값은 개별 `kv_meta` 키로 저장되고 `/api/settings/config` whitelist를 통해서만 노출됩니다.",
              "Review, timeout, and context-compaction values are stored as individual `kv_meta` keys and only exposed through the `/api/settings/config` whitelist.",
            )}
            footer={tr("source: individual kv_meta keys", "source: individual kv_meta keys")}
          />
          <SurfaceCard
            title={tr("온보딩/시크릿", "Onboarding and secrets")}
            body={tr(
              "토큰과 온보딩 provider 설정은 일반 설정창이 아니라 전용 온보딩 API와 wizard에서 관리됩니다.",
              "Tokens and onboarding providers are managed through a dedicated onboarding API and wizard instead of the general settings form.",
            )}
            footer={tr("source: onboarding API + kv_meta", "source: onboarding API + kv_meta")}
          />
        </div>
      </SettingsSection>

      <SettingsSection
          eyebrow={tr("회사 설정", "Workspace Identity")}
          title={tr("브랜드/언어/테마", "Brand, language, and theme")}
          description={tr(
            "이 섹션은 대시보드가 사람에게 어떻게 보일지 결정합니다. 저장 시 현재 `settings` JSON 전체와 합쳐 저장하여 숨겨진 키를 지킵니다.",
            "This section controls how the dashboard presents itself to people. Saves are merged with the current `settings` JSON so hidden keys are preserved.",
          )}
          badge={tr("full replace API → merged save", "full replace API → merged save")}
        >

        <div className="mt-5 grid gap-3 lg:grid-cols-2">
          <SettingsFieldCard
            label={tr("회사 이름", "Company name")}
            description={tr("대시보드 hero와 주요 타이틀에 노출됩니다.", "Shown in the dashboard hero and primary titles.")}
          >
            <input
              type="text"
              value={companyName}
              onChange={(event) => setCompanyName(event.target.value)}
              className="w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
            />
          </SettingsFieldCard>

          <SettingsFieldCard
            label={tr("CEO 이름", "CEO name")}
            description={tr("오피스와 일부 운영 UI에서 대표 인물 이름으로 사용됩니다.", "Used as the representative persona name in office and ops surfaces.")}
          >
            <input
              type="text"
              value={ceoName}
              onChange={(event) => setCeoName(event.target.value)}
              className="w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
            />
          </SettingsFieldCard>

          <SettingsFieldCard
            label={tr("언어", "Language")}
            description={tr("대시보드 전반의 로캘과 기본 텍스트 방향을 정합니다.", "Controls dashboard locale and default text language.")}
          >
            <select
              value={language}
              onChange={(event) => setLanguage(event.target.value as typeof language)}
              className="w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
            >
              <option value="ko">한국어</option>
              <option value="en">English</option>
              <option value="ja">日本語</option>
              <option value="zh">中文</option>
            </select>
          </SettingsFieldCard>

          <SettingsFieldCard
            label={tr("테마", "Theme")}
            description={tr("대시보드 전체 테마와 오피스 화면의 기본 분위기를 정합니다.", "Sets the overall dashboard theme and base office mood.")}
          >
            <select
              value={theme}
              onChange={(event) => setTheme(event.target.value as typeof theme)}
              className="w-full rounded-2xl px-3 py-2.5 text-sm"
              style={inputStyle}
            >
              <option value="dark">{tr("다크", "Dark")}</option>
              <option value="light">{tr("라이트", "Light")}</option>
              <option value="auto">{tr("자동 (시스템)", "Auto (System)")}</option>
            </select>
          </SettingsFieldCard>
        </div>

        <SettingsCallout className="mt-5" action={
            <button
              onClick={handleSave}
              disabled={saving || !companyDirty}
              className={primaryActionClass}
              style={primaryActionStyle}
            >
              {saving ? tr("저장 중...", "Saving...") : tr("회사 설정 저장", "Save company settings")}
            </button>
        }>
          <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "이 저장 버튼은 현재 회사 설정 patch를 기존 `settings` JSON과 합쳐 보냅니다. `roomThemes`처럼 화면에 안 보이는 키가 저장 중 사라지지 않도록 하기 위한 방어선입니다.",
              "This save action merges the edited patch with the existing `settings` JSON. It prevents hidden keys such as `roomThemes` from being wiped during save.",
            )}
          </p>
        </SettingsCallout>
      </SettingsSection>

      <SettingsSection
          eyebrow={tr("즉시 반영", "Live Runtime")}
          title={tr("운영 리듬과 캐시 튜닝", "Tune runtime cadence and caching")}
          description={tr(
            "이 값들은 `runtime-config`에 저장되고 재시작 없이 반영됩니다. 장애 복구 속도, GitHub 동기화 리듬, 사용량 경고 민감도 같은 운영 감각을 조절하는 영역입니다.",
            "These values are saved to `runtime-config` and apply without restart. They tune recovery cadence, GitHub sync rhythm, and usage-alert sensitivity.",
          )}
          badge={tr("no restart needed", "no restart needed")}
        >

        {!rcLoaded ? (
          <SettingsEmptyState className="mt-5 text-sm">
            {tr("런타임 설정을 불러오는 중...", "Loading runtime config...")}
          </SettingsEmptyState>
        ) : (
          <div className="mt-5 space-y-5">
            {CATEGORIES.map((category) => (
              <SettingsSubsection
                key={category.titleEn}
                title={tr(category.titleKo, category.titleEn)}
                description={tr(category.descriptionKo, category.descriptionEn)}
              >
                <div className="grid gap-3 xl:grid-cols-2">
                  {category.fields.map((field) => {
                    const value = rcValues[field.key] ?? rcDefaults[field.key] ?? 0;
                    const defaultValue = rcDefaults[field.key] ?? 0;
                    const isDefault = value === defaultValue;

                    return (
                      <SettingsCard
                        key={field.key}
                        className="rounded-2xl p-4"
                        style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.32)" }}
                      >
                        <div className="flex items-start justify-between gap-3">
                          <div className="min-w-0">
                            <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                              {tr(field.labelKo, field.labelEn)}
                            </div>
                            <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                              {tr(field.descriptionKo, field.descriptionEn)}
                            </p>
                          </div>
                          <div className="shrink-0 text-right">
                            <div className="text-sm font-semibold" style={{ color: isDefault ? "var(--th-text)" : "#fbbf24" }}>
                              {formatUnit(value, field.unit)}
                            </div>
                            <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              {tr("기본값", "Default")}: {formatUnit(defaultValue, field.unit)}
                            </div>
                          </div>
                        </div>

                        <div className="mt-4 flex items-center gap-3">
                          <input
                            type="range"
                            min={field.min}
                            max={field.max}
                            step={field.step}
                            value={value}
                            onChange={(event) => handleRcChange(field.key, Number(event.target.value))}
                            className="h-1.5 flex-1 cursor-pointer appearance-none rounded-full"
                            style={{ accentColor: "var(--th-accent-primary)" }}
                          />
                          <input
                            type="number"
                            min={field.min}
                            max={field.max}
                            step={field.step}
                            value={value}
                            onChange={(event) => {
                              const next = Number(event.target.value);
                              if (Number.isFinite(next) && next >= field.min && next <= field.max) {
                                handleRcChange(field.key, next);
                              }
                            }}
                            className="w-20 rounded-xl px-2.5 py-1.5 text-right text-xs font-mono"
                            style={inputStyle}
                          />
                        </div>

                        {!isDefault && (
                          <div className="mt-3 flex justify-end">
                            <button
                              onClick={() => handleRcReset(field.key)}
                              className={compactSecondaryActionClass}
                              style={compactSecondaryActionStyle}
                            >
                              {tr("기본값으로 되돌리기", "Reset to default")}
                            </button>
                          </div>
                        )}
                      </SettingsCard>
                    );
                  })}
                </div>
              </SettingsSubsection>
            ))}

            <SettingsCallout className="mt-0" action={
              <button
                onClick={handleRcSave}
                disabled={rcSaving || !rcDirty}
                className={primaryActionClass}
                style={primaryActionStyle}
              >
                {rcSaving ? tr("저장 중...", "Saving...") : tr("런타임 설정 저장", "Save runtime config")}
              </button>
            }>
              <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr("런타임 설정은 저장 즉시 적용됩니다. 값 조정이 잦다면 먼저 작은 폭으로 바꾸고 Pulse/영수증/로그 반응을 확인하는 편이 안전합니다.", "Runtime config applies immediately. If you tune frequently, prefer small changes first and verify Pulse, receipts, and logs before making larger moves.")}
              </p>
            </SettingsCallout>
          </div>
        )}
      </SettingsSection>

      <SettingsSection
          eyebrow={tr("파이프라인 정책", "Pipeline Policy")}
          title={tr("개별 `kv_meta` 키 관리", "Manage individual `kv_meta` keys")}
          description={tr(
            "리뷰, 타임아웃, context compact, Discord 채널 연결 값은 일반 설정 JSON이 아니라 개별 `kv_meta` 키입니다. 여기서는 토글/숫자/문자열 타입을 분리해서 보여주고, read-only 항목은 편집 대신 현재 상태만 노출합니다.",
            "Review, timeout, context-compaction, and Discord channel IDs are stored as individual `kv_meta` keys rather than the general settings JSON. This section separates toggles, numeric values, and read-only keys.",
          )}
          badge={tr("whitelisted API only", "whitelisted API only")}
        >

        {configEntries.length === 0 ? (
          <SettingsEmptyState className="mt-5 text-sm">
            {tr("시스템 설정을 불러오는 중...", "Loading system config...")}
          </SettingsEmptyState>
        ) : (
          <div className="mt-5 space-y-4">
            {(Object.keys(SYSTEM_CATEGORY_META) as Array<keyof typeof SYSTEM_CATEGORY_META>).map((categoryKey) => {
              const entries = configEntries.filter((entry) => entry.category === categoryKey);
              if (entries.length === 0) return null;
              const meta = SYSTEM_CATEGORY_META[categoryKey];

              return (
                <SettingsSubsection
                  key={categoryKey}
                  title={tr(meta.titleKo, meta.titleEn)}
                  description={tr(meta.descriptionKo, meta.descriptionEn)}
                >
                  <div className="grid gap-3 xl:grid-cols-2">
                    {entries.map((entry) => {
                      const description = SYSTEM_CONFIG_DESCRIPTIONS[entry.key];
                      const hasLocalEdit = Object.prototype.hasOwnProperty.call(configEdits, entry.key);
                      const currentValue = hasLocalEdit ? configEdits[entry.key] : (entry.value ?? entry.default ?? "");
                      const defaultLabel = entry.default ?? tr("없음", "None");
                      const readOnly = isReadOnlyConfigKey(entry.key);
                      const isEnabled = parseBooleanConfigValue(currentValue);

                      return (
                        <SettingsCard
                          key={entry.key}
                          className="rounded-2xl p-4"
                          style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.32)" }}
                        >
                          <div className="flex flex-wrap items-start justify-between gap-3">
                            <div className="min-w-0">
                              <div className="flex flex-wrap items-center gap-2">
                                <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                                  {isKo ? entry.label_ko : entry.label_en}
                                </div>
                                <span
                                  className="inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium"
                                  style={{ borderColor: "rgba(148,163,184,0.2)", color: "var(--th-text-muted)" }}
                                >
                                  kv_meta
                                </span>
                                {readOnly && (
                                  <span className={`inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium ${auditStatusClass("read-only")}`}>
                                    {auditStatusLabel("read-only", isKo)}
                                  </span>
                                )}
                              </div>
                              {description && (
                                <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                                  {isKo ? description.ko : description.en}
                                </p>
                              )}
                            </div>
                            <code className="max-w-full overflow-hidden text-ellipsis whitespace-nowrap text-[11px]" style={{ color: "var(--th-text-secondary)" }}>
                              {entry.key}
                            </code>
                          </div>

                          <div className="mt-4">
                            {isBooleanConfigKey(entry.key) ? (
                              <button
                                type="button"
                                role="switch"
                                aria-checked={isEnabled}
                                disabled={readOnly}
                                onClick={() => handleConfigEdit(entry.key, !isEnabled)}
                                className="flex min-h-[52px] w-full items-center justify-between rounded-2xl border px-3 py-3 text-left transition-colors disabled:cursor-not-allowed disabled:opacity-70"
                                style={{
                                  borderColor: isEnabled ? "rgba(16,185,129,0.35)" : "rgba(148,163,184,0.24)",
                                  background: isEnabled ? "rgba(16,185,129,0.12)" : "rgba(15,23,42,0.2)",
                                }}
                              >
                                <div className="pr-3">
                                  <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                                    {isEnabled ? tr("활성화됨", "Enabled") : tr("비활성화됨", "Disabled")}
                                  </div>
                                  <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                    {readOnly
                                      ? tr("서버 부팅 시 다시 동기화되는 항목입니다.", "This value is resynced on server boot.")
                                      : tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}
                                  </div>
                                </div>
                                <span
                                  className="relative inline-flex h-7 w-12 shrink-0 items-center rounded-full transition-colors"
                                  style={{ background: isEnabled ? "#10b981" : "rgba(148,163,184,0.32)" }}
                                >
                                  <span
                                    className="absolute h-5 w-5 rounded-full bg-white transition-transform"
                                    style={{ transform: isEnabled ? "translateX(1.55rem)" : "translateX(0.3rem)" }}
                                  />
                                </span>
                              </button>
                            ) : (
                              <>
                                <input
                                  type={isNumericConfigKey(entry.key) && !readOnly ? "number" : "text"}
                                  inputMode={isNumericConfigKey(entry.key) ? "numeric" : undefined}
                                  disabled={readOnly}
                                  className="w-full rounded-2xl px-3 py-2.5 text-sm disabled:cursor-not-allowed disabled:opacity-80"
                                  style={inputStyle}
                                  value={String(currentValue)}
                                  onChange={(event) => handleConfigEdit(entry.key, event.target.value)}
                                />
                                <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                                  <span>{tr(`기본값: ${defaultLabel}`, `Default: ${defaultLabel}`)}</span>
                                  {readOnly && (
                                    <span>{tr("이 값은 서버 설정에서 덮어써집니다.", "This value is overwritten from server config.")}</span>
                                  )}
                                  {entry.key.endsWith("_channel_id") && (
                                    <span>{tr("Discord ID는 정밀도 손실을 피하려고 문자열로 유지합니다.", "Discord IDs stay as strings to avoid precision loss.")}</span>
                                  )}
                                </div>
                              </>
                            )}
                          </div>
                        </SettingsCard>
                      );
                    })}
                  </div>
                </SettingsSubsection>
              );
            })}

            <SettingsCallout className="mt-0" action={
              <button
                onClick={handleConfigSave}
                disabled={configSaving || !configDirty}
                className="inline-flex min-h-[44px] shrink-0 items-center justify-center rounded-2xl bg-emerald-600 px-5 py-2.5 text-sm font-medium text-white transition-colors hover:bg-emerald-500 disabled:opacity-50"
              >
                {configSaving ? tr("저장 중...", "Saving...") : tr("정책 설정 저장", "Save policy settings")}
              </button>
            }>
              <p className="text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "이 섹션은 whitelist된 개별 `kv_meta` 키만 편집합니다. `context_clear_*`처럼 설명은 있지만 API에 없는 항목은 아래 audit 섹션에서 별도 정리 대상으로 표시합니다.",
                  "This section edits only whitelisted individual `kv_meta` keys. Items such as `context_clear_*` that are described but not exposed by the API are surfaced in the audit section below.",
                )}
              </p>
            </SettingsCallout>
          </div>
        )}
      </SettingsSection>

      <SettingsSection
          eyebrow={tr("감사 결과", "Audit Findings")}
          title={tr("별도 관리 / 정리 필요 항목", "Managed elsewhere / cleanup-needed items")}
          description={tr(
            "이 항목들은 일반 설정창에서 바로 편집하지 않는 편이 맞거나, frontend만으로는 정본을 보장할 수 없는 후보들입니다. ADK 본체 쪽 정리 요청의 근거 목록으로도 사용합니다.",
            "These items are either better managed outside the general settings form or cannot be made truthful from the frontend alone. This list also serves as the basis for ADK core cleanup requests.",
          )}
        >

        <div className="mt-5 grid gap-3 xl:grid-cols-2">
          {AUDIT_NOTES.map((note) => (
            <AuditNoteCard key={note.id} note={note} isKo={isKo} />
          ))}
        </div>
      </SettingsSection>

      <SettingsSection
          eyebrow={tr("온보딩", "Onboarding")}
          title={tr("토큰과 첫 설정은 별도 흐름으로", "Keep secrets and first-run setup in a dedicated flow")}
          description={tr(
            "온보딩 관련 토큰/채널/provider 값은 일반 설정 필드보다 wizard가 더 안전하고 이해하기 쉽습니다. 그래서 여기서는 직접 text input으로 섞지 않고 전용 흐름으로 다시 진입하게 했습니다.",
            "Onboarding tokens, channel IDs, and provider values are safer and easier to understand inside a dedicated wizard than in the general settings form, so this screen links back to that flow instead of embedding raw text inputs.",
          )}
          badge={tr("dashboard > discord onboarding bridge", "dashboard > Discord onboarding bridge")}
        >

        <SettingsCard
          className="mt-5 rounded-3xl p-4 sm:p-5"
          style={{ borderColor: "rgba(148,163,184,0.16)", background: "rgba(15,23,42,0.28)" }}
        >
          <div className="grid gap-4 lg:grid-cols-[1fr_auto] lg:items-center">
            <div>
              <div className="text-sm font-medium" style={{ color: "var(--th-text)" }}>
                {tr("온보딩 위저드 재실행", "Re-run onboarding wizard")}
              </div>
              <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {tr(
                  "봇 토큰, guild/owner, provider, announce/notify/command 토큰은 이 버튼으로 다시 설정합니다. 일반 설정창에서 직접 노출하지 않는 이유는 보안과 의미 설명을 같이 다루기 위해서입니다.",
                  "Use this button to reconfigure bot token, guild/owner, provider, and announce/notify/command tokens. They are kept out of the general settings form so security and meaning stay together.",
                )}
              </p>
            </div>
            <button
              onClick={() => setShowOnboarding(true)}
              className={secondaryActionClass}
              style={secondaryActionStyle}
            >
              {tr("온보딩 다시 열기", "Open onboarding again")}
            </button>
          </div>
        </SettingsCard>
      </SettingsSection>

      {showOnboarding && (
        <div className="fixed inset-0 z-50 overflow-y-auto bg-[#0a0e1a]" role="dialog" aria-modal="true" aria-label="Onboarding wizard">
          <div className="flex min-h-screen items-start justify-center pt-8 pb-16">
            <div className="w-full max-w-2xl">
              <div className="mb-2 flex justify-end px-4">
                <button
                  onClick={() => setShowOnboarding(false)}
                  className="min-h-[44px] rounded-lg border px-4 py-2.5 text-sm"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-muted)" }}
                >
                  ✕ {tr("닫기", "Close")}
                </button>
              </div>
              <Suspense fallback={<div className="py-8 text-center" style={{ color: "var(--th-text-muted)" }}>Loading...</div>}>
                <OnboardingWizard
                  isKo={isKo}
                  onComplete={() => {
                    setShowOnboarding(false);
                    window.location.reload();
                  }}
                />
              </Suspense>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
