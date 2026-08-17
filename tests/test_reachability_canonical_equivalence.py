"""Mutation proof for scripts/check_reachability_canonical_equivalence.py (#5071 T4-B2a).

The gate is the machine half of 4987 blocker B1'. A gate that only ever runs
green on the real corpus proves nothing about what it would catch, so every
check it makes is exercised here against a synthetic repo root reproducing one
defect at a time:

  * a Python output that no longer matches the golden corpus;
  * a corpus thinned below the floor, which is the vacuous-pass shape — a gate
    finding nothing to compare must never read as a gate that compared and
    agreed;
  * a mutation whose anchor text no longer exists, i.e. a mutation that is
    silently never applied. This is the failure mode a mutation runner is most
    likely to develop and least likely to notice;
  * a mutation the implementation no longer kills;
  * a consumer added outside the tree, the tree's module declaration removed,
    and a verdict read added to the observation-only B2c consumer;
  * the two ways a consumer can reach the tree WITHOUT writing `reachability::`
    — an `as` alias at the call site, and a re-export from the allowlisted file
    — each with the control that keeps the widened scan from reporting every
    local that happens to be called `reachability`. A gate whose central claim
    is "only the named observation and sanctioned consumers, machine-checked"
    is worth exactly what its weakest spelling catches;
  * a SANCTIONED consumer (#5071 T4-B4) that drifts off its contract — naming
    the tree in a `use` item (the alias/re-export laundering shape) or reading
    a tree path other than the sanctioned `divergence::` one (a fully-qualified
    verdict read). The tier is empty since #5071 T4-B6 promoted its only
    member, so a synthetic member patched into the set holds the rule;
  * a JUDGMENT consumer (#5071 T4-B6) that drifts off its wider contract —
    renaming the tree in a `use` item, re-exporting it, or reading a tree path
    outside the four T4-B6 unlocks (the ledger above all) — next to the
    allowance itself: a plain `use` plus a fully-qualified verdict read is
    exactly what T4-B6 landed, and must scan clean;
  * the TWO-STEP form of that laundering, which the r1 review reproduced
    against the live gate with a real compile: a judgment consumer imports a
    tree item with a plain `use` (allowed), then publishes it as a `pub type`
    alias or a `pub use` of the imported name, and the sibling that reads it
    never writes `reachability`. Both steps are legal on their own, which is
    why the gate has to see the pair. The private-alias control sits beside
    them: a `type` alias with no `pub` binds a spelling inside one file and
    must stay allowed, or the rule would be refusing readability rather than
    republication;
  * an allowance whose file no longer names the tree at all (a stale allowance
    nobody would notice);
  * a `warn_bound` introduced inside the tree (4987 §10 NO-GO).

The live-repo cases at the end pin that the gate is wired into
`scripts/ci-script-checks.sh`, that the checked-in tree passes it, and that the
declared Rust mutations still anchor on text that exists — a `--with-rust` run
needs a compiler and does not belong in the fast lane, but a Rust mutation
silently anchoring on deleted text is exactly the rot this file exists to catch,
and checking the ANCHOR costs nothing.
"""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_reachability_canonical_equivalence.py"
CI_SCRIPT = REPO_ROOT / "scripts" / "ci-script-checks.sh"

_SPEC = importlib.util.spec_from_file_location(
    "check_reachability_canonical_equivalence", SCRIPT_PATH
)
GATE = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = GATE
_SPEC.loader.exec_module(GATE)


def _mirror_repo(tmp: str) -> Path:
    """A synthetic root carrying only what the gate reads.

    Real files rather than hand-written stand-ins: the point of each case below
    is that ONE thing is wrong, and a from-scratch fixture would differ from the
    real tree in ways nobody chose.
    """
    root = Path(tmp)
    (root / "scripts").mkdir()
    (root / "tests/fixtures").mkdir(parents=True)
    (root / "src/services/discord/health").mkdir(parents=True)

    for name in ("relay_watchdog.py", "check_clippy_allow_ratchet.py"):
        shutil.copy(REPO_ROOT / "scripts" / name, root / "scripts" / name)
    shutil.copytree(
        REPO_ROOT / "tests/fixtures/relay_obligation",
        root / "tests/fixtures/relay_obligation",
    )
    shutil.copy(
        REPO_ROOT / "src/services/discord/health/reachability.rs",
        root / "src/services/discord/health/reachability.rs",
    )
    shutil.copytree(
        REPO_ROOT / "src/services/discord/health/reachability",
        root / "src/services/discord/health/reachability",
    )
    for rel in sorted(
        GATE.ALLOWED_TREE_REFERENCES
        | GATE.SANCTIONED_TREE_CONSUMERS
        | GATE.JUDGMENT_TREE_CONSUMERS
    ):
        source = REPO_ROOT / rel
        target = root / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy(source, target)
    return root


