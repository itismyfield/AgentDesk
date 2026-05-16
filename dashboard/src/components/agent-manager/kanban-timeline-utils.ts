import type { GitHubComment } from "../../api";

// ---------------------------------------------------------------------------
// GitHub Comment Timeline Parser
// ---------------------------------------------------------------------------

export type GitHubTimelineKind = "review" | "pm" | "work" | "general";
export type GitHubTimelineStatus =
  | "reviewing"
  | "changes_requested"
  | "passed"
  | "decision"
  | "completed"
  | "comment";

export interface ParsedGitHubComment {
  kind: GitHubTimelineKind;
  status: GitHubTimelineStatus;
  title: string;
  summary: string | null;
  details: string[];
  createdAt: string;
  author: string;
  body: string;
}

export interface CoalescedGitHubTimelineItem {
  id: string;
  kind: GitHubTimelineKind;
  status: GitHubTimelineStatus;
  author: string;
  createdAt: string;
  updatedAt: string;
  entries: ParsedGitHubComment[];
  coalesced: boolean;
  highlights: string[];
}

const TIMELINE_COALESCE_WINDOW_MS = 2 * 60_000;

const TIMELINE_COALESCE_INCLUDE_PATTERNS = [
  /상태.*(변경|전환|업데이트)/u,
  /(변경|전환|업데이트).*(상태|메타데이터|우선순위|라벨|체크리스트|태그)/u,
  /메타데이터.*(변경|수정|업데이트)/u,
  /우선순위.*(변경|수정|업데이트)/u,
  /라벨.*(변경|수정|업데이트)/u,
  /체크리스트.*(변경|수정|업데이트)/u,
  /\bstatus\b.*(changed|updated|set)/i,
  /\bmetadata\b.*(changed|updated|set)/i,
  /\bpriority\b.*(changed|updated|set)/i,
  /\blabels?\b.*(changed|updated|set)/i,
  /\bchecklist\b.*(changed|updated|set)/i,
];

const TIMELINE_COALESCE_EXCLUDE_PATTERNS = [
  /에이전트.*할당/u,
  /담당자.*변경/u,
  /담당 에이전트/u,
  /할당 변경/u,
  /assignee/i,
  /assigned agent/i,
  /assignment/i,
  /리뷰 결정/u,
  /review decision/i,
  /pm 결정/u,
  /pm decision/i,
  /verdict/i,
];

function cleanMarkdownLine(line: string): string {
  return line
    .replace(/^#+(?:\s+|$)/, "")
    .replace(/^\d+\.\s+/, "")
    .replace(/^[-*]\s+/, "")
    .replace(/\*\*/g, "")
    .replace(/`/g, "")
    .trim();
}

function firstMeaningfulLine(body: string): string | null {
  for (const raw of body.split("\n")) {
    const line = cleanMarkdownLine(raw);
    if (!line) continue;
    if (line === "---") continue;
    return line;
  }
  return null;
}

function extractListHighlights(body: string, limit = 3): string[] {
  const results: string[] = [];
  for (const raw of body.split("\n")) {
    if (!/^\s*(\d+\.\s+|[-*]\s+)/.test(raw)) continue;
    const line = cleanMarkdownLine(raw);
    if (!line) continue;
    results.push(line);
    if (results.length >= limit) break;
  }
  return results;
}

function extractSectionHeadings(body: string, limit = 3): string[] {
  const results: string[] = [];
  for (const raw of body.split("\n")) {
    const match = raw.match(/^#{2,6}\s+(.+)$/);
    if (!match) continue;
    const line = cleanMarkdownLine(match[1]);
    if (!line || line.includes("완료 보고")) continue;
    results.push(line);
    if (results.length >= limit) break;
  }
  return results;
}

function filterCommentBody(body: string): string {
  const lines: string[] = [];
  let inCodeBlock = false;
  for (const raw of body.split("\n")) {
    const trimmed = raw.trim();
    if (trimmed.startsWith("```")) {
      inCodeBlock = !inCodeBlock;
      continue;
    }
    if (inCodeBlock || trimmed.startsWith(">")) continue;
    lines.push(raw);
  }
  return lines.join("\n");
}

