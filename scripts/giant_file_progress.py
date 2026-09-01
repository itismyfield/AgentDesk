#!/usr/bin/env python3
from __future__ import annotations
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path
import generate_inventory_docs as inventory
ROOT = Path(__file__).resolve().parent.parent
EVIDENCE = ROOT / "target/giant-file-progress/evidence.json"
REGISTRY = "scripts/giant_file_registry.toml"
FROZEN = ("scripts/audit_maintainability_giant_baseline.toml", "scripts/giant_file_closed_issue_transition_list.txt", "scripts/giant_file_issue_metadata.json")
BOOTSTRAP_PATHS = frozenset({
    ".github/workflows/ci-main.yml",
    ".github/workflows/ci-pr.yml",
    "scripts/check-ci-runner-hardening.sh",
    "scripts/check_agent_maintenance_docs.py",
    "scripts/ci-script-checks.sh",
    "scripts/generate_inventory_docs.py",
    "scripts/giant_file_progress.py",
    REGISTRY,
    "src/services/discord/turn_finalizer.rs",
    "src/services/discord/turn_finalizer/terminal_handler.rs",
    "tests/test_api_docs_coverage.py",
    "tests/test_fast_check_ci_wiring.py",
    "tests/test_giant_file_progress.py",
    "tests/test_inventory_giant_split.py", "ARCHITECTURE.md",
    "docs/agent-maintenance/change-surfaces.md",
})
def git(*args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", *args], cwd=ROOT, check=True, capture_output=True, text=not binary
    )
    return result.stdout
def oid(ref: str, suffix: str = "commit") -> str:
    return str(git("rev-parse", "--verify", f"{ref}^{{{suffix}}}" if suffix else ref)).strip()
def provenance_matches(candidate: str, base: str, head: str, origin: str, parents: list[str]) -> bool: return parents == [candidate, base, head] and base == origin
def main_clean(snapshot: dict[str, object]) -> bool: return not snapshot["overdue"]
def archive(ref: str, destination: Path) -> None:
    with tarfile.open(fileobj=io.BytesIO(git("archive", "--format=tar", ref, binary=True))) as bundle:
        members = bundle.getmembers()
        unsafe = any(item.name.startswith("/") or ".." in Path(item.name).parts
                     or not (item.isfile() or item.isdir()) for item in members)
        if unsafe:
            raise RuntimeError("snapshot contains a non-regular or unsafe path")
        bundle.extractall(destination)
def diff_facts(base: str, candidate: str) -> dict[str, object]:
    changed = set(str(git("diff", "--name-only", "-z", base, candidate)).split("\0")) - {""}
    additions = 0
    for row in str(git("diff", "--numstat", "--no-renames", base, candidate)).splitlines():
        added, _deleted, _path = row.split("\t", 2)
        if not added.isdigit():
            raise RuntimeError("binary change is not valid progress")
        additions += int(added)
    statuses = str(git("diff", "--name-status", "--find-renames", "--find-copies", base, candidate))
    return {"changed": changed, "additions": additions,
            "rename_copy": any(row.startswith(("R", "C")) for row in statuses.splitlines())}
def moved_lines(base: str, candidate: str, root: str) -> int:
    prefix = root[:-3] + "/"
    patch = str(git("diff", "--unified=0", base, candidate, "--", root, prefix))
    current, deleted, added = "", set(), set()
    for line in patch.splitlines():
        if line.startswith("+++ b/"):
            current = line[6:]
        elif line.startswith("-") and not line.startswith("---") and current == root:
            value = line[1:].strip()
            if value and not value.startswith(("//", "/*", "*")):
                deleted.add(value)
        elif line.startswith("+") and not line.startswith("+++") and current.startswith(prefix):
            value = line[1:].strip()
            if value and not value.startswith(("//", "/*", "*")):
                added.add(value)
    return len(deleted & added)