# An unsanctioned sibling path for synthetic intruders and bystanders. It must
# not appear in any allowance set, or the cases below would be testing the
# allowances instead of the scan.
INTRUDER_REL = "src/services/discord/health/stall_liveness.rs"

# The sanctioned tier is empty since #5071 T4-B6 promoted its only member, so
# the T4-B4 contract cases patch this synthetic member into the set. The path
# must not exist in the repo and must not appear in any other allowance set.
SANCTIONED_PROBE_REL = "src/services/discord/health/divergence_probe.rs"

# A real judgment consumer (#5071 T4-B6) the judgment-contract cases overwrite.
# Named literally rather than via `sorted(...)`: if it ever leaves the set, the
# overwrite trips "grew a consumer" and the case fails loudly.
JUDGMENT_CONSUMER_REL = "src/services/discord/health/snapshot.rs"


def _run(root: Path) -> list[str]:
    cases = GATE.load_corpus(root)
    problems = GATE.check_corpus_equivalence(root, cases)
    problems += GATE.run_python_mutations(root, cases)
    problems += GATE.check_no_judgment_authority(root)
    return problems


def _patch(path: Path, before: str, after: str) -> None:
    text = path.read_text(encoding="utf-8")
    assert text.count(before) == 1, f"anchor {before!r} is not unique in {path}"
    path.write_text(text.replace(before, after), encoding="utf-8")


