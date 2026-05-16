const LEVEL_THRESHOLDS = [
  0, 100, 300, 600, 1000, 1600, 2500, 4000, 6000, 10000,
];
const LEVEL_TITLES_KO = [
  "신입",
  "수습",
  "사원",
  "주임",
  "대리",
  "과장",
  "차장",
  "부장",
  "이사",
  "사장",
];
const LEVEL_TITLES_EN = [
  "Newbie",
  "Trainee",
  "Staff",
  "Associate",
  "Sr. Associate",
  "Manager",
  "Asst. Dir.",
  "Director",
  "VP",
  "President",
];

export function getAgentLevel(xp: number) {
  let level = 1;
  for (let i = LEVEL_THRESHOLDS.length - 1; i >= 0; i--) {
    if (xp >= LEVEL_THRESHOLDS[i]) {
      level = i + 1;
      break;
    }
  }
  const nextThreshold =
    LEVEL_THRESHOLDS[Math.min(level, LEVEL_THRESHOLDS.length - 1)] ?? Infinity;
  const currentThreshold = LEVEL_THRESHOLDS[level - 1] ?? 0;
  const progress =
    nextThreshold === Infinity
      ? 1
      : (xp - currentThreshold) / (nextThreshold - currentThreshold);
  return {
    level,
    progress: Math.min(1, progress),
    nextThreshold,
    currentThreshold,
  };
}

export function getAgentTitle(xp: number, isKo: boolean) {
  const { level } = getAgentLevel(xp);
  const idx = Math.min(level - 1, LEVEL_TITLES_KO.length - 1);
  return isKo ? LEVEL_TITLES_KO[idx] : LEVEL_TITLES_EN[idx];
}
