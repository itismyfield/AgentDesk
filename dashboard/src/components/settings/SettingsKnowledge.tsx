import {
  SurfaceCard as SettingsCard,
} from "../common/SurfacePrimitives";

type AuditNoteStatus =
  | "read-only"
  | "managed-elsewhere"
  | "backend-contract"
  | "typed-only"
  | "backend-followup";

interface AuditNote {
  id: string;
  titleKo: string;
  titleEn: string;
  descriptionKo: string;
  descriptionEn: string;
  keys: string[];
  status: AuditNoteStatus;
}

const SETTINGS_GLOSSARY = [
  {
    termKo: "Dispatch",
    termEn: "Dispatch",
    definitionKo: "에이전트에게 작업을 전달하고 상태를 추적하는 실행 단위입니다.",
    definitionEn: "A unit of work handed to an agent and tracked through execution.",
  },
  {
    termKo: "Triage",
    termEn: "Triage",
    definitionKo: "새 이슈나 요청을 분류해 backlog, queue, review 중 어디로 보낼지 정하는 단계입니다.",
    definitionEn: "Classifies new issues or requests into backlog, queue, review, or another path.",
  },
  {
    termKo: "TTL",
    termEn: "TTL",
    definitionKo: "값이나 캐시가 신뢰 가능한 것으로 간주되는 유효 시간입니다.",
    definitionEn: "The time window where a value or cache entry is considered fresh.",
  },
  {
    termKo: "Context compact",
    termEn: "Context compact",
    definitionKo: "긴 대화나 로그를 요약해 다음 실행에 넘기는 컨텍스트 절약 단계입니다.",
    definitionEn: "Compacts long conversations or logs before handing them to the next run.",
  },
  {
    termKo: "Barge-in",
    termEn: "Barge-in",
    definitionKo: "음성 응답 중 사용자가 끼어들어 새 명령을 말하는 동작입니다.",
    definitionEn: "A user interruption while a voice response is still being spoken.",
  },
  {
    termKo: "NFC normalization",
    termEn: "NFC normalization",
    definitionKo: "한글과 유니코드 문자를 비교 가능한 조합형으로 정리하는 처리입니다.",
    definitionEn: "Normalizes Unicode text so Korean and other composed characters compare reliably.",
  },
] as const;

const AUDIT_NOTES: AuditNote[] = [
  {
    id: "settings-json-merge",
    titleKo: "회사 설정 JSON은 전체 덮어쓰기 모델",
    titleEn: "Company settings JSON uses full replacement",
    descriptionKo: "`/api/settings`는 patch merge가 아니라 body 전체를 저장합니다. 현재 UI는 기존 `settings` JSON과 병합해 hidden key 손실을 막아야 합니다.",
    descriptionEn: "`/api/settings` stores the full body instead of merging patches. The UI must merge with the existing `settings` JSON to avoid losing hidden keys.",
    keys: ["settings"],
    status: "backend-followup",
  },
  {
    id: "server-port-readonly",
    titleKo: "`server_port`는 사실상 읽기 전용",
    titleEn: "`server_port` is effectively read-only",
    descriptionKo: "`src/server/mod.rs`에서 서버 부팅 시 `config.server.port` 값으로 다시 기록합니다. 편집 가능한 값처럼 보이면 운영 오해를 만듭니다.",
    descriptionEn: "`src/server/mod.rs` rewrites it from `config.server.port` on boot. Presenting it as editable is misleading.",
    keys: ["server_port"],
    status: "read-only",
  },
  {
    id: "context-clear-gap",
    titleKo: "`context_clear_*`는 설명은 있지만 settings API에 없음",
    titleEn: "`context_clear_*` is described but not exposed by settings API",
    descriptionKo: "UI 설명에는 등장하지만 `/api/settings/config` whitelist에는 없습니다. dead config인지 빠진 API 항목인지 본체 정리가 필요합니다.",
    descriptionEn: "The UI descriptions mention it, but `/api/settings/config` does not expose it. ADK core should decide whether it is dead config or a missing API field.",
    keys: ["context_clear_percent", "context_clear_idle_minutes"],
    status: "backend-followup",
  },
  {
    id: "onboarding-secrets",
    titleKo: "온보딩 관련 설정은 별도 API/DB 전용",
    titleEn: "Onboarding settings are managed through a dedicated API/DB path",
    descriptionKo: "봇 토큰, guild/owner/provider, 보조 command token은 `/api/onboarding/*`와 개별 `kv_meta` 키로 관리됩니다. 일반 설정창보다 위저드가 안전합니다.",
    descriptionEn: "Bot tokens, guild/owner/provider, and secondary command tokens are managed via `/api/onboarding/*` and dedicated `kv_meta` keys. A wizard is safer than the general settings form.",
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
    descriptionKo: "`dashboard/src/app/office-workflow-pack.ts`에서 preset room theme와 custom room theme를 합쳐 사용합니다. 일반 설정 필드보다 office/visual 편집 흐름에서 관리하는 편이 맞습니다.",
    descriptionEn: "`dashboard/src/app/office-workflow-pack.ts` merges preset room themes with custom room themes. It fits office/visual editing better than a generic settings form.",
    keys: ["roomThemes"],
    status: "managed-elsewhere",
  },
  {
    id: "typed-only-company-settings",
    titleKo: "타입에는 있지만 현재 소비/편집 경로가 확인되지 않은 회사 설정",
    titleEn: "Company settings with no confirmed editor or runtime consumer",
    descriptionKo: "현재 audit 기준으로 일부 `CompanySettings` 필드는 타입에는 있지만 실제 편집 화면이나 소비처가 확인되지 않았습니다. 제거/활성화/문서화 중 하나가 필요합니다.",
    descriptionEn: "In the current audit, some `CompanySettings` fields exist in types but have no confirmed editor or runtime consumer. They should be removed, activated, or documented.",
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
    descriptionKo: "`merge_automation_enabled`, `merge_strategy`, `merge_allowed_authors`는 policy에서 실제 사용되지만 현재 settings API whitelist와 UI에는 없습니다.",
    descriptionEn: "`merge_automation_enabled`, `merge_strategy`, and `merge_allowed_authors` are consumed by policy, but they are absent from the current settings API whitelist and UI.",
    keys: ["merge_automation_enabled", "merge_strategy", "merge_allowed_authors"],
    status: "backend-followup",
  },
  {
    id: "workspace-fallback-gap",
    titleKo: "`workspace`는 policy fallback에서 읽지만 정본이 아님",
    titleEn: "`workspace` is read as a policy fallback but is not canonical",
    descriptionKo: "`agentdesk.config.get('workspace')`는 `kv_meta` fallback일 뿐이고 실제 정본은 agent/session/runtime에 퍼져 있습니다. 일반 설정값처럼 설명하면 오해가 생깁니다.",
    descriptionEn: "`agentdesk.config.get('workspace')` is only a `kv_meta` fallback. The real source of truth is spread across agent, session, and runtime surfaces.",
    keys: ["workspace"],
    status: "backend-followup",
  },
  {
    id: "max-chain-depth-consumer-gap",
    titleKo: "`max_chain_depth`는 노출되지만 실제 소비처가 확인되지 않음",
    titleEn: "`max_chain_depth` is exposed but has no confirmed runtime consumer",
    descriptionKo: "`/api/settings/config` whitelist에는 있지만 현재 코드 검색 기준으로 확실한 런타임 소비처가 보이지 않습니다. dead config인지 누락 연결인지 본체 정리가 필요합니다.",
    descriptionEn: "It is in the `/api/settings/config` whitelist, but the current code audit did not find a confirmed runtime consumer. ADK core should decide whether it is dead config or a missing integration.",
    keys: ["max_chain_depth"],
    status: "backend-followup",
  },
];

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