class SyntheticRootTests(unittest.TestCase):
    def assertProblem(self, problems: list[str], needle: str) -> None:
        self.assertTrue(
            any(needle in problem for problem in problems),
            f"expected a problem mentioning {needle!r}; got {problems}",
        )

    def test_the_mirrored_root_is_clean(self):
        with TemporaryDirectory() as tmp:
            self.assertEqual(_run(_mirror_repo(tmp)), [])

    def test_a_python_output_that_drifts_from_the_corpus_is_reported(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            _patch(
                root / "scripts/relay_watchdog.py",
                'CANONICAL_SCHEMA_HEADER = "relay_obligation_canonical_v1"',
                'CANONICAL_SCHEMA_HEADER = "relay_obligation_canonical_v2"',
            )
            self.assertProblem(_run(root), "!= golden corpus")

    def test_a_corpus_thinned_below_the_floor_is_not_a_clean_run(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            manifest = root / "tests/fixtures/relay_obligation/cases.json"
            entries = json.loads(manifest.read_text(encoding="utf-8"))
            manifest.write_text(json.dumps(entries[:2]), encoding="utf-8")
            self.assertProblem(_run(root), "below the")

    def test_a_mutation_whose_anchor_vanished_is_reported_not_skipped(self):
        """The anchor is dissolved by an edit that changes NOTHING semantically
        — the same byte spelled `\\x0d` instead of `\\r`. So the corpus still
        matches and the ONLY complaint is that a declared mutation could not be
        applied, which is precisely the silent-skip the runner must never
        tolerate: a runner that applies nothing reports no survivors."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            name, before, _after = GATE.PYTHON_MUTATIONS[0]
            _patch(
                root / "scripts/relay_watchdog.py",
                before,
                before.replace('b"\\r"', 'b"\\x0d"'),
            )
            problems = _run(root)
            self.assertProblem(problems, f"{name!r} anchors on text appearing 0")

    def test_a_surviving_mutation_is_reported(self):
        """A mutation that applies cleanly and changes nothing observable must
        be named, not counted as killed.

        Constructed by adding one deliberately semantics-free mutation to the
        declared set — renaming a local — rather than by weakening the
        implementation: the claim under test is about the RUNNER's verdict, and
        a real rule broken in a way that happened to survive would be testing
        the rule instead."""
        original = GATE.PYTHON_MUTATIONS
        GATE.PYTHON_MUTATIONS = original + (
            (
                "no-op-local-rename",
                "    line_start = 0\n    while True:",
                "    line_start = 0\n    del_me = line_start\n    line_start = del_me\n    while True:",
            ),
        )
        try:
            with TemporaryDirectory() as tmp:
                problems = _run(_mirror_repo(tmp))
        finally:
            GATE.PYTHON_MUTATIONS = original
        self.assertProblem(problems, "'no-op-local-rename' SURVIVED")

    def test_a_consumer_outside_the_tree_is_reported(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            intruder = root / INTRUDER_REL
            intruder.write_text(
                "fn peek() {\n"
                "    let _ = super::reachability::obligation::CANONICAL_SCHEMA_HEADER;\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "grew a consumer")

    def test_a_consumer_reaching_the_tree_through_an_alias_is_reported(self):
        """The bypass a qualified-path scan cannot see.

        `use super::reachability as rx;` reads the tree without the substring
        `reachability::` ever appearing at the call site, so a gate that only
        knows qualified paths prints "no consumer" over a file that has one.
        The claim this gate makes is "no unsanctioned consumer, machine-
        checked", and an alias is a name."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            intruder = root / INTRUDER_REL
            intruder.write_text(
                "#[cfg(unix)]\n"
                "use super::reachability as rx;\n"
                "\n"
                "#[cfg(unix)]\n"
                "fn peek() -> &'static str {\n"
                "    rx::obligation::CANONICAL_SCHEMA_HEADER\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "grew a consumer")

    def test_a_sanctioned_consumer_naming_the_tree_in_a_use_item_is_reported(self):
        """#5071 T4-B4's sanction is for fully-qualified reads only.

        The same alias laundering as above, launched from INSIDE a sanctioned
        file: `use super::reachability as rx;` would let every later read hide
        behind `rx::`, and a `pub use` would republish the tree to files the
        scan then reports as clean. Both spellings live in `use` items, so a
        sanctioned file with a tree-naming `use` item must be reported even
        though its qualified reads are allowed.

        The tier is empty since #5071 T4-B6 promoted `health/snapshot.rs` to a
        judgment consumer, so a synthetic member holds the rule for the next
        divergence-only reader."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            sanctioned = SANCTIONED_PROBE_REL
            (root / sanctioned).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability as rx;\n"
                "\n"
                "#[cfg(unix)]\n"
                "fn observe() -> &'static str {\n"
                "    rx::divergence::RowCoordinateDivergence::Unknown.as_str()\n"
                "}\n",
                encoding="utf-8",
            )
            with mock.patch.object(
                GATE, "SANCTIONED_TREE_CONSUMERS", {sanctioned}
            ):
                problems = _run(root)
            self.assertProblem(problems, "fully-qualified paths only")

    def test_the_sanctioned_consumer_cannot_read_a_verdict(self):
        """The sanction names ONE read — the descriptive `divergence` record.

        A fully-qualified `verdict` read contains no `use` item, so the alias
        scan alone would pass it, and the sanction would be covering more than
        its slice landed. This is the mutation that keeps the tier honest: the
        qualified read that IS sanctioned stays clean, the sibling path is
        reported. Same synthetic member as above — the tier is empty since
        #5071 T4-B6."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            sanctioned = SANCTIONED_PROBE_REL
            (root / sanctioned).write_text(
                "#[cfg(unix)]\n"
                "fn observe() -> &'static str {\n"
                "    super::reachability::divergence::RowCoordinateDivergence"
                "::Unknown.as_str()\n"
                "}\n"
                "\n"
                "#[cfg(unix)]\n"
                "fn forbidden() {\n"
                "    let _ = super::reachability::verdict::ReachabilityVerdict"
                "::Unknown;\n"
                "}\n",
                encoding="utf-8",
            )
            with mock.patch.object(
                GATE, "SANCTIONED_TREE_CONSUMERS", {sanctioned}
            ):
                problems = _run(root)
            self.assertProblem(problems, "this sanction covers exactly")

    def test_a_stale_consumer_allowance_is_reported(self):
        """The allowance and the read it allows must move together, exactly
        as the module declaration and its allowance must: an allowed file
        that no longer names the tree is an allowance nobody would notice had
        gone stale. Pinned on a judgment consumer, the union's live tier."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            stale = sorted(GATE.JUDGMENT_TREE_CONSUMERS)[0]
            (root / stale).write_text(
                "// the composed verdict read was removed\n", encoding="utf-8"
            )
            self.assertProblem(_run(root), "the expected wiring is gone")

    def test_a_judgment_consumer_renaming_the_tree_in_a_use_item_is_reported(self):
        """#5071 T4-B6 lets a judgment consumer import the tree — not rename it.

        An `as` alias is the laundering half the wider allowance keeps refusing:
        every later read hides behind `rx::`, a spelling `names_tree` cannot
        follow."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability as rx;\n"
                "\n"
                "#[cfg(unix)]\n"
                "fn compose() {\n"
                "    let _ = rx::verdict::ReachabilityVerdict::Unknown;\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "may not rename it")

    def test_a_judgment_consumer_reexporting_the_tree_is_reported(self):
        """A `pub use` republishes a tree item to files holding no allowance —
        the second laundering shape the T4-B6 allowance keeps refusing."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "pub use super::reachability::verdict::ReachabilityVerdict;\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "may not re-export it")

    def test_a_judgment_consumer_publishing_an_imported_tree_type_is_reported(self):
        """The r1 two-step laundering, reproduced: import plainly, republish.

        Both halves pass the pre-r1 rules. The `use` names the tree but neither
        renames nor re-exports it, and the alias never spells `reachability`, so
        the sibling that imports `LaunderedRelayVerdictReport` is not a consumer
        by `names_tree` — the gate ran green on exactly this pair, with the
        crate compiling.
        """
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability::composite::RelayVerdictReport;\n"
                "\n"
                "pub(super) type LaunderedRelayVerdictReport = RelayVerdictReport;\n",
                encoding="utf-8",
            )
            sibling = root / INTRUDER_REL
            sibling.write_text(
                "use super::snapshot::LaunderedRelayVerdictReport;\n"
                "\n"
                "fn read(report: &LaunderedRelayVerdictReport) -> bool {\n"
                "    report.governs_health_polarity\n"
                "}\n",
                encoding="utf-8",
            )
            problems = _run(root)
            self.assertProblem(problems, "republishes `RelayVerdictReport`")
            # The sibling is still invisible to the consumer scan; that is the
            # point of killing the alias at its declaration rather than at the
            # reads it enables.
            self.assertNotIn(INTRUDER_REL, " ".join(problems))

    def test_a_judgment_consumer_reexporting_an_imported_tree_name_is_reported(self):
        """The same two-step bypass with a `pub use` as its second step.

        `pub use self::RelayVerdictProbe;` names no tree path at all, so the
        existing `pub use` rule — which only inspects items naming the tree —
        does not see it."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability::composite::RelayVerdictProbe;\n"
                "\n"
                "pub(super) use self::RelayVerdictProbe;\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "re-exports `RelayVerdictProbe`")

    def test_a_judgment_consumer_reexporting_an_imported_name_renamed_is_reported(self):
        """The renamed variant of the `pub use` second step — the r2 bypass.

        `pub use self::RelayVerdictProbe as Renamed;` binds `Renamed`, so a
        scan that intersects only the BOUND names with the imported ones comes
        back empty; what the item reads is `RelayVerdictProbe`, and the source
        side is where the republication happens. (rustc happens to refuse this
        exact spelling for a type imported at private visibility — E0364 — but
        the gate's claim is about source text, and visibilities move.)"""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability::composite::RelayVerdictProbe;\n"
                "\n"
                "pub(super) use self::RelayVerdictProbe as Renamed;\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "re-exports `RelayVerdictProbe`")

    def test_a_judgment_consumer_may_alias_an_imported_tree_type_privately(self):
        """The control the rule above must not swallow.

        A private `type` alias binds a shorter spelling inside one file. No
        sibling can import it, so nothing is republished and the file is still
        counted as the consumer it is. Rejecting this would make the gate a
        style rule about naming rather than a rule about reach."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability::composite::RelayVerdictReport;\n"
                "\n"
                "type Report = RelayVerdictReport;\n"
                "\n"
                "#[cfg(unix)]\n"
                "fn governs(report: &Report) -> bool {\n"
                "    report.governs_health_polarity\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertEqual(_run(root), [])

    def test_a_judgment_consumer_reading_the_ledger_is_reported(self):
        """T4-B6 unlocks four paths; the ledger is not one of them.

        A consumer that reads the ledger is rebuilding the classification
        outside `composite`, which is exactly the judgment-authority sprawl the
        four-path list exists to refuse."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "fn peek() {\n"
                "    let _ = super::reachability::ledger::snapshot();\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "belongs to the composition module")

    def test_a_judgment_consumer_may_import_and_read_the_verdict(self):
        """The allowance itself, next to its three refusals.

        A plain private `use` plus fully-qualified `verdict` and `divergence`
        reads is exactly the shape T4-B6 landed in `health/snapshot.rs`; if
        this scans dirty, the gate is refusing the slice it was amended to
        admit."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            (root / JUDGMENT_CONSUMER_REL).write_text(
                "#[cfg(unix)]\n"
                "use super::reachability::verdict::ReachabilityVerdict;\n"
                "\n"
                "#[cfg(unix)]\n"
                "fn compose() -> ReachabilityVerdict {\n"
                "    let _ = super::reachability::divergence"
                "::RowCoordinateDivergence::Unknown;\n"
                "    ReachabilityVerdict::Unknown\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertEqual(_run(root), [])

    def test_a_reexport_from_the_allowlisted_file_is_reported(self):
        """The same bypass one hop further out, and the reason the allowlist
        grants the right to DECLARE the module rather than to use it.

        The allowlisted file may name the tree, so it can republish a tree item
        under a name the consumer scan has no way to recognise — and then every
        other file in `src/` reads the tree while the gate reports zero
        consumers. Neither file below is reported by the qualified-path scan:
        one is allowlisted, and the other never spells the module."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            wiring = root / "src/services/discord/health.rs"
            wiring.write_text(
                wiring.read_text(encoding="utf-8")
                + "\npub(in crate::services::discord) use reachability::obligation"
                "::CANONICAL_SCHEMA_HEADER as CANON_HEADER;\n",
                encoding="utf-8",
            )
            consumer = root / INTRUDER_REL
            consumer.write_text(
                "#[cfg(unix)]\n"
                "fn peek() -> &'static str {\n"
                "    super::CANON_HEADER\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "allowlisted to DECLARE")

    def test_the_b2c_consumer_cannot_read_a_verdict(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            wiring = root / "src/services/discord/runtime_bootstrap/spawns.rs"
            wiring.write_text(
                wiring.read_text(encoding="utf-8")
                + "\nfn forbidden() { let _ = health::reachability::verdict"
                "::ReachabilityVerdict::Unknown; }\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "may name only direct")

    def test_the_b2c_consumer_cannot_alias_the_tree(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            wiring = root / "src/services/discord/runtime_bootstrap/spawns.rs"
            wiring.write_text(
                wiring.read_text(encoding="utf-8")
                + "\nuse crate::services::discord::health::reachability as rx;\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "may name only direct")

    def test_an_identifier_that_merely_contains_the_name_is_not_a_consumer(self):
        """The control for the two cases above.

        Widening the scan to the bare module name is what catches an alias, and
        it is also how a gate starts reporting every local called
        `reachability`. So the bare name is only looked for inside a `use` item
        (and inside the allowlisted file, which may name it in its `mod` line
        and nowhere else); a binding, a field, and a longer identifier that
        merely contains the substring are not readers of the tree."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            bystander = root / INTRUDER_REL
            bystander.write_text(
                "use std::time::Duration;\n"
                "\n"
                "struct Probe {\n"
                "    reachability: bool,\n"
                "}\n"
                "\n"
                "fn run_bot_spawn_reachability_observation() -> Duration {\n"
                "    let reachability = Duration::from_secs(1);\n"
                "    reachability\n"
                "}\n",
                encoding="utf-8",
            )
            self.assertEqual(_run(root), [])

    def test_prose_and_string_literals_are_not_consumers(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            bystander = root / INTRUDER_REL
            bystander.write_text(
                "// The reachability::obligation tree is deliberately not read here.\n"
                'const NOTE: &str = "reachability::obligation";\n',
                encoding="utf-8",
            )
            self.assertEqual(_run(root), [])

    def test_removing_the_module_declaration_is_reported(self):
        """The allowance and the thing it allows must move together: an
        allowlist naming a file that no longer references the tree is an
        allowance nobody would notice had gone stale."""
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            wiring = root / "src/services/discord/health.rs"
            wiring.write_text("// the module declaration was removed\n", encoding="utf-8")
            self.assertProblem(_run(root), "the expected wiring is gone")

    def test_a_bound_introduced_inside_the_tree_is_reported(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            target = root / "src/services/discord/health/reachability/obligation.rs"
            target.write_text(
                target.read_text(encoding="utf-8")
                + "\nconst WARN_BOUND_SECS: u64 = 60;\n"
                + "fn bound() -> u64 { let warn_bound = WARN_BOUND_SECS; warn_bound }\n",
                encoding="utf-8",
            )
            self.assertProblem(_run(root), "warn_bound")

    def test_a_bound_named_only_in_prose_is_not_reported(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            target = root / "src/services/discord/health/reachability/obligation.rs"
            target.write_text(
                "//! The warn_bound and fail_bound belong to T4-B6, not here.\n"
                + target.read_text(encoding="utf-8"),
                encoding="utf-8",
            )
            self.assertEqual(_run(root), [])

    def test_rust_preflight_reports_an_orphaned_mutation_signature(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            target = root / GATE.OBLIGATION_REL
            name, before, after = GATE.RUST_MUTATIONS[0]
            _patch(target, before, after)

            with mock.patch.object(GATE.subprocess, "run") as run:
                problems = GATE.run_rust_mutations(root)

            run.assert_not_called()
            self.assertProblem(problems, f"rust mutation signature(s): {name!r}")
            self.assertProblem(problems, "before anchor absent")
            self.assertProblem(problems, "after text appears 1 time(s)")
            self.assertProblem(problems, f"git diff -- {GATE.OBLIGATION_REL}")
            self.assertProblem(
                problems,
                "restore only the mutated line for each detected signature named above",
            )
            self.assertFalse(any("git checkout" in problem for problem in problems))

    def test_rust_preflight_allows_an_unrelated_worktree_edit(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            target = root / GATE.OBLIGATION_REL
            edited = "// unrelated worktree note\n" + target.read_text(encoding="utf-8")
            target.write_text(edited, encoding="utf-8")
            killed = subprocess.CompletedProcess([], 1, stdout="", stderr="")
            results = [
                subprocess.CompletedProcess([], 0, stdout="", stderr=""),
                *([killed] * len(GATE.RUST_MUTATIONS)),
            ]

            with mock.patch.object(GATE.subprocess, "run", side_effect=results):
                problems = GATE.run_rust_mutations(root)

            self.assertEqual(problems, [])
            self.assertEqual(target.read_text(encoding="utf-8"), edited)

    def test_an_absent_tree_is_not_a_clean_scan(self):
        with TemporaryDirectory() as tmp:
            root = _mirror_repo(tmp)
            shutil.rmtree(root / "src/services/discord/health/reachability")
            (root / "src/services/discord/health/reachability.rs").unlink()
            self.assertProblem(
                GATE.check_no_judgment_authority(root), "an empty scan is not a clean scan"
            )


class LiveRepoTests(unittest.TestCase):
    def test_the_checked_in_tree_passes_the_gate(self):
        self.assertEqual(GATE.main(["--repo-root", str(REPO_ROOT)]), 0)

    def test_the_gate_is_wired_into_ci_script_checks(self):
        body = CI_SCRIPT.read_text(encoding="utf-8")
        self.assertIn("scripts/check_reachability_canonical_equivalence.py", body)
        self.assertIn("tests.test_reachability_canonical_equivalence", body)

    def test_every_declared_rust_mutation_still_anchors_on_real_source(self):
        """A `--with-rust` run needs cargo and stays out of the fast lane, but a
        mutation anchored on text that no longer exists would silently stop
        being applied — and a runner that applies nothing reports no
        survivors."""
        source = (REPO_ROOT / GATE.OBLIGATION_REL).read_text(encoding="utf-8")
        for name, before, after in GATE.RUST_MUTATIONS:
            with self.subTest(mutation=name):
                self.assertEqual(
                    source.count(before),
                    1,
                    f"rust mutation {name!r} must anchor on exactly one site",
                )
                self.assertNotEqual(before, after)

    def test_every_declared_rust_mutation_satisfies_preflight_anchor_invariant(self):
        source = (REPO_ROOT / GATE.OBLIGATION_REL).read_text(encoding="utf-8")
        # This invariant is the premise of run_rust_mutations's orphan preflight predicate.
        for name, before, after in GATE.RUST_MUTATIONS:
            with self.subTest(mutation=name):
                self.assertGreaterEqual(source.count(before), 1)
                self.assertEqual(source.count(after), 0)

    def test_every_declared_python_mutation_still_anchors_on_real_source(self):
        source = (REPO_ROOT / GATE.WATCHDOG_REL).read_text(encoding="utf-8")
        for name, before, after in GATE.PYTHON_MUTATIONS:
            with self.subTest(mutation=name):
                self.assertEqual(source.count(before), 1)
                self.assertNotEqual(before, after)


if __name__ == "__main__":
    unittest.main()
