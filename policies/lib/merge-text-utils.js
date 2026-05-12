/** @module policies/lib/merge-text-utils
 *
 * #1078: Extracted from merge-automation.js as part of the policy modularization pass.
 *
 * Pure string helpers used across the merge-automation surface (review
 * summaries, kv key sanitization, GitHub URL parsing, push/cherry-pick error
 * classification). No external runtime dependencies; can be unit-tested
 * independently.
 */

function sanitizeKvKeyPart(value) {
  return String(value || "").replace(/[^A-Za-z0-9._-]/g, "_");
}

function containsBlockingSeverity(text) {
  return /\bP[12]\b/i.test(text || "");
}

function compactWhitespace(text) {
  return String(text || "").replace(/\s+/g, " ").trim();
}

function summarizeInlineText(text) {
  var compact = compactWhitespace(text);
  if (compact.length <= 180) return compact;
  return compact.substring(0, 177) + "...";
}

function extractIssueNumberFromText(text) {
  var match = String(text || "").match(/#(\d+)/);
  return match ? parseInt(match[1], 10) : null;
}

function extractIssueNumberFromUrl(url) {
  var match = String(url || "").match(/\/issues\/(\d+)(?:[/?#]|$)/);
  return match ? parseInt(match[1], 10) : null;
}

function normalizeGitHubUrlOutput(text) {
  var lines = String(text || "").split(/\r?\n/);
  for (var i = 0; i < lines.length; i++) {
    var trimmed = lines[i].trim();
    if (/^https?:\/\//i.test(trimmed)) return trimmed;
  }
  var compact = compactWhitespace(text);
  return /^https?:\/\//i.test(compact) ? compact : "";
}

function parsePrNumberFromOutput(output) {
  var match = String(output || "").match(/\/pull\/(\d+)/);
  return match ? parseInt(match[1], 10) : null;
}

function isCherryPickConflict(errorText) {
  return /CONFLICT|could not apply|after resolving the conflicts|merge conflict/i.test(String(errorText || ""));
}

function isPushRejected(errorText) {
  return /rejected|fetch first|non-fast-forward|failed to push some refs/i.test(String(errorText || ""));
}

function firstPresent() {
  for (var i = 0; i < arguments.length; i++) {
    var value = arguments[i];
    if (value === null || value === undefined) continue;
    if (typeof value === "string" && value.trim() === "") continue;
    return value;
  }
  return null;
}

function parseJsonObject(raw) {
  if (!raw) return {};
  try {
    return JSON.parse(raw) || {};
  } catch (e) {
    return {};
  }
}

module.exports = {
  sanitizeKvKeyPart: sanitizeKvKeyPart,
  containsBlockingSeverity: containsBlockingSeverity,
  compactWhitespace: compactWhitespace,
  summarizeInlineText: summarizeInlineText,
  extractIssueNumberFromText: extractIssueNumberFromText,
  extractIssueNumberFromUrl: extractIssueNumberFromUrl,
  normalizeGitHubUrlOutput: normalizeGitHubUrlOutput,
  parsePrNumberFromOutput: parsePrNumberFromOutput,
  isCherryPickConflict: isCherryPickConflict,
  isPushRejected: isPushRejected,
  firstPresent: firstPresent,
  parseJsonObject: parseJsonObject
};