def without_entry(text: str, path: str) -> str | None:
    lines = text.splitlines(keepends=True)
    try:
        file_line = next(i for i, line in enumerate(lines) if line.strip() == f'file = "{path}"')
        start = max(i for i in range(file_line + 1) if lines[i].strip() == "[[entry]]")
        end = next(i for i in range(file_line + 1, len(lines)) if not lines[i].strip())
    except (StopIteration, ValueError):
        return None
    return "".join(lines[:start] + lines[end + 1:])
def progress_errors(base: dict[str, object], candidate: dict[str, object],
                    facts: dict[str, object]) -> list[str]:
    errors: list[str] = []
    before, after = set(base["overdue"]), set(candidate["overdue"])
    base_loc, candidate_loc = base["modules"], candidate["modules"]
    base_meta, candidate_meta = base["registrations"], candidate["registrations"]
    retired = before - after
    if not before or not after < before:
        errors.append("overdue set is not a proper subset of nonempty base debt")
    if facts["rename_copy"]:
        errors.append("rename/copy cannot prove same-path retirement")
    changed = facts["changed"]
    if len(changed) > 20 or facts["additions"] > 800:
        errors.append("diff exceeds 20 files or 800 additions")
    expected = BOOTSTRAP_PATHS if facts["bootstrap"] else {
        REGISTRY, *retired,
        *(child for children in facts["children"].values() for child in children),
    }
    if changed != expected:
        errors.append("changed-path closure is not exact")
    for path in after:
        if base_meta.get(path) != candidate_meta.get(path):
            errors.append(f"retained metadata changed: {path}")
    for path, loc in candidate_loc.items():
        if loc >= 1000 and (base_loc.get(path, 0) < 1000 or loc > base_loc.get(path, 0)):
            errors.append(f"new or growing giant: {path}")
    for path in retired:
        old, new = base_loc.get(path), candidate_loc.get(path)
        children = facts["children"].get(path, ())
        if old is None or old < 1000 or new is None or new >= 1000 or new >= old:
            errors.append(f"not an actual same-path retirement: {path}")
        if path not in base_meta or path in candidate_meta:
            errors.append(f"registry entry not retired exactly once: {path}")
        if not children or any(candidate_loc.get(child, 0) >= 1000 for child in children):
            errors.append(f"retirement lacks bounded derived child: {path}")
        if facts["moved"].get(path, 0) < max(1, min(20, (old or 0) - (new or 0))):
            errors.append(f"retirement lacks moved production code: {path}")
    if not facts["authority_equal"]:
        errors.append("frozen authority blob changed")
    if not facts["registry_exact"]:
        errors.append("registry is not the exact retired-entry deletion")
    return errors
def write_evidence(payload: dict[str, object]) -> None:
    EVIDENCE.parent.mkdir(parents=True, exist_ok=True)
    temporary = EVIDENCE.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(EVIDENCE)
