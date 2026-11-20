"""Validate the feature map and publish revision-specific coverage evidence."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
from difflib import unified_diff
import os
from pathlib import Path
import runpy
import sys
from tempfile import TemporaryDirectory

import yaml

from scripts import generate_user_feature_map as gen
from scripts.ci.generated_artifact_report import ReportError, git, publish, repository_url, resolve_comparison


@contextmanager
def checkout(repo, sha):
    with TemporaryDirectory(prefix="feature-map-report-") as temporary:
        root = Path(temporary) / "source"
        git(repo, "worktree", "add", "--quiet", "--detach", str(root), sha)
        try:
            yield root
        finally:
            git(repo, "worktree", "remove", str(root))


def render_revision(repo, sha, baseline, *, candidate=False):
    with checkout(repo, sha) as root:
        if candidate and os.path.lexists(root / gen.RETIRED_REPORT):
            raise ReportError(f"{gen.RETIRED_REPORT}: remove the retired coverage JSON from the candidate")
        if not candidate and not (root / gen.ROWS).exists():
            return None
        with gen.input_context(sha):
            options = runpy.run_path(str(root / gen.REGISTRY))["ASSERTION_OPTIONS"]
            markdown, coverage = gen.generate(root, baseline, assertion_options=options)
            rows = gen.load_yaml(root / gen.ROWS)
        removed = {entry["key"]: entry["reason"] for entry in rows.get("removal_reasons", [])}
        return markdown, coverage, removed


def tuple_changes(old, new):
    old_tuples = {entry["key"]: entry for feature in old.values() for entry in feature["tuples"]}
    new_tuples = {entry["key"]: entry for feature in new.values() for entry in feature["tuples"]}
    for key in sorted(old_tuples.keys() | new_tuples.keys()):
        before, after = old_tuples.get(key), new_tuples.get(key)
        if before == after:
            continue
        if before is None:
            yield f"- Added `{key}` ({after['status']})"
        elif after is None:
            yield f"- Removed `{key}` ({before['status']})"
        else:
            status = f"{before['status']} → {after['status']}"
            details = []
            for field, label in (("required_assertion_ids", "required"), ("scenario_ids", "live claims"),
                                 ("non_live_scenario_ids", "non-live claims")):
                if before[field] != after[field]:
                    previous = ", ".join(before[field]) or "—"
                    current = ", ".join(after[field]) or "—"
                    details.append(f"{label} {previous} → {current}")
            yield f"- Changed `{key}` ({'; '.join([status, *details])})"


def report(repo, directory, baseline_sha, env):
    comparison = resolve_comparison(repo, env)
    markdown, coverage, removals = render_revision(repo, comparison.head, baseline_sha, candidate=True)
    source_link = "summary.md#source-inputs"
    files = {"head.md": gen.with_rows_link(markdown, source_link).encode(),
             "head.json": gen.report_bytes(coverage)}
    repository = repository_url(env)
    inputs = ["", "### Source inputs", "",
              f"- [Head rows YAML]({repository}/blob/{comparison.head}/{gen.ROWS.as_posix()})"]
    lines = ["## User feature map", "", comparison.describe(),
             f"Source validation baseline: `{baseline_sha}`.", "",
             f"Applicable {coverage['applicable']} · covered {coverage['covered']} · missing {coverage['missing']}."]

    if comparison.mode != "self":
        ancestry = git(repo, "rev-list", "--parents", "-n", "1", comparison.base).decode().split()
        base_baseline = ancestry[1] if len(ancestry) > 1 else comparison.base
        base = render_revision(repo, comparison.base, base_baseline)
        if base is None:
            lines.append(f"Comparison unavailable. `{comparison.base}` predates the handwritten feature rows.")
        else:
            old_markdown, old_coverage, old_removals = base
            files.update({"base.md": gen.with_rows_link(old_markdown, source_link).encode(),
                          "base.json": gen.report_bytes(old_coverage)})
            inputs.append(f"- [Base rows YAML]({repository}/blob/{comparison.base}/{gen.ROWS.as_posix()})")
            patch = "".join(unified_diff(old_markdown.splitlines(keepends=True), markdown.splitlines(keepends=True),
                                         fromfile=f"{comparison.base}:{gen.MARKDOWN}",
                                         tofile=f"{comparison.head}:{gen.MARKDOWN}"))
            if patch:
                files["feature-map.diff"] = patch.encode()
                lines.append("The Markdown patch is included as `feature-map.diff`.")
            else:
                lines.append("Markdown unchanged.")
            if coverage["source_hash"] != old_coverage["source_hash"]:
                lines.append(f"Source hash changed: `{old_coverage['source_hash']}` → `{coverage['source_hash']}`.")
            old_ids, new_ids = old_coverage["by_feature"], coverage["by_feature"]
            lines += [f"- Added feature `{fid}`" for fid in sorted(new_ids.keys() - old_ids.keys())]
            lines += [f"- Removed feature `{fid}`" for fid in sorted(old_ids.keys() - new_ids.keys())]
            lines.extend(tuple_changes(old_ids, new_ids))
            for key, reason in sorted(removals.items()):
                if old_removals.get(key) != reason:
                    lines.append(f"- Removal reason for `{key}`: {reason}")
    publish(directory, comparison, "\n".join(lines + inputs) + "\n", files, env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--base-ref", required=True, help="existing feature map validation baseline")
    args = parser.parse_args()
    root, baseline = Path.cwd(), args.base_ref
    try:
        baseline = git(root, "rev-parse", "--verify", "--end-of-options", f"{baseline}^{{commit}}").decode().strip()
        report(root, args.output_dir, baseline, os.environ)
    except (OSError, ValueError, KeyError, yaml.YAMLError) as error:
        print(f"user feature map: {error}\nReproduce: {gen.recovery(baseline)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
