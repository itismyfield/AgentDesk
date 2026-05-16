import type { CSSProperties } from "react";
import { Settings2 } from "lucide-react";
import type { I18nContextValue } from "../i18n";
import type { RoundTableMeetingExpertOption } from "../types";
import {
  MEETING_PROVIDERS,
  PROVIDER_LABELS,
} from "./meetingMinutesModel";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceNotice,
  SurfaceSection,
} from "./common/SurfacePrimitives";

interface MeetingStartFormProps {
  t: I18nContextValue["t"];
  agenda: string;
  channelId: string;
  showChannelEdit: boolean;
  primaryProvider: string;
  reviewerProvider: string;
  reviewerOptions: string[];
  expertQuery: string;
  filteredExperts: RoundTableMeetingExpertOption[];
  fixedParticipants: string[];
  startError: string | null;
  starting: boolean;
  inputStyle: CSSProperties;
  onAgendaChange: (value: string) => void;
  onChannelIdChange: (value: string) => void;
  onShowChannelEditChange: (value: boolean) => void;
  onPrimaryProviderChange: (value: string) => void;
  onReviewerProviderChange: (value: string) => void;
  onExpertQueryChange: (value: string) => void;
  onToggleFixedParticipant: (expert: RoundTableMeetingExpertOption) => void;
  onStartMeeting: () => void;
  onCancel: () => void;
}