def main() -> int:
    env = os.environ
    event, repository = env.get("GFP_EVENT_NAME", ""), env.get("GFP_REPOSITORY", "")
    candidate_sha = env.get("GFP_CANDIDATE_SHA", "")
    selector, candidate = "main_fail_closed", {"overdue": [], "registrations": {}}
    payload: dict[str, object] = {
        "schema": 1, "evaluator_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "event": event, "repository": repository, "selected": True, "executed": True,
    }
    try:
        if Path(__file__).is_symlink() or str(git("status", "--porcelain")).strip():
            raise RuntimeError("evaluator input must be regular and clean")
        candidate_sha = oid(candidate_sha)
        if oid("HEAD") != candidate_sha:
            raise RuntimeError("candidate SHA is not checked-out HEAD")
        today = inventory.today_utc()
        with tempfile.TemporaryDirectory() as temporary:
            candidate_root = Path(temporary) / "candidate"
            candidate_root.mkdir(); archive(candidate_sha, candidate_root)
            candidate = inventory.giant_file_snapshot(candidate_root, evaluation_date=today)
            if event == "pull_request":
                selector = "pr_strict_progress"
                if repository != "itismyfield/AgentDesk" or env.get("GFP_HEAD_REPOSITORY") != repository:
                    raise RuntimeError("progress requires an exact same-repository PR")
                git("fetch", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
                base_sha, head_sha = oid(env.get("GFP_BASE_SHA", "")), oid(env.get("GFP_HEAD_SHA", ""))
                parents = str(git("rev-list", "--parents", "-n1", candidate_sha)).split()
                origin_sha = oid("origin/main")
                if not provenance_matches(candidate_sha, base_sha, head_sha, origin_sha, parents):
                    raise RuntimeError("event/base/head/merge object provenance mismatch")
                base_root = Path(temporary) / "base"
                base_root.mkdir(); archive(base_sha, base_root)
                base = inventory.giant_file_snapshot(base_root, evaluation_date=today)
                facts = diff_facts(base_sha, candidate_sha)
                facts["bootstrap"] = not (base_root / "scripts/giant_file_progress.py").exists()
                retired = set(base["overdue"]) - set(candidate["overdue"])
                facts["children"] = {path: sorted(child for child in facts["changed"]
                    if child.startswith(path[:-3] + "/") and child.endswith(".rs")) for path in retired}
                facts["moved"] = {path: moved_lines(base_sha, candidate_sha, path) for path in retired}
                facts["authority_equal"] = all(oid(f"{base_sha}:{path}", "")
                    == oid(f"{candidate_sha}:{path}", "") for path in FROZEN)
                expected = (base_root / REGISTRY).read_text(encoding="utf-8")
                for path in sorted(retired):
                    expected = without_entry(expected, path) or ""
                facts["registry_exact"] = expected == (candidate_root / REGISTRY).read_text(encoding="utf-8")
                errors = progress_errors(base, candidate, facts)
                if errors:
                    raise RuntimeError("; ".join(errors))
                payload.update({"event_base_sha": base_sha, "observed_origin_main_sha": origin_sha,
                    "merge_first_parent": parents[1], "head_sha": head_sha, "merge_sha": candidate_sha,
                    "base_tree": oid(base_sha, "tree"), "base_overdue": base["overdue"],
                    "retired": [{"path": path, "base_prod_loc": base["modules"][path], "candidate_prod_loc": candidate["modules"][path], "children": [{"path": child, "candidate_prod_loc": candidate["modules"][child]} for child in facts["children"][path]]} for path in sorted(retired)],
                    "changed_files": len(facts["changed"]), "additions": facts["additions"]})
                reason = "proper subset with actual same-path retirement"
            elif event == "push" and repository == "itismyfield/AgentDesk":
                if not main_clean(candidate):
                    raise RuntimeError("main has overdue giant-file debt")
                reason = "ordinary zero-debt main"
            else:
                selector = "reject"
                raise RuntimeError("event is not protected main/push or same-repository PR")
            if env.get("GFP_REFRESH_DOCS") == "1":
                inventory.write_documents(inventory.generated_documents(allow_overdue=True), check=False)
            payload.update({"selector": selector, "candidate_tree": oid(candidate_sha, "tree"),
                "candidate_overdue": candidate["overdue"], "ordinary_problem_count": 0,
                "metadata_fingerprints": {path: hashlib.sha256(repr(value).encode()).hexdigest()
                    for path, value in sorted(candidate["registrations"].items())},
                "verdict": "progress-pass", "reason": reason})
            write_evidence(payload)
            return 0
    except (OSError, RuntimeError, subprocess.CalledProcessError, inventory.ParseError) as error:
        payload.update({"selector": selector, "selected": selector != "reject", "candidate_overdue": candidate["overdue"],
            "ordinary_problem_count": 1, "verdict": "fail", "reason": str(error)})
        try:
            write_evidence(payload)
        except OSError as write_error:
            print(f"giant progress evidence write failed: {write_error}", file=sys.stderr)
        print(f"giant progress failed: {error}", file=sys.stderr)
        return 2
if __name__ == "__main__": raise SystemExit(main())
