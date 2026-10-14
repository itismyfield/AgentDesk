#!/usr/bin/env python3
"""B3b-4 dormancy (#5340): the recovery custody and registry bodies exist only under cfg(test).

Checked together: module declarations, re-exports and aliases, references from other files (a
field, a constructor call, a wiring point), the test-only seams the bodies use, gate shape
(`cfg(any(test, ..))`, `cfg_attr`, a feature), the 0140 migration staying additive, and files that
must not change. `--diff-base REF` adds the changed-file check under `--policy NAME` (or the
default list); `--forbid GLOB` extends it. General CI runs `dormant`, which freezes no file; a slice
names its own freeze list. In CI (`--ci` or CI=true) a run without both a base and a policy fails
as unverified instead of passing.
"""
import argparse
import fnmatch
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODULES = ("channel_recovery_custody", "recovery_custody", "recovery_registry", "mailbox_access",
           "mailbox_transport")
BODY = (
    "src/db/channel_recovery_custody.rs",
    "src/services/turn_orchestrator/recovery_custody/",
    "src/services/turn_orchestrator/recovery_registry/",
)
PURGE = "src/services/turn_orchestrator/registry_purge.rs"
MAILBOX = "src/services/turn_orchestrator.rs"
FENCE = "src/services/turn_orchestrator/input_fence.rs"
INCARNATION = "src/services/turn_orchestrator/incarnation.rs"
# file -> the only lines that may name a B3b-4 module there, each under exactly `#[cfg(test)]`.
DECLARATIONS = {
    "src/db/mod.rs": ("pub(crate) mod channel_recovery_custody;",),
    PURGE: (
        '#[path = "recovery_custody/mod.rs"]', "pub(crate) mod recovery_custody;",
        '#[path = "recovery_registry/mod.rs"]', "pub(crate) mod recovery_registry;",
        '#[path = "recovery_custody/mailbox_access.rs"]', "mod mailbox_access;",
        '#[path = "recovery_custody/mailbox_transport.rs"]', "pub(super) mod mailbox_transport;",
    ),
    # The actor's carried apply: one test-only message variant.
    MAILBOX: ("Carried(Box<registry_purge::mailbox_transport::Carried>),",),
}
# file -> the only other lines a slice may add there, each a test-only arm of that variant.
WIRING = {
    MAILBOX: ("ChannelMailboxMsg::Carried(msg) => msg.apply(&mut state, channel_id, own_recovery_done),",
              "use incarnation::spawn_channel_mailbox;"),
    FENCE: ("M::Carried(carried) => carried.persistence(),", "M::Carried(carried) => carried.refuse_fenced(failure),"),
    PURGE: ("M::Carried(carried) => carried.refuse_closed(),",),
}
# file -> the file its removed lines must reappear in: request and spawn moved, not rewritten.
MOVED = {MAILBOX: INCARNATION}
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
TEST_ONLY = ("fn isolated(", "fn unlocked(", "mod commit_fault {", "mod seam {", "mod custody_pg_tests;",
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
# The writers and restore paths the actor slice leaves to the writer slice.
WRITERS = (
    "src/services/turn_orchestrator/queued_head.rs", "src/services/turn_orchestrator/pending_queue_persistence.rs",
    "src/services/turn_orchestrator/scoped_claim.rs", "src/services/turn_orchestrator/installed_actor_handoff.rs",
    "src/services/turn_orchestrator/queue_enqueue.rs", "src/services/cluster/channel_home*",
    "src/services/tui_o/*", "src/services/discord/recovery_engine/restore_inflight*",
)
# policy -> (globs that must not change, files that may change only in their DECLARATIONS lines).
POLICIES = {
    # Every PR: the structural checks above only, so unrelated DB or migration work still passes.
    "dormant": ((), ()),
    # The feature PR against its base branch: what no B3b-4 slice changes.
    "b3b4": (FORBIDDEN + ("src/services/discord/runtime_bootstrap*", "migrations/*", "src/db/*"),
             (PURGE, MAILBOX, FENCE)),
    # Slice 4a2b-0 against its start head: only the dormant bodies, this gate and registrations.
    "4a2b-0": (FORBIDDEN + (
        "src/services/turn_orchestrator.rs", "src/services/turn_orchestrator/incarnation.rs", PURGE,
        "src/services/cluster/channel_home_port.rs",
        "src/services/turn_orchestrator/pending_queue_persistence.rs",
        "src/services/turn_orchestrator/queued_head.rs", "src/services/turn_orchestrator/queue_enqueue.rs",
        "src/services/discord/*", ".github/workflows/*", "src/db/*", "migrations/*"), ()),
    # Slice 4a2b-1: the actor contact; the mailbox, fence and purge change only in their wiring.
    "4a2b-1": (FORBIDDEN + WRITERS + (
        "src/services/discord/runtime_bootstrap*", ".github/workflows/*", "src/db/*", "migrations/*"),
        (MAILBOX, FENCE, PURGE)),
}
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
                gates = gate_above(lines, number - 1)
                if text not in allowed:
                    errors.append(f"{path}:{number}: reference to a dormant module outside it: {text}")
                elif not text.startswith("#[") and "#[cfg(test)]" not in gates:
                    errors.append(f"{path}:{number}: declaration not under #[cfg(test)]: {text}")
                elif any(not gate.startswith(("#[cfg(test)]", "#[path")) for gate in gates):
                    errors.append(f"{path}:{number}: declaration gate is not exactly #[cfg(test)]: {text}")
    for path, declared in DECLARATIONS.items():
        text = [line.strip() for line in sources.get(path, [])]
        errors.extend(f"{path}: declaration missing: {item}" for item in declared if text.count(item) != 1)
    for path, wired in WIRING.items():
        lines = sources.get(path, [])
        errors.extend(f"{path}:{number}: wiring not under #[cfg(test)]: {line.strip()}"
                      for number, line in enumerate(lines, 1)
                      if line.strip() in wired and "#[cfg(test)]" not in gate_above(lines, number - 1))
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


def git(root, *args):
    done = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    if done.returncode:
        raise RuntimeError(done.stderr.strip() or f"git {args[0]} failed")
    return done.stdout


def changed_lines(root, base, name):
    """The (sign, stripped text) of each changed line of `name`, without blank and comment lines."""
    diff = git(root, "diff", "-U0", f"{base}...HEAD", "--", name).splitlines()
    lines = [(line[0], line[1:].strip()) for line in diff
             if line[:1] in "+-" and not line.startswith(("+++", "---"))]
    return [(sign, text) for sign, text in lines if text and not text.startswith("//")]


def visible(text):
    """A moved line without the visibility its new module gives it."""
    return re.sub(r"^pub(\([^)]*\))? ", "", text)


def changed_forbidden(root, base, policy, extra):
    forbidden, declared_only = POLICIES[policy] if policy else (FORBIDDEN, ())
    globs = forbidden + tuple(extra)
    errors = []
    for name in git(root, "diff", "--name-only", f"{base}...HEAD").split():
        if any(fnmatch.fnmatch(name, glob) for glob in globs):
            errors.append(f"{name}: changed against {base} but must stay as it is")
        elif name in declared_only:
            allowed = DECLARATIONS.get(name, ()) + WIRING.get(name, ()) + ("#[cfg(test)]",)
            moved = [visible(text) for sign, text in changed_lines(root, base, MOVED[name])
                     if sign == "+"] if name in MOVED else []
            for sign, line in changed_lines(root, base, name):
                if sign == "-" and visible(line) in moved:
                    moved.remove(visible(line))
                elif line not in allowed:
                    errors.append(f"{name}: `{line}` changed; only its gated declarations may")
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--diff-base")
    parser.add_argument("--policy", choices=sorted(POLICIES))
    parser.add_argument("--forbid", action="append", default=[])
    parser.add_argument("--ci", action="store_true")
    args = parser.parse_args(argv)
    errors = check(args.root)
    if (args.ci or os.environ.get("CI") == "true") and not (args.diff_base and args.policy):
        errors.append("unverified: CI needs --diff-base and --policy for the changed-file check")
    elif args.diff_base:
        try:
            errors += changed_forbidden(args.root, args.diff_base, args.policy, args.forbid)
        except RuntimeError as error:
            errors.append(f"unverified: no diff against {args.diff_base}: {error}")
    print("\n".join(errors) if errors else "B3b-4 dormancy: PASS")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