export default function MeetingStartForm({
  t,
  agenda,
  channelId,
  showChannelEdit,
  primaryProvider,
  reviewerProvider,
  reviewerOptions,
  expertQuery,
  filteredExperts,
  fixedParticipants,
  startError,
  starting,
  inputStyle,
  onAgendaChange,
  onChannelIdChange,
  onShowChannelEditChange,
  onPrimaryProviderChange,
  onReviewerProviderChange,
  onExpertQueryChange,
  onToggleFixedParticipant,
  onStartMeeting,
  onCancel,
}: MeetingStartFormProps) {
  return (
    <SurfaceSection
      eyebrow={t({ ko: "Compose", en: "Compose" })}
      title={t({ ko: "회의 시작", en: "Start Meeting" })}
      description={t({
        ko: "회의 채널, 안건, 진행 모델을 정하면 반대 모델 교차검증이 자동으로 따라옵니다.",
        en: "Set the channel, agenda, and primary model. Counter-model cross-review follows automatically.",
      })}
      actions={
        <SurfaceActionButton tone="neutral" onClick={onCancel}>
          {t({ ko: "취소", en: "Cancel" })}
        </SurfaceActionButton>
      }
    >
      <div className="mt-4 space-y-3">
        <SurfaceCard
          className="rounded-2xl p-4"
          style={{
            background:
              "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
            borderColor:
              "color-mix(in srgb, var(--th-border) 72%, transparent)",
          }}
        >
          <div className="flex flex-col gap-1 sm:flex-row sm:items-center sm:gap-3">
            <label
              className="shrink-0 text-xs font-semibold uppercase tracking-widest sm:w-24"
              style={{ color: "var(--th-text-muted)" }}
            >
              {t({ ko: "채널 ID", en: "Channel ID" })}
            </label>
            {showChannelEdit || !channelId ? (
              <input
                type="text"
                value={channelId}
                onChange={(event) => onChannelIdChange(event.target.value)}
                placeholder={t({
                  ko: "Discord 채널 ID",
                  en: "Discord Channel ID",
                })}
                className="flex-1 rounded-lg px-3 py-1.5 font-mono text-xs"
                style={inputStyle}
                onBlur={() => {
                  if (channelId) onShowChannelEditChange(false);
                }}
                autoFocus
              />
            ) : (
              <div className="flex flex-1 items-center gap-2">
                <span
                  className="font-mono text-xs"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {channelId}
                </span>
                <SurfaceActionButton
                  onClick={() => onShowChannelEditChange(true)}
                  tone="neutral"
                  compact
                  title={t({ ko: "채널 ID 변경", en: "Change Channel ID" })}
                  aria-label={t({
                    ko: "채널 ID 변경",
                    en: "Change channel ID",
                  })}
                >
                  <Settings2
                    size={12}
                    style={{ color: "var(--th-text-muted)" }}
                  />
                </SurfaceActionButton>
              </div>
            )}
          </div>
        </SurfaceCard>

        <SurfaceCard
          className="rounded-2xl p-4"
          style={{
            background:
              "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
            borderColor:
              "color-mix(in srgb, var(--th-border) 72%, transparent)",
          }}
        >
          <div className="flex flex-col gap-1 sm:flex-row sm:items-start sm:gap-3">
            <label
              className="shrink-0 text-xs font-semibold uppercase tracking-widest sm:w-24 sm:pt-2"
              style={{ color: "var(--th-text-muted)" }}
            >
              {t({ ko: "안건", en: "Agenda" })}
            </label>
            <textarea
              value={agenda}
              onChange={(event) => onAgendaChange(event.target.value)}
              placeholder={t({
                ko: "회의 안건을 입력하세요",
                en: "Enter meeting agenda",
              })}
              rows={3}
              className="flex-1 resize-y rounded-lg px-3 py-2 text-sm"
              style={inputStyle}
              onKeyDown={(event) => {
                if (
                  event.key === "Enter" &&
                  (event.metaKey || event.ctrlKey) &&
                  !event.nativeEvent.isComposing
                ) {
                  event.preventDefault();
                  onStartMeeting();
                }
              }}
            />
          </div>
          <div
            className="mt-2 text-xs"
            style={{ color: "var(--th-text-muted)" }}
          >
            {t({ ko: "시작: Ctrl/⌘ + Enter", en: "Start: Ctrl/⌘ + Enter" })}
          </div>
        </SurfaceCard>

        <SurfaceCard
          className="rounded-2xl p-4"
          style={{
            background:
              "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
            borderColor:
              "color-mix(in srgb, var(--th-border) 72%, transparent)",
          }}
        >
          <div className="flex flex-col gap-4">
            <div className="grid gap-3 lg:grid-cols-2">
              <div className="flex flex-col gap-1">
                <label
                  className="text-xs font-semibold uppercase tracking-widest"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {t({ ko: "진행 프로바이더", en: "Primary Provider" })}
                </label>
                <select
                  value={primaryProvider}
                  onChange={(event) =>
                    onPrimaryProviderChange(event.target.value)
                  }
                  className="rounded-lg px-3 py-2 text-xs"
                  style={inputStyle}
                >
                  {MEETING_PROVIDERS.map((provider) => (
                    <option key={provider} value={provider}>
                      {PROVIDER_LABELS[provider] ?? provider.toUpperCase()}
                    </option>
                  ))}
                </select>
              </div>
              <div className="flex flex-col gap-1">
                <label
                  className="text-xs font-semibold uppercase tracking-widest"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {t({ ko: "리뷰 프로바이더", en: "Reviewer Provider" })}
                </label>
                <select
                  value={reviewerProvider}
                  onChange={(event) =>
                    onReviewerProviderChange(event.target.value)
                  }
                  className="rounded-lg px-3 py-2 text-xs"
                  style={inputStyle}
                >
                  {reviewerOptions.map((provider) => (
                    <option key={provider} value={provider}>
                      {PROVIDER_LABELS[provider] ?? provider.toUpperCase()}
                    </option>
                  ))}
                </select>
              </div>
            </div>
            <SurfaceNotice tone="info" compact>
              {t({
                ko: "반대 모델이 자동 교차검증",
                en: "Counter model auto cross-review",
              })}
            </SurfaceNotice>
            <div
              className="flex flex-col gap-3 rounded-2xl border p-3"
              style={{
                borderColor:
                  "color-mix(in srgb, var(--th-border) 72%, transparent)",
                background:
                  "color-mix(in srgb, var(--th-bg-surface) 70%, transparent)",
              }}
            >
              <div className="flex flex-col gap-1">
                <label
                  className="text-xs font-semibold uppercase tracking-widest"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {t({
                    ko: "고정 전문 에이전트",
                    en: "Fixed Expert Agents",
                  })}
                </label>
                <input
                  type="text"
                  value={expertQuery}
                  onChange={(event) => onExpertQueryChange(event.target.value)}
                  placeholder={t({
                    ko: "전문 에이전트 검색 후 여러 명 선택",
                    en: "Search experts and select multiple",
                  })}
                  className="rounded-lg px-3 py-2 text-xs"
                  style={inputStyle}
                />
              </div>
              <div className="flex flex-wrap gap-2">
                {filteredExperts.map((expert) => {
                  const selected = fixedParticipants.includes(expert.role_id);
                  return (
                    <button
                      key={expert.role_id}
                      type="button"
                      onClick={() => onToggleFixedParticipant(expert)}
                      className="min-h-11 rounded-2xl border px-3 py-2.5 text-left text-sm transition-colors"
                      style={{
                        borderColor: selected
                          ? "color-mix(in srgb, var(--th-accent-primary) 40%, var(--th-border) 60%)"
                          : "color-mix(in srgb, var(--th-border) 72%, transparent)",
                        background: selected
                          ? "color-mix(in srgb, var(--th-accent-primary-soft) 72%, transparent)"
                          : "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                        color: "var(--th-text)",
                      }}
                    >
                      <span className="font-semibold">
                        {expert.display_name}
                      </span>
                      <span
                        className="ml-1.5"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        #{expert.role_id}
                      </span>
                    </button>
                  );
                })}
                {filteredExperts.length === 0 && (
                  <span
                    className="text-xs"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {t({
                      ko: "선택 가능한 전문 에이전트가 없습니다",
                      en: "No available experts",
                    })}
                  </span>
                )}
              </div>
            </div>
          </div>
        </SurfaceCard>

        {startError && (
          <SurfaceNotice tone="danger" compact>
            {startError}
          </SurfaceNotice>
        )}

        <div className="flex items-center justify-end gap-2">
          <SurfaceActionButton tone="neutral" onClick={onCancel}>
            {t({ ko: "취소", en: "Cancel" })}
          </SurfaceActionButton>
          <SurfaceActionButton
            tone="accent"
            onClick={onStartMeeting}
            disabled={starting || !agenda.trim() || !channelId.trim()}
          >
            {starting
              ? t({ ko: "시작 중...", en: "Starting..." })
              : t({ ko: "회의 시작", en: "Start Meeting" })}
          </SurfaceActionButton>
        </div>
      </div>
    </SurfaceSection>
  );
}