function AuditNoteCard({ note, isKo }: { note: AuditNote; isKo: boolean }) {
  return (
    <SettingsCard
      className="rounded-2xl p-4"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
      }}
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
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
              background: "color-mix(in srgb, var(--th-overlay-medium) 84%, transparent)",
              color: "var(--th-text-muted)",
            }}
          >
            {key}
          </span>
        ))}
      </div>
    </SettingsCard>
  );
}

export function SettingsGlossary({ isKo }: { isKo: boolean }) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  return (
    <SettingsCard
      className="settings-glossary rounded-[18px] border px-4 py-4 sm:px-5"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("용어 빠른 정의", "Quick term definitions")}
          </div>
          <p className="settings-copy mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "설정값을 바꾸기 전에 화면에 자주 나오는 운영 용어를 먼저 맞춥니다.",
              "Align on common operations terms before changing values.",
            )}
          </p>
        </div>
        <span
          className="settings-count-chip inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          {tr("핵심 6개", "6 core terms")}
        </span>
      </div>
      <dl className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {SETTINGS_GLOSSARY.map((item) => (
          <div
            key={item.termEn}
            className="rounded-2xl border px-3 py-3"
            style={{
              borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
              background: "color-mix(in srgb, var(--th-bg-surface) 88%, transparent)",
            }}
          >
            <dt className="settings-term text-xs font-semibold" style={{ color: "var(--th-text)" }}>
              {tr(item.termKo, item.termEn)}
            </dt>
            <dd className="settings-copy mt-1 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
              {tr(item.definitionKo, item.definitionEn)}
            </dd>
          </div>
        ))}
      </dl>
    </SettingsCard>
  );
}

export function SettingsAuditNotes({ isKo }: { isKo: boolean }) {
  const tr = (ko: string, en: string) => (isKo ? ko : en);
  return (
    <details
      id="settings-audit-notes"
      data-testid="settings-audit-notes"
      className="settings-audit-disclosure rounded-[20px] border"
      style={{
        borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
        background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
      }}
    >
      <summary
        className="flex cursor-pointer list-none items-start justify-between gap-3 px-4 py-4 sm:px-5"
        data-testid="settings-audit-summary"
      >
        <span className="min-w-0">
          <span className="settings-section-title block text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {tr("운영자 감사 노트", "Operator audit notes")}
          </span>
          <span className="settings-copy mt-1 block text-[12px] leading-5" style={{ color: "var(--th-text-muted)" }}>
            {tr(
              "일반 폼에 넣으면 오해를 만드는 backend-followup과 read-only 항목만 접어 둡니다.",
              "Backend-followup and read-only items stay folded away from the regular form.",
            )}
          </span>
        </span>
        <span
          className="settings-count-chip inline-flex shrink-0 items-center rounded-full border px-2.5 py-1 text-[10px] font-medium"
          style={{
            borderColor: "color-mix(in srgb, var(--th-border) 70%, transparent)",
            background: "color-mix(in srgb, var(--th-overlay-medium) 88%, transparent)",
            color: "var(--th-text-muted)",
          }}
        >
          {tr(`${AUDIT_NOTES.length}개`, `${AUDIT_NOTES.length} notes`)}
        </span>
      </summary>
      <div className="border-t px-4 py-4 sm:px-5" style={{ borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)" }}>
        <div className="grid gap-3 md:grid-cols-2">
          {AUDIT_NOTES.map((note) => (
            <AuditNoteCard key={note.id} note={note} isKo={isKo} />
          ))}
        </div>
      </div>
    </details>
  );
}
