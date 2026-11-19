import type { RoutineRecord, RoutineStatus } from "../../api";
import type { TFunction } from "./model";

export type RoutineFilter = "all" | RoutineStatus;

export const ROUTINE_FILTERS: RoutineFilter[] = ["all", "enabled", "paused", "detached"];

export function parseTime(value: string | null | undefined): number | null {
  if (!value) return null;
  const time = Date.parse(value);
  return Number.isFinite(time) ? time : null;
}

function compareMaybeTime(left: number | null, right: number | null): number {
  if (left == null && right == null) return 0;
  if (left == null) return 1;
  if (right == null) return -1;
  return left - right;
}

export function sortRoutinesChronologically(
  routines: RoutineRecord[],
): RoutineRecord[] {
  return [...routines].sort((left, right) => {
    const dueCompare = compareMaybeTime(
      parseTime(left.next_due_at),
      parseTime(right.next_due_at),
    );
    if (dueCompare !== 0) return dueCompare;

    const lastRunCompare = compareMaybeTime(
      parseTime(right.last_run_at),
      parseTime(left.last_run_at),
    );
    if (lastRunCompare !== 0) return lastRunCompare;

    return left.name.localeCompare(right.name);
  });
}

function pad2(value: number): string {
  return value.toString().padStart(2, "0");
}

function parseNumberList(value: string, min: number, max: number): number[] | null {
  const numbers = value.split(",").map((part) => Number(part));
  if (
    numbers.length === 0 ||
    numbers.some((number) => !Number.isInteger(number) || number < min || number > max)
  ) {
    return null;
  }
  return numbers;
}

function parseNumberRange(value: string, min: number, max: number): [number, number] | null {
  const match = value.match(/^(\d+)-(\d+)$/);
  if (!match) return null;
  const start = Number(match[1]);
  const end = Number(match[2]);
  if (
    !Number.isInteger(start) ||
    !Number.isInteger(end) ||
    start < min ||
    end > max ||
    start > end
  ) {
    return null;
  }
  return [start, end];
}

function weekdayLabel(
  dayOfWeek: string,
  language: "ko" | "en" | "ja" | "zh",
): string | null {
  const weekday = dayOfWeek === "7" ? 0 : Number(dayOfWeek);
  if (!Number.isInteger(weekday) || weekday < 0 || weekday > 6) return null;
  const labels = {
    ko: ["일요일", "월요일", "화요일", "수요일", "목요일", "금요일", "토요일"],
    en: ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"],
    ja: ["日曜日", "月曜日", "火曜日", "水曜日", "木曜日", "金曜日", "土曜日"],
    zh: ["周日", "周一", "周二", "周三", "周四", "周五", "周六"],
  };
  return labels[language][weekday];
}

