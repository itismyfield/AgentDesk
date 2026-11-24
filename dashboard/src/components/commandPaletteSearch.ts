import type { CommandMenuCommand, CommandMenuGroup } from "../kit/pro/command-menu";

// Same fold as the kit: no case or accents, Hangul split into jamo so a half-typed syllable still matches.
const foldJamo = (text: string) => text.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();
// Syllable-level fold for loose matching, so "설정" does not match scattered jamo.
const foldSyllables = (text: string) => foldJamo(text).normalize("NFC");

function keywordList(command: CommandMenuCommand): string[] {
  if (Array.isArray(command.keywords)) return command.keywords;
  return command.keywords ? [command.keywords] : [];
}

/** Length of the tightest run of `text` holding `query` in order, or 0 when it is not a subsequence. */
function tightestSpan(text: string, query: string): number {
  let best = 0;
  for (let start = text.indexOf(query[0]); start >= 0; start = text.indexOf(query[0], start + 1)) {
    let at = start;
    for (const char of query.slice(1)) {
      at = text.indexOf(char, at + 1);
      if (at < 0) return best;
    }
    if (!best || at - start + 1 < best) best = at - start + 1;
  }
  return best;
}

/** Old-palette score: a prefix of one value beats a substring, which beats an in-order abbreviation ("sts" → "/stats"). 0 = no match. */
export function commandMatchScore(command: CommandMenuCommand, query: string): number {
  const values = [command.label, ...keywordList(command)].filter(Boolean);
  const q = foldJamo(query.trim());
  if (!q) return 1;
  if (values.some((value) => foldJamo(value).startsWith(q))) return 3;
  if (foldJamo(values.join(" ")).includes(q)) return 2;
  const loose = foldSyllables(query.trim());
  const span = tightestSpan(foldSyllables(values.join(" ")), loose);
  return span ? 1 + loose.length / span / 2 : 0;
}

/**
 * Filters and sorts like the old palette: best rows first, groups by their best row. The kit then filters by plain
 * substring, so each kept row also carries the query as a keyword.
 */
export function filterCommandGroups(groups: CommandMenuGroup[], query: string): CommandMenuGroup[] {
  const q = query.trim();
  if (!q) return groups;
  return groups
    .map((group) => {
      const scored = group.commands
        .map((command) => ({ command, score: commandMatchScore(command, q) }))
        .filter((row) => row.score > 0)
        .sort((a, b) => b.score - a.score);
      return {
        group: { ...group, commands: scored.map(({ command }) => ({ ...command, keywords: [...keywordList(command), q] })) },
        best: scored[0]?.score ?? 0,
      };
    })
    .filter((entry) => entry.best > 0)
    .sort((a, b) => b.best - a.best)
    .map((entry) => entry.group);
}
