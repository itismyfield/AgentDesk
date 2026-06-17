import subprocess
import json
import re
from datetime import datetime, timedelta, timezone

def run(cmd):
    result = subprocess.run(cmd, capture_output=True, text=True, shell=True)
    return result.stdout.strip(), result.returncode

def _detect_repo():
    # Repo-agnostic: derive the owner/name from the checked-out repo so this runs
    # against whatever fork/origin it lives in, not a hardcoded one.
    out, code = run("gh repo view --json nameWithOwner --jq .nameWithOwner")
    return out if code == 0 and out else "itismyfield/AgentDesk"

REPO = _detect_repo()

def parse_github_timestamp(value):
    if not value:
        return None
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except Exception:
        return None

def head_commit_timestamp(pr):
    head_oid = pr.get("headRefOid")
    if not head_oid:
        return None
    timestamp, code = run(
        f"gh api repos/{REPO}/commits/{head_oid} --jq .commit.committer.date"
    )
    if code != 0:
        return None
    return parse_github_timestamp(timestamp)

def _meaningful_field_value(value):
    normalized = value.strip().strip("-–—").strip()
    return bool(normalized) and normalized not in {"n/a", "none", "todo", "tbd", "na"}

def has_non_empty_body_field(body, labels):
    for label in labels:
        pattern = re.compile(
            rf"(?im)^[ \t]*(?:[-*][ \t]*)?(?:#{{1,6}}[ \t]*)?{re.escape(label)}(?:[ \t]*:[ \t]*(.*)|[ \t]*)$"
        )
        for match in pattern.finditer(body):
            if _meaningful_field_value(match.group(1) or ""):
                return True

            for next_line in body[match.end():].splitlines():
                stripped = next_line.strip()
                if not stripped:
                    continue
                if stripped.startswith("#"):
                    break
                if re.match(r"^(?:[-*]\s*)?[a-z0-9 /_-]+\s*:\s*$", stripped, re.I):
                    break
                if _meaningful_field_value(stripped):
                    return True
                break
    return False

print("Fetching PRs...")
prs_json, gh_code = run(f"gh pr list --repo {REPO} --state open --limit 50 --json number,title,headRefName,createdAt,headRefOid,body")

if gh_code != 0 or not prs_json:
    print("Warning: `gh` CLI not available or failed. Skipping PR analysis.")
    exit(0)

try:
    prs = json.loads(prs_json)
except Exception as e:
    print(f"Error parsing JSON: {e}")
    print(prs_json)
    exit(1)

inventory_refresh_count = 0
now = datetime.now(timezone.utc)

for pr in prs:
    num = pr['number']
    title = pr['title']
    body = str(pr.get('body') or '').lower()
    head_commit_at = head_commit_timestamp(pr)
    print(f"\n# {num} - {title}")

    # Check PR hygiene requirements
    if "workfingerprint" not in body:
        print("  [!] MISSING FINGERPRINT: PR body lacks the required 'WorkFingerprint' section.")
    if "duplicate" not in body and "overlap" not in body:
        print("  [!] MISSING OVERLAP CHECK: PR body fails to explicitly mention a 'duplicate' or 'overlap' check.")
    if "verification" not in body:
        print("  [!] MISSING VERIFICATION: PR body lacks the required 'verification' commands and results.")
    if "skipped checks" not in body:
        print("  [!] MISSING SKIPPED CHECKS: PR body fails to mention 'skipped checks' with reasons.")
    if not has_non_empty_body_field(body, ["risk", "risk assessment"]):
        print("  [!] MISSING RISK: PR body fails to mention 'risk' assessment.")
    if not has_non_empty_body_field(body, ["rollback notes", "rollback"]):
        print("  [!] MISSING ROLLBACK NOTES: PR body fails to mention 'rollback notes'.")

    # 2026-05-13 lesson: treat low-signal or stale broad branches as queue debt
    is_stale = head_commit_at is not None and (now - head_commit_at) > timedelta(days=14)

    # Get diff stat
    stat, _ = run(f"gh pr diff {num} --repo {REPO} --stat")
    print(f"Stat:\n{stat}")

    if is_stale:
        print(f"  [!] STALE BRANCH: Head commit is > 14 days old. Treat as queue debt. Close or recommend closing instead of salvaging in place.")

    # PR #214/#215 lesson: no-change PRs must have 0 changed files
    if "no-change" in title.lower():
        files_json, _ = run(f"gh pr view {num} --repo {REPO} --json files")
        try:
            files_data = json.loads(files_json)
            if files_data.get("files") is not None:
                if len(files_data["files"]) > 0:
                    print(f"  [!] UNSAFE NO-CHANGE PR: Title claims no-change but modifies {len(files_data['files'])} files.")
                else:
                    print(f"  [i] EMPTY NO-CHANGE PR: No changed files. If no durable queue-hygiene artifact is changed, it is a close candidate (report only).")
        except Exception:
            pass

    # PR #199/#200/#201 lesson: check for multiple inventory refreshes
    if "inventory" in title.lower() and "refresh" in title.lower():
        inventory_refresh_count += 1
        if "duplicate-pr guard" not in body and "duplicate pr guard" not in body:
            print("  [!] MISSING DUPLICATE PR GUARD: Inventory refresh PR body fails to mention 'duplicate-pr guard'.")

if inventory_refresh_count > 1:
    print("\n[!] WARNING: Multiple open inventory refresh PRs detected. Ensure strict duplicate-PR guard is followed.")
    exit(1)