export function describeRoutineSchedule(
  schedule: string | null,
  language: "ko" | "en" | "ja" | "zh",
): string {
  const trimmed = schedule?.trim();
  if (!trimmed) {
    return language === "ko" ? "수동 실행" : "Manual run";
  }

  const every = trimmed.match(/^@every\s+(\d+)(ms|s|m|h|d)$/i);
  if (every) {
    const value = Number(every[1]);
    const unit = every[2].toLowerCase();
    const label =
      unit === "d"
        ? language === "ko"
          ? "일"
          : "d"
        : unit === "h"
          ? language === "ko"
            ? "시간"
            : "h"
          : unit === "m"
            ? language === "ko"
              ? "분"
              : "m"
            : unit;
    return language === "ko" ? `${value}${label}마다` : `Every ${value}${label}`;
  }

  const parts = trimmed.split(/\s+/);
  if (parts.length === 5) {
    const [minute, hour, dayOfMonth, month, dayOfWeek] = parts;
    const hourNum = Number(hour);
    const minuteNum = Number(minute);
    const hourStep = hour.match(/^\*\/(\d+)$/);
    const minuteList = parseNumberList(minute, 0, 59);
    const hourRange = parseNumberRange(hour, 0, 23);
    if (
      hourStep &&
      Number.isInteger(minuteNum) &&
      minuteNum >= 0 &&
      minuteNum <= 59 &&
      dayOfMonth === "*" &&
      month === "*" &&
      dayOfWeek === "*"
    ) {
      const value = Number(hourStep[1]);
      if (Number.isInteger(value) && value > 0 && value <= 23) {
        if (minuteNum === 0) {
          return language === "ko"
            ? `${value}시간마다 정각`
            : language === "ja"
              ? `${value}時間ごとの正時`
              : language === "zh"
                ? `每 ${value} 小时整点`
                : `Every ${value}h on the hour`;
        }
        const minuteLabel = pad2(minuteNum);
        return language === "ko"
          ? `${value}시간마다 ${minuteLabel}분`
          : language === "ja"
            ? `${value}時間ごと ${minuteLabel}分`
            : language === "zh"
              ? `每 ${value} 小时 ${minuteLabel} 分`
              : `Every ${value}h at :${minuteLabel}`;
      }
    }
    if (
      minuteList &&
      minuteList.length > 1 &&
      hourRange &&
      dayOfMonth === "*" &&
      month === "*" &&
      dayOfWeek === "*"
    ) {
      const [startHour, endHour] = hourRange;
      const sortedMinutes = [...minuteList].sort((left, right) => left - right);
      const interval =
        sortedMinutes.length === 2
          ? sortedMinutes[1] - sortedMinutes[0]
          : null;
      const startClock = `${pad2(startHour)}:${pad2(sortedMinutes[0])}`;
      const endClock = `${pad2(endHour)}:${pad2(sortedMinutes[sortedMinutes.length - 1])}`;
      const rangeLabel =
        language === "ko"
          ? `매일 ${startClock}~${endClock}`
          : language === "ja"
            ? `毎日 ${startClock}〜${endClock}`
            : language === "zh"
              ? `每天 ${startClock}~${endClock}`
              : `Daily ${startClock}-${endClock}`;
      if (interval && interval > 0) {
        return language === "ko"
          ? `${rangeLabel}, ${interval}분마다`
          : language === "ja"
            ? `${rangeLabel}、${interval}分ごと`
            : language === "zh"
              ? `${rangeLabel}，每 ${interval} 分钟`
              : `${rangeLabel}, every ${interval}m`;
      }
      return rangeLabel;
    }
    if (
      Number.isInteger(hourNum) &&
      Number.isInteger(minuteNum) &&
      hourNum >= 0 &&
      hourNum <= 23 &&
      minuteNum >= 0 &&
      minuteNum <= 59 &&
      dayOfMonth === "*" &&
      month === "*"
    ) {
      const clock = `${pad2(hourNum)}:${pad2(minuteNum)}`;
      if (dayOfWeek === "*") {
        return language === "ko" ? `매일 ${clock}` : `Daily ${clock}`;
      }
      if (dayOfWeek === "1-5") {
        return language === "ko" ? `평일 ${clock}` : `Weekdays ${clock}`;
      }
      const weekday = weekdayLabel(dayOfWeek, language);
      if (weekday) {
        return language === "ko"
          ? `매주 ${weekday} ${clock}`
          : language === "ja"
            ? `毎週${weekday} ${clock}`
            : language === "zh"
              ? `每${weekday} ${clock}`
              : `Every ${weekday} ${clock}`;
      }
    }
  }

  return trimmed;
}

export function statusTone(routine: RoutineRecord): "info" | "success" | "warn" | "neutral" | "danger" {
  if (routine.in_flight_run_id) return "info";
  if (routine.status === "enabled") return "success";
  if (routine.status === "paused") return "warn";
  if (routine.status === "detached") return "neutral";
  return "danger";
}

export function statusLabel(routine: RoutineRecord, t: TFunction): string {
  if (routine.in_flight_run_id) {
    return t({ ko: "진행 중", en: "Running", ja: "実行中", zh: "运行中" });
  }
  if (routine.status === "enabled") {
    return t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" });
  }
  if (routine.status === "paused") {
    return t({ ko: "일시정지", en: "Paused", ja: "一時停止", zh: "已暂停" });
  }
  if (routine.status === "detached") {
    return t({ ko: "분리됨", en: "Detached", ja: "切り離し", zh: "已分离" });
  }
  return routine.status;
}

export function runStatusTone(status: string | null | undefined): "info" | "success" | "warn" | "neutral" | "danger" {
  if (status === "succeeded") return "success";
  if (status === "running") return "info";
  if (status === "failed" || status === "interrupted") return "danger";
  if (status === "paused") return "warn";
  return "neutral";
}

