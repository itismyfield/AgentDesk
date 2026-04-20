import { BookOpen, MessagesSquare } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { summarizeMeetings } from "../app/meetingSummary";
import { useI18n } from "../i18n";
import type {
  RoundTableMeeting,
  RoundTableMeetingChannelOption,
} from "../types";
import MeetingMinutesView from "./MeetingMinutesView";
import SkillCatalogView from "./SkillCatalogView";
import {
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSegmentButton,
} from "./common/SurfacePrimitives";

type MeetingNotificationType = "info" | "success" | "warning" | "error";
type MeetingNotifier = (
  message: string,
  type?: MeetingNotificationType,
) => string | void;
type MeetingNotificationUpdater = (
  id: string,
  message: string,
  type?: MeetingNotificationType,
) => void;
type MobilePane = "meetings" | "skills";
const DESKTOP_SPLIT_QUERY = "(min-width: 1024px)";

interface MeetingsAndSkillsPageProps {
  meetings: RoundTableMeeting[];
  onRefresh: () => void;
  onNotify?: MeetingNotifier;
  onUpdateNotification?: MeetingNotificationUpdater;
  initialShowStartForm?: boolean;
  initialMeetingChannels?: RoundTableMeetingChannelOption[];
  initialChannelId?: string;
}

const PRESERVED_BUTTON_PATTERNS = [
  /details/i,
  /상세 보기/u,
  /preview issues to create/i,
  /생성될 일감 미리보기/u,
];

const HIDDEN_BUTTON_PATTERNS = [
  /new meeting/i,
  /새 회의/u,
  /close form/i,
  /입력 닫기/u,
  /^cancel$/i,
  /^취소$/u,
  /start meeting/i,
  /회의 시작/u,
  /create issues/i,
  /일감 생성/u,
  /issues created/i,
  /일감 생성 완료/u,
  /issues resolved/i,
  /일감 처리 완료/u,
  /retry failed/i,
  /실패분 재시도/u,
  /discard/i,
  /폐기/u,
];

const HIDDEN_SECTION_PATTERNS = [/start meeting/i, /회의 시작/u];
const DELETE_PATTERNS = [/delete/i, /삭제/u];

function normalizeNodeLabel(node: Element): string {
  const text = node.textContent ?? "";
  const title = node.getAttribute("title") ?? "";
  const ariaLabel = node.getAttribute("aria-label") ?? "";
  return `${text} ${title} ${ariaLabel}`.replace(/\s+/g, " ").trim();
}

function matchesAnyPattern(
  value: string,
  patterns: readonly RegExp[],
): boolean {
  return patterns.some((pattern) => pattern.test(value));
}

function hideElement(element: HTMLElement | null): void {
  if (!element || element.dataset.readOnlyHidden === "true") return;
  element.dataset.readOnlyHidden = "true";
  element.style.display = "none";
}

function pruneMeetingMutations(root: HTMLElement): void {
  root.querySelectorAll("section").forEach((section) => {
    const heading = section.querySelector("h3");
    if (!heading) return;
    const headingText = normalizeNodeLabel(heading);
    if (matchesAnyPattern(headingText, HIDDEN_SECTION_PATTERNS)) {
      hideElement(section as HTMLElement);
    }
  });

  root.querySelectorAll("button").forEach((button) => {
    const label = normalizeNodeLabel(button);
    if (matchesAnyPattern(label, PRESERVED_BUTTON_PATTERNS)) return;
    if (
      matchesAnyPattern(label, HIDDEN_BUTTON_PATTERNS) ||
      matchesAnyPattern(label, DELETE_PATTERNS)
    ) {
      hideElement(button as HTMLElement);
    }
  });

  root.querySelectorAll("select").forEach((select) => {
    hideElement(select.parentElement as HTMLElement | null);
  });

  root.querySelectorAll("input, textarea").forEach((field) => {
    const section = field.closest("section");
    if (!section) return;
    const heading = section.querySelector("h3");
    if (!heading) return;
    const headingText = normalizeNodeLabel(heading);
    if (matchesAnyPattern(headingText, HIDDEN_SECTION_PATTERNS)) {
      hideElement(section as HTMLElement);
    }
  });
}

