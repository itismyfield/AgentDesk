#!/usr/bin/env python3
"""B3b-4 dormancy (#5340): the recovery custody and registry bodies exist only under cfg(test).

Checked together: module declarations, re-exports and aliases, references from other files (a
field, a constructor call, a wiring point), the test-only seams the bodies use, gate shape
(`cfg(any(test, ..))`, `cfg_attr`, a feature), the 0140 migration staying additive, and files that
must not change. `--diff-base REF` adds the changed-file check; `--forbid GLOB` extends its list.
"""
import argparse
import fnmatch
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODULES = ("channel_recovery_custody", "recovery_custody", "recovery_registry")
BODY = (
    "src/db/channel_recovery_custody.rs",
    "src/services/turn_orchestrator/recovery_custody/",
    "src/services/turn_orchestrator/recovery_registry/",
)
PURGE = "src/services/turn_orchestrator/registry_purge.rs"
# file -> the only lines that may name a B3b-4 module there, each under exactly `#[cfg(test)]`.
DECLARATIONS = {
    "src/db/mod.rs": ("pub(crate) mod channel_recovery_custody;",),
    PURGE: (
        '#[path = "recovery_custody/mod.rs"]', "pub(crate) mod recovery_custody;",
        '#[path = "recovery_registry/mod.rs"]', "pub(crate) mod recovery_registry;",
    ),
}
# file -> (gate, item): test-only seams outside the bodies; the gate sits right above the item.
SEAMS = {
    "src/services/agent_recovery/admission.rs": ("#[cfg(test)]", "impl AdmissionGuard {"),
    "src/db/o_channel_homes.rs": ("#[cfg(test)]", "pub(crate) async fn read_home_in("),
    "src/services/cluster/channel_home_port.rs": (
        "#[cfg(all(test, unix))]", "pub(crate) mod scoped_restore {"),
}
# Names that may exist only inside the gated scoped_restore block.
SCOPED_ONLY = ("AttemptIdentity", "ExactWitness", "Installed,")
# Test fixtures inside the bodies; each needs its own `#[cfg(test)]`.
TEST_ONLY = ("fn isolated(", "fn unlocked(", "mod commit_fault {", "mod custody_pg_tests;",
             "mod reconcile_pg_tests;", "mod registry_pg_tests;")
OPEN_GATE = re.compile(r"cfg\(\s*any\(|cfg_attr|feature\s*=|#!?\[allow\(|env::var|env!\(")
MIGRATION = "migrations/postgres/0140_channel_recovery_custody.sql"
NOT_ADDITIVE = re.compile(r"\b(ALTER|DROP|UPDATE|INSERT|DELETE|TRIGGER|VIEW|TRUNCATE)\b", re.I)
# Unchanged in every B3b-4 slice.
FORBIDDEN = (
    "src/services/turn_orchestrator/injected_inputs.rs", "src/services/discord/queue_io/backstop.rs",
    "src/services/discord/queue_dispatch/kickoff.rs", "src/services/discord/queue_io.rs",
    "src/services/discord/mod.rs",
)
NAMED = re.compile(r"\b(?:%s)\b" % "|".join(MODULES))


def is_body(path):
    return any(path == body or path.startswith(body) for body in BODY)


def is_test_file(path):
    return path.endswith(("_tests.rs", "_test.rs")) or "/tests/" in path


def gate_above(lines, index):
    """The attribute lines directly above `lines[index]`, nearest first."""
    gates = []
    while index > 0 and lines[index - 1].strip().startswith("#["):
        index -= 1
        gates.append(lines[index].strip())
    return gates


def block_end(lines, start):
    depth = 0
    for index in range(start, len(lines)):
        depth += lines[index].count("{") - lines[index].count("}")
        if depth == 0:
            return index
    return len(lines) - 1


def check(root):
    errors = []
    sources = {str(path.relative_to(root)): path.read_text().splitlines()
               for path in sorted((root / "src").rglob("*.rs"))}
    for path, lines in sources.items():
        allowed = DECLARATIONS.get(path, ())
        for number, line in enumerate(lines, 1):
            text = line.strip()
            if is_body(path):
                gated = "#[cfg(test)]" in gate_above(lines, number - 1)
                if OPEN_GATE.search(text) and not text.startswith("//") and not is_test_file(path):
                    errors.append(f"{path}:{number}: gate or switch not allowed in a dormant body: {text}")
                if any(text.endswith(item) or item in text for item in TEST_ONLY) and not gated:
                    errors.append(f"{path}:{number}: test fixture without #[cfg(test)]: {text}")
            elif NAMED.search(text) and not text.startswith("//"):
                if text not in allowed:
                    errors.append(f"{path}:{number}: reference to a dormant module outside it: {text}")
                elif text.startswith("pub(crate) mod") and "#[cfg(test)]" not in gate_above(lines, number - 1):
                    errors.append(f"{path}:{number}: declaration not under #[cfg(test)]: {text}")
                elif text.startswith("pub(crate) mod") and any(
                        not gate.startswith(("#[cfg(test)]", "#[path")) for gate in gate_above(lines, number - 1)):
                    errors.append(f"{path}:{number}: declaration gate is not exactly #[cfg(test)]: {text}")
    for path, declared in DECLARATIONS.items():
        text = [line.strip() for line in sources.get(path, [])]
        errors.extend(f"{path}: declaration missing: {item}" for item in declared if text.count(item) != 1)
    for path, (gate, item) in SEAMS.items():
        lines = sources.get(path, [])
        at = [index for index, line in enumerate(lines) if line.strip() == item]
        if len(at) != 1 or gate_above(lines, at[0])[:1] != [gate]:
            errors.append(f"{path}: test seam `{item}` is not directly under {gate}")
        elif item.endswith("mod scoped_restore {"):
            end = block_end(lines, at[0])
            for number, line in enumerate(lines, 1):
                if any(name in line for name in SCOPED_ONLY) and not at[0] < number - 1 <= end:
                    errors.append(f"{path}:{number}: scoped-restore type outside its test block: {line.strip()}")
    cargo = (root / "Cargo.toml").read_text() if (root / "Cargo.toml").exists() else ""
    if re.search(r"(?m)^\s*(?:b3b4|recovery[-_](?:custody|registry))\w*\s*=", cargo):
        errors.append("Cargo.toml: a feature or key opens the dormant modules")
    migration = root / MIGRATION
    if not migration.exists():
        errors.append(f"{MIGRATION}: missing")
    else:
        sql = re.sub(r"--[^\n]*", "", migration.read_text())
        errors.extend(f"{MIGRATION}: not additive: {word.group(0)}" for word in NOT_ADDITIVE.finditer(sql))
    return errors


def changed_forbidden(root, base, extra):
    names = subprocess.run(["git", "-C", str(root), "diff", "--name-only", f"{base}...HEAD"],
                           capture_output=True, text=True, check=True).stdout.split()
    globs = FORBIDDEN + tuple(extra)
    return [f"{name}: changed against {base} but must stay as it is"
            for name in names if any(fnmatch.fnmatch(name, glob) for glob in globs)]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--diff-base")
    parser.add_argument("--forbid", action="append", default=[])
    args = parser.parse_args(argv)
    errors = check(args.root)
    if args.diff_base:
        errors += changed_forbidden(args.root, args.diff_base, args.forbid)
    print("\n".join(errors) if errors else "B3b-4 dormancy: PASS")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