function getHeading(filteredBody: string): string | null {
  for (const raw of filteredBody.split("\n")) {
    const match = raw.match(/^##\s+(.+)$/);
    if (match) return cleanMarkdownLine(match[1]);
  }
  return null;
}

function getMeaningfulLines(body: string): string[] {
  return body
    .split("\n")
    .map((raw) => cleanMarkdownLine(raw))
    .filter((line) => line && line !== "---");
}

const REVIEW_PASS_PATTERNS = [
  "추가 blocking finding은 없습니다",
  "현재 diff 기준으로 머지를 막을 추가 결함은 확인하지 못했습니다",
  "머지를 막을 추가 결함은 확인하지 못했습니다",
  "추가 결함은 확인하지 못했습니다",
];

const REVIEW_BLOCKING_PATTERNS = [
  /blocking finding\s*\d+건/i,
  /blocking \d+건/i,
  /확인된 이슈 \d+건/,
  /결함 \d+건/,
  /문제 \d+건/,
  /남아 있습니다/,
];

const REVIEW_FEEDBACK_PREFIXES = [
  "리뷰했습니다",
  "추가 리뷰했습니다",
  "추가 구현분 리뷰했습니다",
  "재확인했습니다",
  "재검토했습니다",
  "코드 리뷰 결과",
  "코드 검토 결과",
  "리뷰 결과",
  "검토 결과",
];

/** Broader review-signal keywords — used for secondary heuristic detection */
const REVIEW_SIGNAL_KEYWORDS = [
  /리뷰[를을]?\s*(완료|진행|확인)/,
  /검토[를을]?\s*(완료|진행|확인)/,
  /코드\s*리뷰/,
  /code\s*review/i,
  /reviewed?\b/i,
];

const PM_MARKER_PATTERNS = [
  /^PM 결정(?:[:\s]|$)/u,
  /^PM 판단(?:[:\s]|$)/u,
  /^PMD 결정(?:[:\s]|$)/u,
  /^PMD 판단(?:[:\s]|$)/u,
  /^프로듀서 결정(?:[:\s]|$)/u,
  /^프로듀서 판단(?:[:\s]|$)/u,
  /^PM Decision\b/i,
  /^PM Verdict\b/i,
];

const WORK_HEADING_PATTERNS = [
  /완료 보고/u,
  /^#\d+\s+작업 완료(?:[:\s]|$)/u,
  /^작업 완료(?:[:\s]|$)/u,
];

function matchesAny(text: string, patterns: RegExp[]): boolean {
  return patterns.some((pattern) => pattern.test(text));
}

function timelineCoalescingText(entry: ParsedGitHubComment): string {
  return [entry.title, entry.summary ?? "", entry.body].join("\n");
}

function isTimelineCoalesceEligible(entry: ParsedGitHubComment): boolean {
  if (entry.kind !== "general") return false;
  const text = timelineCoalescingText(entry);
  if (matchesAny(text, TIMELINE_COALESCE_EXCLUDE_PATTERNS)) return false;
  return matchesAny(text, TIMELINE_COALESCE_INCLUDE_PATTERNS);
}

function buildCoalescedTimelineItem(
  entries: ParsedGitHubComment[],
  index: number,
): CoalescedGitHubTimelineItem {
  const first = entries[0];
  const last = entries[entries.length - 1];
  const highlights = Array.from(
    new Set(
      entries
        .flatMap((entry) => [entry.title, entry.summary ?? ""])
        .map((value) => cleanMarkdownLine(value))
        .filter(Boolean),
    ),
  ).slice(0, 3);

  return {
    id: `${first.author}-${first.createdAt}-${index}`,
    kind: first.kind,
    status: first.status,
    author: first.author,
    createdAt: first.createdAt,
    updatedAt: last.createdAt,
    entries,
    coalesced: entries.length > 1,
    highlights,
  };
}

export function parseGitHubCommentTimeline(comments: GitHubComment[]): ParsedGitHubComment[] {
  return comments.flatMap<ParsedGitHubComment>((comment) => {
    const body = comment.body.trim();
    if (!body) return [];

    const filteredBody = filterCommentBody(body);
    const classificationBody = filteredBody.trim() || body;
    const meaningfulLines = getMeaningfulLines(classificationBody);
    const firstLine = meaningfulLines[0] ?? firstMeaningfulLine(body);
    const heading = getHeading(classificationBody);
    const leadText = meaningfulLines.slice(0, 3).join(" ");
    const author = comment.author?.login ?? "unknown";

    if (body.startsWith("🔍 칸반 상태:") || firstLine?.startsWith("🔍 칸반 상태:")) {
      return [{
        kind: "review",
        status: "reviewing",
        title: "리뷰 진행",
        summary: cleanMarkdownLine(firstLine ?? body),
        details: [],
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    // Pre-compute work heading to guard secondary review heuristic
    const workMarker = heading ?? firstLine ?? "";
    const isWorkHeading = heading ? matchesAny(heading, WORK_HEADING_PATTERNS) : false;
    const isWorkLead =
      (!heading && (workMarker.includes("완료 보고") || /^#\d+\s+작업 완료(?:[:\s]|$)/u.test(workMarker)))
      || workMarker.startsWith("구현 완료")
      || workMarker.startsWith("수정 완료")
      || workMarker.startsWith("배포 완료");
    const isWorkComment = isWorkHeading || isWorkLead;

    const passed = REVIEW_PASS_PATTERNS.some((pattern) => leadText.includes(pattern))
      && !matchesAny(leadText, REVIEW_BLOCKING_PATTERNS);
    const reviewFeedbackExplicit =
      REVIEW_FEEDBACK_PREFIXES.some((prefix) => leadText.startsWith(prefix))
      || leadText.includes("재검토 결과")
      || /blocking finding/i.test(leadText)
      || /blocking \d+건/i.test(leadText)
      || /확인된 이슈 \d+건/.test(leadText)
      || /결함 \d+건/.test(leadText)
      || /문제 \d+건/.test(leadText);
    // Secondary heuristic: review keyword + numbered code-reference list
    // Guard: skip if heading matches work patterns (e.g. "완료 보고" with code refs)
    const hasReviewSignal = !isWorkComment && matchesAny(leadText, REVIEW_SIGNAL_KEYWORDS);
    const hasNumberedFindings = /^\s*\d+\.\s+/m.test(classificationBody)
      && /`[^`]+\.\w+[:`]/.test(classificationBody); // file reference like `foo.rs:123`
    const reviewFeedback = reviewFeedbackExplicit
      || (hasReviewSignal && hasNumberedFindings)
      || (hasReviewSignal && matchesAny(leadText, REVIEW_BLOCKING_PATTERNS));

    if (passed || reviewFeedback) {
      const highlights = extractListHighlights(body, passed ? 1 : 3);
      return [{
        kind: "review",
        status: passed ? "passed" : "changes_requested",
        title: passed ? "리뷰 통과" : "리뷰 피드백",
        summary: highlights[0] ?? cleanMarkdownLine(firstLine ?? "리뷰 결과"),
        details: passed ? [] : highlights.slice(1),
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    const pmMarker = heading ?? firstLine ?? "";
    if (matchesAny(pmMarker, PM_MARKER_PATTERNS)) {
      return [{
        kind: "pm",
        status: "decision",
        title: heading ?? "PM 결정",
        summary: cleanMarkdownLine(firstLine ?? "PM 결정"),
        details: extractListHighlights(body, 3),
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    if (isWorkComment) {
      return [{
        kind: "work",
        status: "completed",
        title: heading ?? "작업 완료",
        summary: cleanMarkdownLine(firstLine ?? "작업 완료"),
        details: extractSectionHeadings(body, 3),
        createdAt: comment.createdAt,
        author,
        body,
      }];
    }

    // Fallback: unrecognized comments shown as "general" type
    const truncated = body.length > 200 ? body.slice(0, 200) + "…" : body;
    return [{
      kind: "general",
      status: "comment",
      title: heading ?? cleanMarkdownLine(firstLine ?? "코멘트"),
      summary: truncated,
      details: [],
      createdAt: comment.createdAt,
      author,
      body,
    }];
  });
}

export function coalesceGitHubCommentTimeline(
  entries: ParsedGitHubComment[],
): CoalescedGitHubTimelineItem[] {
  const groups: ParsedGitHubComment[][] = [];
  let currentGroup: ParsedGitHubComment[] = [];
  let groupStartMs = Number.NaN;

  const flushGroup = () => {
    if (currentGroup.length === 0) return;
    groups.push(currentGroup);
    currentGroup = [];
    groupStartMs = Number.NaN;
  };

  for (const entry of entries) {
    const entryTs = Date.parse(entry.createdAt);
    const eligible = Number.isFinite(entryTs) && isTimelineCoalesceEligible(entry);

    if (!eligible) {
      flushGroup();
      groups.push([entry]);
      continue;
    }

    if (currentGroup.length === 0) {
      currentGroup = [entry];
      groupStartMs = entryTs;
      continue;
    }

    const sameAuthor = currentGroup[0]?.author === entry.author;
    const withinWindow = entryTs - groupStartMs <= TIMELINE_COALESCE_WINDOW_MS;

    if (sameAuthor && withinWindow) {
      currentGroup.push(entry);
      continue;
    }

    flushGroup();
    currentGroup = [entry];
    groupStartMs = entryTs;
  }

  flushGroup();

  return groups.map((group, index) => buildCoalescedTimelineItem(group, index));
}