function useDesktopSplitLayout(): boolean {
  const [isDesktopSplit, setIsDesktopSplit] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia(DESKTOP_SPLIT_QUERY).matches;
  });

  useEffect(() => {
    if (typeof window === "undefined") return;
    const mediaQuery = window.matchMedia(DESKTOP_SPLIT_QUERY);
    const handleChange = (event: MediaQueryListEvent) => {
      setIsDesktopSplit(event.matches);
    };

    setIsDesktopSplit(mediaQuery.matches);
    mediaQuery.addEventListener("change", handleChange);
    return () => mediaQuery.removeEventListener("change", handleChange);
  }, []);

  return isDesktopSplit;
}

export default function MeetingsAndSkillsPage({
  meetings,
  onRefresh,
  onNotify,
  onUpdateNotification,
  initialShowStartForm = false,
  initialMeetingChannels = [],
  initialChannelId,
}: MeetingsAndSkillsPageProps) {
  const { t } = useI18n();
  const [mobilePane, setMobilePane] = useState<MobilePane>("meetings");
  const isDesktopSplit = useDesktopSplitLayout();
  const meetingsRef = useRef<HTMLDivElement | null>(null);
  const meetingSummary = useMemo(() => summarizeMeetings(meetings), [meetings]);
  const completedCount = meetings.filter(
    (meeting) => meeting.status === "completed",
  ).length;

  useEffect(() => {
    const root = meetingsRef.current;
    if (!root) return;

    const applyReadOnly = () => pruneMeetingMutations(root);
    applyReadOnly();

    const observer = new MutationObserver(() => {
      applyReadOnly();
    });
    observer.observe(root, { childList: true, subtree: true });

    return () => observer.disconnect();
  }, [meetings]);

  const renderMeetingsPanel = () => (
    <div
      ref={meetingsRef}
      className="min-w-0"
      data-testid="meetings-page-timeline"
    >
      <MeetingMinutesView
        meetings={meetings}
        onRefresh={onRefresh}
        embedded
        onNotify={onNotify}
        onUpdateNotification={onUpdateNotification}
        initialShowStartForm={initialShowStartForm}
        initialMeetingChannels={initialMeetingChannels}
        initialChannelId={initialChannelId}
      />
    </div>
  );

  const renderSkillsPanel = () => (
    <div data-testid="meetings-page-skills" className="min-w-0">
      <SurfaceSection
        eyebrow={t({ ko: "Skills", en: "Skills" })}
        title={t({ ko: "스킬 카탈로그", en: "Skill Catalog" })}
        description={t({
          ko: "회의 타임라인 옆에서 최근에 축적된 자동화 스킬을 함께 확인합니다.",
          en: "Review the current automation skill catalog alongside the meeting timeline.",
        })}
        className="rounded-[28px] p-4 sm:p-5"
        style={{
          borderColor:
            "color-mix(in srgb, var(--th-accent-info) 20%, var(--th-border) 80%)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
        }}
      >
        <div className="meetings-and-skills__skills mt-4 min-w-0">
          <SkillCatalogView embedded />
        </div>
      </SurfaceSection>
    </div>
  );

  return (
    <div
      data-testid="meetings-page"
      className="mx-auto h-full w-full max-w-[1600px] min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <style>{`
        @media (min-width: 1024px) {
          .meetings-and-skills__skills > div:last-child {
            grid-template-columns: minmax(0, 1fr) !important;
          }
        }
      `}</style>

      <SurfaceSection
        eyebrow={t({ ko: "Meetings", en: "Meetings" })}
        title={t({ ko: "회의 + 스킬 허브", en: "Meetings + Skills Hub" })}
        description={t({
          ko: "회의 기록 타임라인과 스킬 카탈로그를 한 화면에 묶은 Phase 2 통합 보기입니다.",
          en: "Phase 2 combines the meeting timeline and skill catalog into a single workspace view.",
        })}
        badge={t({
          ko: `${meetings.length}개 회의`,
          en: `${meetings.length} meetings`,
        })}
        className="rounded-[28px] p-4 sm:p-5"
        style={{
          borderColor:
            "color-mix(in srgb, var(--th-accent-success) 18%, var(--th-border) 82%)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-success) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
        }}
      >
        <div className="mt-4 flex flex-wrap gap-3">
          <SurfaceMetricPill
            label={t({ ko: "전체 회의", en: "Total Meetings" })}
            value={t({
              ko: `${meetings.length}건`,
              en: `${meetings.length} records`,
            })}
            tone="info"
          />
          <SurfaceMetricPill
            label={t({ ko: "활성 회의", en: "Active" })}
            value={t({
              ko: `${meetingSummary.activeCount}건 진행 중`,
              en: `${meetingSummary.activeCount} in progress`,
            })}
            tone={meetingSummary.activeCount > 0 ? "accent" : "neutral"}
          />
          <SurfaceMetricPill
            label={t({ ko: "완료 회의", en: "Completed" })}
            value={t({
              ko: `${completedCount}건`,
              en: `${completedCount} records`,
            })}
            tone="success"
          />
          <SurfaceMetricPill
            label={t({ ko: "후속 이슈", en: "Open Follow-ups" })}
            value={t({
              ko: `${meetingSummary.unresolvedCount}건 미해결`,
              en: `${meetingSummary.unresolvedCount} unresolved`,
            })}
            tone={meetingSummary.unresolvedCount > 0 ? "warn" : "neutral"}
          />
        </div>

        <SurfaceNotice tone="info" className="mt-4">
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
            <div className="min-w-0">
              <div
                className="text-xs font-semibold uppercase tracking-[0.16em]"
                style={{ color: "var(--th-accent-info)" }}
              >
                {t({ ko: "Read Only", en: "Read Only" })}
              </div>
              <div className="mt-1 text-sm leading-6">
                {t({
                  ko: "이 화면은 회의 흐름과 스킬 문서를 함께 훑는 읽기 전용 허브입니다. 회의 상세 보기 드로어는 유지되지만 생성·폐기·삭제 같은 변경 액션은 숨깁니다.",
                  en: "This page is a read-only hub for reviewing meeting flow alongside skill docs. The meeting detail drawer stays available, while create, discard, and delete actions stay hidden.",
                })}
              </div>
            </div>
          </div>
        </SurfaceNotice>

        {!isDesktopSplit && (
          <div className="mt-4 flex gap-2">
          <SurfaceSegmentButton
            active={mobilePane === "meetings"}
            tone="success"
            onClick={() => setMobilePane("meetings")}
            className="flex-1 text-center"
          >
            <span className="inline-flex items-center gap-1.5">
              <MessagesSquare size={14} />
              {t({ ko: "회의", en: "Meetings" })}
            </span>
          </SurfaceSegmentButton>
          <SurfaceSegmentButton
            active={mobilePane === "skills"}
            tone="info"
            onClick={() => setMobilePane("skills")}
            className="flex-1 text-center"
          >
            <span className="inline-flex items-center gap-1.5">
              <BookOpen size={14} />
              {t({ ko: "스킬", en: "Skills" })}
            </span>
          </SurfaceSegmentButton>
          </div>
        )}
      </SurfaceSection>

      {isDesktopSplit ? (
        <div className="mt-5 grid min-w-0 gap-5 lg:grid-cols-[minmax(0,2fr)_minmax(320px,1fr)] lg:items-start">
          <div className="min-w-0">{renderMeetingsPanel()}</div>
          <div className="min-w-0">{renderSkillsPanel()}</div>
        </div>
      ) : (
        <div className="mt-5 space-y-5">
          {mobilePane === "meetings"
            ? renderMeetingsPanel()
            : renderSkillsPanel()}
        </div>
      )}
    </div>
  );
}
