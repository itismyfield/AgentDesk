import subprocess
import json
import re
import sys
from datetime import datetime, timedelta, timezone

def run(cmd):
    result = subprocess.run(cmd, capture_output=True, text=True, shell=True)
    return result.stdout.strip(), result.returncode

def _detect_repo():
    # Repo-agnostic: derive the owner/name from the checked-out repo so this runs
    # against whatever fork/origin it lives in, not a hardcoded one.
    out, code = run("gh repo view --json nameWithOwner --jq .nameWithOwner")
    return out if code == 0 and out else "itismyfield/AgentDesk"

def parse_github_timestamp(value):
    if not value:
        return None
    try:
        return datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except Exception:
        return None

def head_commit_timestamp(pr, repo):
    head_oid = pr.get("headRefOid")
    if not head_oid:
        return None
    timestamp, code = run(
        f"gh api repos/{repo}/commits/{head_oid} --jq .commit.committer.date"
    )
    if code != 0:
        return None
    return parse_github_timestamp(timestamp)

def _meaningful_field_value(value):
    normalized = value.strip().strip("-–—").strip()
    return bool(normalized) and normalized.casefold() not in {"n/a", "none", "todo", "tbd", "na"}

def _is_top_level_field_label(line):
    if line[:1] in {" ", "\t"}:
        return False
    return re.match(r"^(?:[-*]\s*)?[a-z0-9 /_-]+\s*:(?:\s.*)?$", line.strip(), re.I)

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
                # Next field label is a boundary whether or not it carries a
                # value: an empty `- Risk:` must not borrow the value of a
                # following populated field (e.g. `- Rollback notes: revert`).
                if _is_top_level_field_label(next_line):
                    break
                if _meaningful_field_value(stripped):
                    return True
                break
    return False

def has_duplicate_guard_ack(body):
    if re.search(r"(?im)^[ \t]*[-*][ \t]*\[[xX]\][ \t]*\*\*duplicate pr guard:\*\*", body):
        return True
    return has_non_empty_body_field(
        body,
        [
            "duplicate pr guard",
            "duplicate-pr guard",
            "duplicate/overlap check",
            "overlap check",
        ],
    )

def has_no_change_verification_ack(body):
    if re.search(r"(?im)^[ \t]*[-*][ \t]*\[[xX]\][ \t]*\*\*no-change verification:\*\*", body):
        return True
    return has_non_empty_body_field(
        body,
        [
            "no-change verification",
            "no change verification",
        ],
    )

def has_stale_branch_cleanup_ack(body):
    if re.search(r"(?im)^[ \t]*[-*][ \t]*\[[xX]\][ \t]*\*\*stale branch cleanup:\*\*", body):
        return True
    return has_non_empty_body_field(
        body,
        [
            "stale branch cleanup",
            "stale-branch cleanup",
        ],
    )

def has_scratch_file_cleanup_ack(body):
    if re.search(r"(?im)^[ \t]*[-*][ \t]*\[[xX]\][ \t]*\*\*scratch file cleanup:\*\*", body):
        return True
    return has_non_empty_body_field(
        body,
        [
            "scratch file cleanup",
            "scratch-file cleanup",
        ],
    )

def main():
    repo = _detect_repo()
    print("Fetching PRs...")
    prs_json, gh_code = run(f"gh pr list --repo {repo} --state open --limit 50 --json number,title,headRefName,createdAt,headRefOid,body")

    if gh_code != 0 or not prs_json:
        print("Warning: `gh` CLI not available or failed. Skipping PR analysis.")
        return 0

    try:
        prs = json.loads(prs_json)
    except Exception as e:
        print(f"Error parsing JSON: {e}")
        print(prs_json)
        return 1

    inventory_refresh_count = 0
    now = datetime.now(timezone.utc)

    for pr in prs:
        num = pr['number']
        title = pr['title']
        body = str(pr.get('body') or '')
        normalized_body = body.lower()
        head_commit_at = head_commit_timestamp(pr, repo)
        print(f"\n# {num} - {title}")

        # Check PR hygiene requirements
        if "workfingerprint" not in normalized_body:
            print("  [!] MISSING FINGERPRINT: PR body lacks the required 'WorkFingerprint' section.")
        if not has_duplicate_guard_ack(body):
            print("  [!] MISSING OVERLAP CHECK: PR body lacks a completed duplicate/overlap guard acknowledgement.")
        if not has_no_change_verification_ack(body):
            print("  [!] MISSING NO-CHANGE VERIFICATION CHECK: PR body lacks a completed no-change verification acknowledgement.")
        if not has_stale_branch_cleanup_ack(body):
            print("  [!] MISSING STALE BRANCH CLEANUP CHECK: PR body lacks a completed stale branch cleanup acknowledgement.")
        if not has_scratch_file_cleanup_ack(body):
            print("  [!] MISSING SCRATCH FILE CLEANUP CHECK: PR body lacks a completed scratch file cleanup acknowledgement.")
        if "verification" not in normalized_body:
            print("  [!] MISSING VERIFICATION: PR body lacks the required 'verification' commands and results.")
        if "skipped checks" not in normalized_body:
            print("  [!] MISSING SKIPPED CHECKS: PR body fails to mention 'skipped checks' with reasons.")
        if not has_non_empty_body_field(body, ["risk", "risk assessment"]):
            print("  [!] MISSING RISK: PR body fails to mention 'risk' assessment.")
        if not has_non_empty_body_field(body, ["rollback notes", "rollback"]):
            print("  [!] MISSING ROLLBACK NOTES: PR body fails to mention 'rollback notes'.")

        # 2026-05-13 lesson: treat low-signal or stale broad branches as queue debt
        is_stale = head_commit_at is not None and (now - head_commit_at) > timedelta(days=14)

        # Get diff stat
        stat, _ = run(f"gh pr diff {num} --repo {repo} --stat")
        print(f"Stat:\n{stat}")

        if is_stale:
            print(f"  [!] STALE BRANCH: Head commit is > 14 days old. Treat as queue debt. Close or recommend closing instead of salvaging in place.")

        # PR #214/#215 lesson: no-change PRs must have 0 changed files
        if "no-change" in title.lower():
            files_json, _ = run(f"gh pr view {num} --repo {repo} --json files")
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
            if not has_duplicate_guard_ack(body):
                print("  [!] MISSING DUPLICATE PR GUARD: Inventory refresh PR body lacks a completed duplicate-pr guard acknowledgement.")

    if inventory_refresh_count > 1:
        print("\n[!] WARNING: Multiple open inventory refresh PRs detected. Ensure strict duplicate-PR guard is followed.")
        return 1
    return 0

if __name__ == "__main__":
    sys.exit(main())
