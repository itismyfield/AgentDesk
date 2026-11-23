"""Identify CI metadata edits and compare the YAML that controls Rust test execution."""

import re
from pathlib import PurePosixPath

import yaml

WORKFLOW_DIR = PurePosixPath(".github/workflows")
CHECK_SCRIPT = re.compile(r"scripts/check[-_][^/]+\.(sh|py|rb)")
TEST_JOBS = {"base_compare", "rust_scope", "rust_tests", "pg15_owner_proof", "test_evidence"}
JOB_DEFAULTS = ("env", "defaults", "permissions")


def is_workflow(path):
    file = PurePosixPath(path)
    return file.parent == WORKFLOW_DIR and file.suffix in (".yml", ".yaml")


def is_ci_metadata(path):
    file = PurePosixPath(path)
    if file.suffix == ".rs" or file.name in ("Cargo.toml", "Cargo.lock"):
        return False
    return (is_workflow(path) or path.startswith("scripts/ci/")
            or CHECK_SCRIPT.fullmatch(path) is not None or path == "scripts/ci-script-checks.sh"
            or (path.startswith("tests/") and path.endswith((".py", ".rb")))
            or (file.suffix == ".md" and (len(file.parts) == 1 or file.parts[0] == "docs")))


def mapping(node):
    if not isinstance(node, yaml.MappingNode):
        raise ValueError("expected a YAML mapping")
    entries = {}
    for key, value in node.value:
        if not isinstance(key, yaml.ScalarNode) or key.value in entries:
            raise ValueError("expected unique scalar YAML keys")
        entries[key.value] = (key.tag, value)
    return entries


def signature(node):
    # Keep scalar tags and text distinct; Python equality would conflate true and 1.
    if isinstance(node, yaml.ScalarNode):
        return node.tag, node.value
    if isinstance(node, yaml.SequenceNode):
        return node.tag, [signature(item) for item in node.value]
    return node.tag, {key: (tag, signature(value)) for key, (tag, value) in mapping(node).items()}


def test_wiring(raw):
    """Resolved test job definitions and inherited settings, keyed for diagnostics."""
    workflow = mapping(yaml.compose(raw, Loader=yaml.SafeLoader))
    jobs = mapping(workflow["jobs"][1])
    if "<<" in workflow or "<<" in jobs:
        raise ValueError("workflow and jobs must declare their keys explicitly")
    selected = {f"jobs.{job}": signature(jobs[job][1]) for job in TEST_JOBS & jobs.keys()}
    if selected:
        selected.update({key: signature(workflow[key][1]) for key in JOB_DEFAULTS if key in workflow})
    return selected