export function runStatusLabel(status: string | null | undefined, t: TFunction): string | null {
  if (!status) return null;
  switch (status) {
    case "succeeded":
      return t({ ko: "최근 성공", en: "Last succeeded", ja: "前回成功", zh: "最近成功" });
    case "failed":
      return t({ ko: "최근 실패", en: "Last failed", ja: "前回失敗", zh: "最近失败" });
    case "skipped":
      return t({ ko: "최근 스킵", en: "Last skipped", ja: "前回スキップ", zh: "最近跳过" });
    case "running":
      return t({ ko: "최근 진행 중", en: "Last running", ja: "前回実行中", zh: "最近运行中" });
    case "paused":
      return t({ ko: "최근 일시정지", en: "Last paused", ja: "前回一時停止", zh: "最近暂停" });
    case "interrupted":
      return t({ ko: "최근 중단", en: "Last interrupted", ja: "前回中断", zh: "最近中断" });
    default:
      return t({
        ko: `최근 ${status}`,
        en: `Last ${status}`,
        ja: `前回${status}`,
        zh: `最近 ${status}`,
      });
  }
}

export function formatDateTime(value: string | null, localeTag: string): string {
  const time = parseTime(value);
  if (time == null) return "-";
  return new Date(time).toLocaleString(localeTag, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatRelative(value: string | number | null, localeTag: string, now = Date.now()): string | null {
  const time = typeof value === "number" ? value : parseTime(value);
  if (time == null) return null;
  const diffMs = time - now;
  const absMs = Math.abs(diffMs);
  const formatter = new Intl.RelativeTimeFormat(localeTag, { numeric: "auto" });
  if (absMs < 60_000) return formatter.format(Math.round(diffMs / 1_000), "second");
  if (absMs < 3_600_000) return formatter.format(Math.round(diffMs / 60_000), "minute");
  if (absMs < 86_400_000) return formatter.format(Math.round(diffMs / 3_600_000), "hour");
  return formatter.format(Math.round(diffMs / 86_400_000), "day");
}

export function filterLabel(filter: RoutineFilter, t: TFunction): string {
  switch (filter) {
    case "enabled":
      return t({ ko: "활성", en: "Active", ja: "有効", zh: "活跃" });
    case "paused":
      return t({ ko: "일시정지", en: "Paused", ja: "一時停止", zh: "已暂停" });
    case "detached":
      return t({ ko: "분리", en: "Detached", ja: "切り離し", zh: "分离" });
    case "all":
    default:
      return t({ ko: "전체", en: "All", ja: "すべて", zh: "全部" });
  }
}

function includesAny(value: string, terms: string[]): boolean {
  return terms.some((term) => value.includes(term));
}

function humanizeRoutineName(name: string): string {
  return name
    .replace(/\.(js|ts|sh)$/i, "")
    .split(/[-_/]+/)
    .filter(Boolean)
    .join(" ");
}

export function describeRoutinePurpose(routine: RoutineRecord, t: TFunction): string {
  const haystack = `${routine.name} ${routine.script_ref}`.toLowerCase();
  if (includesAny(haystack, ["family-morning-briefing"])) {
    return t({
      ko: "날씨, 캘린더, 리마인더를 모아 가족 아침 브리핑을 보냅니다.",
      en: "Sends a family morning briefing from weather, calendar, and reminders.",
      ja: "天気、カレンダー、リマインダーをまとめて朝のブリーフィングを送信します。",
      zh: "汇总天气、日历和提醒事项并发送家庭晨报。",
    });
  }
  if (includesAny(haystack, ["family-profile-probe"])) {
    return t({
      ko: "가족 프로필의 빈 정보를 찾아 DM 질문으로 보강합니다.",
      en: "Fills missing family profile details through DM prompts.",
      ja: "家族プロフィールの不足情報をDM質問で補完します。",
      zh: "通过 DM 提问补全家庭档案中的缺失信息。",
    });
  }
  if (includesAny(haystack, ["token-daily-report"])) {
    return t({
      ko: "토큰 사용량과 비용을 일일 리포트로 정리합니다.",
      en: "Builds a daily token usage and cost report.",
      ja: "トークン使用量とコストを日次レポートにまとめます。",
      zh: "生成每日 token 使用量和成本报告。",
    });
  }
  if (includesAny(haystack, ["memento-daily-report"])) {
    return t({
      ko: "Memento 기억 현황을 일일 리포트로 정리합니다.",
      en: "Summarizes the Memento memory store in a daily report.",
      ja: "Memento の記憶状況を日次レポートにまとめます。",
      zh: "将 Memento 记忆状况整理为每日报告。",
    });
  }
  if (includesAny(haystack, ["memento-hygiene"])) {
    return t({
      ko: "Memento 기억을 위생 정리하고 오래된 파편을 관리합니다.",
      en: "Maintains Memento memory hygiene and stale fragments.",
      ja: "Mementoメモリの衛生管理と古いフラグメント整理を行います。",
      zh: "维护 Memento 记忆卫生并管理过期片段。",
    });
  }
  if (includesAny(haystack, ["memento-scope-audit"])) {
    return t({
      ko: "Memento 파편의 스코프와 고정 상태를 주기적으로 감사합니다.",
      en: "Audits Memento fragment scope and anchoring on a schedule.",
      ja: "Mementoフラグメントのスコープと固定状態を定期監査します。",
      zh: "定期审计 Memento 片段的范围和固定状态。",
    });
  }
  if (includesAny(haystack, ["worktree", "janitor", "local-worktree-gc"])) {
    return t({
      ko: "오래된 로컬 worktree와 임시 빌드 캐시를 안전 기준에 맞춰 정리합니다.",
      en: "Cleans stale local worktrees and temporary build caches with safety gates.",
      ja: "古いローカルworktreeと一時ビルドキャッシュを安全条件付きで整理します。",
      zh: "按安全规则清理过期本地 worktree 和临时构建缓存。",
    });
  }
  if (includesAny(haystack, ["dependency-update"])) {
    return t({
      ko: "Agent CLI, 핵심 crate, CVE 등 외부 의존성 변경을 추적합니다.",
      en: "Tracks external dependency updates across Agent CLIs, core crates, and CVEs.",
      ja: "Agent CLI、主要crate、CVEなど外部依存関係の更新を追跡します。",
      zh: "跟踪 Agent CLI、核心 crate 和 CVE 等外部依赖更新。",
    });
  }
  if (includesAny(haystack, ["banchan-day-reminder-prep"])) {
    return t({
      ko: "반찬데이 전날 장보기와 준비 알림을 보냅니다.",
      en: "Sends grocery and prep reminders before banchan day.",
      ja: "惣菜デー前日の買い物と準備リマインダーを送信します。",
      zh: "在配菜日前发送采购和准备提醒。",
    });
  }
  if (includesAny(haystack, ["banchan-day-reminder-cook"])) {
    return t({
      ko: "반찬데이 당일 조리 알림을 보냅니다.",
      en: "Sends cooking reminders on banchan day.",
      ja: "惣菜デー当日の調理リマインダーを送信します。",
      zh: "在配菜日当天发送烹饪提醒。",
    });
  }
  if (includesAny(haystack, ["ai-integrated-briefing"])) {
    return t({
      ko: "AI 제품/릴리즈 뉴스를 통합 브리핑으로 정리합니다.",
      en: "Builds an integrated briefing of AI product and release news.",
      ja: "AI製品とリリースニュースを統合ブリーフィングにまとめます。",
      zh: "整理 AI 产品和发布新闻的综合简报。",
    });
  }
  if (includesAny(haystack, ["cookingheart-daily-briefing"])) {
    return t({
      ko: "CookingHeart 개발 현황과 다음 액션을 일일 브리핑합니다.",
      en: "Sends a daily CookingHeart development briefing and next actions.",
      ja: "CookingHeart開発状況と次のアクションを日次ブリーフィングします。",
      zh: "发送 CookingHeart 开发状态和下一步行动日报。",
    });
  }
  if (includesAny(haystack, ["queue-stability"])) {
    return t({
      ko: "큐 안정성 신호를 점검하고 필요한 복구 작업을 묶어 실행합니다.",
      en: "Checks queue stability signals and batches needed recovery work.",
      ja: "キュー安定性シグナルを確認し、必要な復旧作業をまとめます。",
      zh: "检查队列稳定性信号并批量执行必要恢复工作。",
    });
  }
  if (includesAny(haystack, ["memory-merge"])) {
    return t({
      ko: "에이전트 메모리 파일을 정리하고 장기 기억 계층으로 분배합니다.",
      en: "Cleans agent memory files and distributes them into long-term memory layers.",
      ja: "エージェントメモリファイルを整理し長期記憶層へ分配します。",
      zh: "整理智能体记忆文件并分配到长期记忆层。",
    });
  }
  if (includesAny(haystack, ["automation-candidate"])) {
    return t({
      ko: "자동화 후보를 감지, 추천, 또는 실행하는 모니터링 루틴입니다.",
      en: "Detects, recommends, or executes automation candidates.",
      ja: "自動化候補を検出、推薦、または実行する監視ルーチンです。",
      zh: "用于检测、推荐或执行自动化候选项的监控例程。",
    });
  }
  if (includesAny(haystack, ["working-watchdog"])) {
    return t({
      ko: "작업 중인 에이전트/세션 상태를 감시하고 이상 신호를 찾습니다.",
      en: "Watches active agent and session health for anomalies.",
      ja: "稼働中のエージェントとセッション状態を監視します。",
      zh: "监控运行中的智能体和会话健康状态。",
    });
  }

  const owner = routine.agent_id ?? routine.fallback_agent_id;
  const readableName = humanizeRoutineName(routine.name || routine.script_ref);
  return owner
    ? t({
        ko: `${readableName} 작업을 ${owner}가 정해진 시간에 실행합니다.`,
        en: `${readableName} is run by ${owner} on its schedule.`,
        ja: `${readableName} を ${owner} がスケジュールに沿って実行します。`,
        zh: `${readableName} 由 ${owner} 按计划执行。`,
      })
    : t({
        ko: `${readableName} 작업을 정해진 시간에 실행합니다.`,
        en: `${readableName} runs on its schedule.`,
        ja: `${readableName} をスケジュールに沿って実行します。`,
        zh: `${readableName} 按计划执行。`,
      });
}

export function previewValue(value: unknown, fallback = "-"): string {
  if (value === null || value === undefined || value === "") return fallback;
  const raw = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  if (!raw) return fallback;
  return raw.length > 520 ? `${raw.slice(0, 520)}...` : raw;
}

type RoutineRunResultFact = {
  key: string;
  value: string;
  mono?: boolean;
};

type RoutineRunResultNote = {
  key: string;
  value: string;
};

export interface RoutineRunResultSummary {
  summary: string | null;
  assistantPreview: string | null;
  facts: RoutineRunResultFact[];
  notes: RoutineRunResultNote[];
  rawPreview: string | null;
  structured: boolean;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function compactResultText(value: string, maxChars = 240): string {
  const trimmed = value.trim().replace(/\r/g, "");
  if (!trimmed) return "";
  const text = trimmed.replace(/\n{3,}/g, "\n\n");
  if (text.length <= maxChars) return text;
  return `${text.slice(0, maxChars)}...`;
}

function resultString(value: unknown, maxChars = 160): string | null {
  if (value === null || value === undefined || value === "") return null;
  if (typeof value === "string") return compactResultText(value, maxChars);
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  const raw = JSON.stringify(value);
  return raw ? compactResultText(raw, maxChars) : null;
}

function firstResultString(
  record: Record<string, unknown>,
  keys: string[],
  maxChars?: number,
): string | null {
  for (const key of keys) {
    const text = resultString(record[key], maxChars);
    if (text) return text;
  }
  return null;
}

function formatDurationMs(value: unknown): string | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  if (value < 1_000) return `${Math.round(value)}ms`;
  return `${(value / 1_000).toFixed(value < 10_000 ? 1 : 0)}s`;
}

export function summarizeRoutineRunResult(
  result: unknown,
): RoutineRunResultSummary {
  if (!isRecord(result)) {
    const text = resultString(result, 360);
    return {
      summary: text,
      assistantPreview: null,
      facts: [],
      notes: [],
      rawPreview: null,
      structured: Boolean(text),
    };
  }

  const facts: RoutineRunResultFact[] = [];
  const notes: RoutineRunResultNote[] = [];
  const addFact = (key: string, value: string | null, mono = false) => {
    if (value) facts.push({ key, value, mono });
  };
  const addNote = (key: string, value: string | null) => {
    if (value) notes.push({ key, value });
  };

  const summary = firstResultString(
    result,
    ["outcome_summary", "summary", "reason", "error"],
    280,
  );
  const assistantPreview = firstResultString(
    result,
    ["assistant_message_preview"],
    420,
  );

  addFact("status", firstResultString(result, ["status"], 80));
  addFact("agent", firstResultString(result, ["agent_id"], 80));
  addFact("failed_agent", firstResultString(result, ["failed_agent_id"], 80));
  addFact("attempt", firstResultString(result, ["attempt_kind"], 80));
  addFact("retry_count", firstResultString(result, ["retry_count"], 40));
  addFact("next_retry", firstResultString(result, ["next_retry_at"], 80));
  addFact("provider", firstResultString(result, ["provider"], 80));
  addFact("turn", firstResultString(result, ["turn_id"], 80), true);
  addFact("duration", formatDurationMs(result.duration_ms));
  addFact("evidence", firstResultString(result, ["completion_evidence"], 80));
  addFact("terminal", firstResultString(result, ["turn_terminal_status"], 80));
  addFact("mode", firstResultString(result, ["mode"], 80));
  addFact("agent_status", firstResultString(result, ["agentStatus", "agent_status"], 80));
  addFact("idle", firstResultString(result, ["isIdle", "is_idle"], 40));
  addFact("heartbeat", firstResultString(result, ["heartbeatCount", "heartbeat_count"], 40));
  addFact("checked", firstResultString(result, ["checkedAt", "checked_at"], 80));
  addFact("reviews", firstResultString(result, ["review_count"], 40));
  addFact("approved", firstResultString(result, ["approved_count"], 40));
  addFact("skipped", firstResultString(result, ["skipped"], 40));
  addFact("skipped_approved", firstResultString(result, ["skipped_approved"], 40));
  addFact("skipped_quality", firstResultString(result, ["skipped_quality_gate"], 40));
  addFact("observations", firstResultString(result, ["observation_count"], 40));
  addFact("active_candidates", firstResultString(result, ["active_candidate_count"], 40));
  addFact("recommendations_today", firstResultString(result, ["recommendations_today"], 40));
  addFact("reopt", firstResultString(result, ["reopt_count"], 40));
  addFact("target", firstResultString(result, ["targetKey", "target_key"], 80));
  addFact("summary_count", firstResultString(result, ["summaryCount", "summary_count"], 40));
  addFact(
    "fresh_context",
    firstResultString(result, ["fresh_context_guaranteed", "freshContextGuaranteed"], 40),
  );

  addNote("decision", firstResultString(result, ["decision_summary"], 260));
  addNote("evidence", firstResultString(result, ["top_evidence_summary"], 260));
  addNote("suppression", firstResultString(result, ["suppression_summary"], 260));
  addNote("scoring", firstResultString(result, ["scoring_summary"], 260));

  const structured = Boolean(summary || assistantPreview || facts.length || notes.length);
  return {
    summary,
    assistantPreview,
    facts,
    notes,
    rawPreview: previewValue(result, ""),
    structured,
  };
}

export function runResultFactLabel(key: string, t: TFunction): string {
  switch (key) {
    case "status":
      return t({ ko: "상태", en: "Status", ja: "状態", zh: "状态" });
    case "agent":
      return t({ ko: "agent", en: "Agent", ja: "agent", zh: "agent" });
    case "failed_agent":
      return t({ ko: "실패 agent", en: "Failed agent", ja: "失敗agent", zh: "失败 agent" });
    case "attempt":
      return t({ ko: "시도", en: "Attempt", ja: "試行", zh: "尝试" });
    case "retry_count":
      return t({ ko: "재시도", en: "Retries", ja: "リトライ", zh: "重试" });
    case "next_retry":
      return t({ ko: "다음 재시도", en: "Next retry", ja: "次回リトライ", zh: "下次重试" });
    case "provider":
      return t({ ko: "provider", en: "Provider", ja: "provider", zh: "provider" });
    case "turn":
      return t({ ko: "turn", en: "Turn", ja: "turn", zh: "turn" });
    case "duration":
      return t({ ko: "소요", en: "Duration", ja: "所要時間", zh: "耗时" });
    case "evidence":
      return t({ ko: "완료 근거", en: "Evidence", ja: "完了根拠", zh: "完成依据" });
    case "terminal":
      return t({ ko: "종료 상태", en: "Terminal", ja: "終了状態", zh: "终止状态" });
    case "mode":
      return t({ ko: "모드", en: "Mode", ja: "モード", zh: "模式" });
    case "agent_status":
      return t({ ko: "agent 상태", en: "Agent status", ja: "agent状態", zh: "agent 状态" });
    case "idle":
      return t({ ko: "idle", en: "Idle", ja: "idle", zh: "idle" });
    case "heartbeat":
      return t({ ko: "heartbeat", en: "Heartbeat", ja: "heartbeat", zh: "heartbeat" });
    case "checked":
      return t({ ko: "확인 시각", en: "Checked", ja: "確認時刻", zh: "检查时间" });
    case "reviews":
      return t({ ko: "검토", en: "Reviews", ja: "レビュー", zh: "审查" });
    case "approved":
      return t({ ko: "승인", en: "Approved", ja: "承認", zh: "批准" });
    case "skipped":
      return t({ ko: "스킵", en: "Skipped", ja: "スキップ", zh: "跳过" });
    case "skipped_approved":
      return t({ ko: "이미 승인", en: "Already approved", ja: "承認済み", zh: "已批准" });
    case "skipped_quality":
      return t({ ko: "품질 게이트", en: "Quality gate", ja: "品質ゲート", zh: "质量门" });
    case "observations":
      return t({ ko: "관찰", en: "Observations", ja: "観察", zh: "观测" });
    case "active_candidates":
      return t({ ko: "활성 후보", en: "Active candidates", ja: "有効候補", zh: "活跃候选" });
    case "recommendations_today":
      return t({ ko: "오늘 추천", en: "Recommendations today", ja: "今日の推薦", zh: "今日推荐" });
    case "reopt":
      return t({ ko: "재최적화", en: "Reopt", ja: "再最適化", zh: "重新优化" });
    case "target":
      return t({ ko: "대상", en: "Target", ja: "対象", zh: "目标" });
    case "summary_count":
      return t({ ko: "요약 횟수", en: "Summary count", ja: "要約回数", zh: "摘要次数" });
    case "fresh_context":
      return t({ ko: "fresh context", en: "Fresh context", ja: "fresh context", zh: "fresh context" });
    default:
      return key;
  }
}

export function runResultNoteLabel(key: string, t: TFunction): string {
  switch (key) {
    case "decision":
      return t({ ko: "판단", en: "Decision", ja: "判断", zh: "判断" });
    case "evidence":
      return t({ ko: "근거", en: "Evidence", ja: "根拠", zh: "依据" });
    case "suppression":
      return t({ ko: "억제", en: "Suppression", ja: "抑制", zh: "抑制" });
    case "scoring":
      return t({ ko: "스코어링", en: "Scoring", ja: "スコアリング", zh: "评分" });
    default:
      return key;
  }
}

export function strategyLabel(strategy: string, t: TFunction): string {
  if (strategy === "fresh") return t({ ko: "매번 새 세션", en: "New session each run", ja: "毎回新しいセッション", zh: "每次新会话" });
  if (strategy === "persistent") return t({ ko: "같은 세션 이어서", en: "Reuse one session", ja: "同じセッションを継続", zh: "沿用同一会话" });
  return strategy;
}

export function formatTimeout(seconds: number | null, t: TFunction): string {
  if (!seconds) return "-";
  if (seconds < 60) return t({ ko: `${seconds}초`, en: `${seconds}s`, ja: `${seconds}秒`, zh: `${seconds}秒` });
  const minutes = Math.round(seconds / 60);
  return t({ ko: `${minutes}분`, en: `${minutes} min`, ja: `${minutes}分`, zh: `${minutes}分钟` });
}

export type RoutineTone = "info" | "success" | "warn" | "neutral" | "danger";

/** Kit Badge tone for a dashboard routine tone. */
export function badgeTone(tone: RoutineTone): "info" | "success" | "warning" | "neutral" | "danger" {
  return tone === "warn" ? "warning" : tone;
}

export function isRoutineRunning(routine: RoutineRecord): boolean {
  return Boolean(routine.in_flight_run_id) || routine.last_run?.status === "running";
}

const HOUR = 3_600_000;
const DAY = 86_400_000;
/** The timeline shows four hours back and eight hours ahead of now. */
export const TIMELINE_PAST_MS = 4 * HOUR;
export const TIMELINE_FUTURE_MS = 8 * HOUR;
/** Touch target edge; markers in the same 44px column share one target. */
export const MARKER_TARGET_PX = 44;

export type RoutineMarkerState = "past" | "failed" | "running" | "future";

export interface RoutineMarker {
  routine: RoutineRecord;
  at: number;
  state: RoutineMarkerState;
  tone: RoutineTone;
  /** Position inside the window, 0 (start) to 1 (end). */
  ratio: number;
}

/**
 * Markers from server data only: the last run start (past, failed or running) and `next_due_at` for enabled
 * routines. No run times are derived from the cron expression.
 */
export function routineTimelineMarkers(routines: RoutineRecord[], now: number): RoutineMarker[] {
  const start = now - TIMELINE_PAST_MS;
  const end = now + TIMELINE_FUTURE_MS;
  const ratio = (at: number) => (at - start) / (end - start);
  const markers: RoutineMarker[] = [];
  for (const routine of routines) {
    const running = isRoutineRunning(routine);
    const runAt = parseTime(routine.last_run?.started_at) ?? parseTime(routine.last_run_at);
    if (running) {
      const at = Math.min(now, Math.max(start, runAt ?? now));
      markers.push({ routine, at, state: "running", tone: "info", ratio: ratio(at) });
    } else if (runAt != null && runAt >= start && runAt <= now) {
      const tone = runStatusTone(routine.last_run?.status);
      markers.push({ routine, at: runAt, state: tone === "danger" ? "failed" : "past", tone, ratio: ratio(runAt) });
    }
    const due = parseTime(routine.next_due_at);
    if (routine.status === "enabled" && due != null && due > now && due <= end) {
      markers.push({ routine, at: due, state: "future", tone: "neutral", ratio: ratio(due) });
    }
  }
  return markers.sort((left, right) => left.at - right.at || left.routine.name.localeCompare(right.routine.name));
}

export interface RoutineMarkerBucket {
  index: number;
  /** Centre of the 44px target in px. */
  center: number;
  markers: RoutineMarker[];
}

/** Groups markers into fixed 44px columns so every target is 44px wide and targets never overlap. */
export function bucketRoutineMarkers(markers: RoutineMarker[], width: number): RoutineMarkerBucket[] {
  const count = Math.max(1, Math.floor(width / MARKER_TARGET_PX));
  const buckets = new Map<number, RoutineMarker[]>();
  for (const marker of markers) {
    const index = Math.min(count - 1, Math.max(0, Math.floor((marker.ratio * width) / MARKER_TARGET_PX)));
    buckets.set(index, [...(buckets.get(index) ?? []), marker]);
  }
  return Array.from(buckets, ([index, group]) => ({ index, center: (index + 0.5) * MARKER_TARGET_PX, markers: group }));
}

function startOfDay(time: number): number {
  const date = new Date(time);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/** Today/tomorrow/yesterday, a short weekday within a week, otherwise month and day; then 24h HH:MM. */
export function formatScheduleAbsolute(time: number, now: number, localeTag: string, t: TFunction): string {
  const date = new Date(time);
  const days = Math.round((startOfDay(time) - startOfDay(now)) / DAY);
  const day =
    days === 0
      ? t({ ko: "오늘", en: "Today", ja: "今日", zh: "今天" })
      : days === 1
        ? t({ ko: "내일", en: "Tomorrow", ja: "明日", zh: "明天" })
        : days === -1
          ? t({ ko: "어제", en: "Yesterday", ja: "昨日", zh: "昨天" })
          : Math.abs(days) < 7
            ? new Intl.DateTimeFormat(localeTag, { weekday: "short" }).format(date)
            : new Intl.DateTimeFormat(localeTag, { month: "short", day: "numeric" }).format(date);
  return `${day} ${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

/** Hour ticks inside the window: every 2h, or every 4h on narrow charts. */
export function timelineHourTicks(now: number, width: number): number[] {
  const start = now - TIMELINE_PAST_MS;
  const end = now + TIMELINE_FUTURE_MS;
  const step = width < 420 ? 4 : 2;
  const first = new Date(start);
  first.setMinutes(0, 0, 0);
  const ticks: number[] = [];
  for (let time = first.getTime(); time < end; time += HOUR) {
    if (time > start && new Date(time).getHours() % step === 0) ticks.push(time);
  }
  return ticks;
}

export function formatClock(time: number): string {
  const date = new Date(time);
  return `${pad2(date.getHours())}:${pad2(date.getMinutes())}`;
}

