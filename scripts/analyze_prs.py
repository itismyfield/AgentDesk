import subprocess
import json
from datetime import datetime, timedelta, timezone

def run(cmd):
    result = subprocess.run(cmd, capture_output=True, text=True, shell=True)
    return result.stdout.strip(), result.returncode

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
        f"gh api repos/kunkunGames/AgentDesk/commits/{head_oid} --jq .commit.committer.date"
    )
    if code != 0:
        return None
    return parse_github_timestamp(timestamp)

print("Fetching PRs...")
prs_json, gh_code = run("gh pr list --repo kunkunGames/AgentDesk --state open --limit 50 --json number,title,headRefName,createdAt,headRefOid")

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
    head_commit_at = head_commit_timestamp(pr)
    print(f"\n# {num} - {title}")

    # 2026-05-13 lesson: treat low-signal or stale broad branches as queue debt
    is_stale = head_commit_at is not None and (now - head_commit_at) > timedelta(days=14)

    # Get diff stat
    stat, _ = run(f"gh pr diff {num} --repo kunkunGames/AgentDesk --stat")
    print(f"Stat:\n{stat}")

    if is_stale:
        print(f"  [!] STALE BRANCH: Head commit is > 14 days old. Treat as queue debt. Close or recommend closing instead of salvaging in place.")

    # PR #214/#215 lesson: no-change PRs must have 0 changed files
    if "no-change" in title.lower():
        files_json, _ = run(f"gh pr view {num} --repo kunkunGames/AgentDesk --json files")
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

if inventory_refresh_count > 1:
    print("\n[!] WARNING: Multiple open inventory refresh PRs detected. Ensure strict duplicate-PR guard is followed.")
    exit(1)
