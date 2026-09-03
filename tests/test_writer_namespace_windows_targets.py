from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from dataclasses import FrozenInstanceError, dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import exact_rust_test_proof as proof
from check_test_target_integrity import render_lib_inventory_manifest

RUNNER = ROOT / "scripts/ci/run-writer-namespace-windows-targets.sh"
ENGINE = ROOT / "scripts/exact_rust_test_proof.py"
INVENTORY = ROOT / "scripts/check_test_target_integrity.py"
FAMILY = "services::writer_protocol::namespace::lexical::tests"
IDS = (
    f"{FAMILY}::sealed_portable_roots_normalize_exactly",
    f"{FAMILY}::unsupported_prefixes_and_escape_components_fail_closed",
    f"{FAMILY}::normalized_candidates_preserve_case_separators_and_root_boundaries",
)
CARGO_CALLS = tuple(f"test --lib {test_id} -- --exact --test-threads=1" for test_id in IDS)
OPTIONAL_FAMILY = "services::writer_protocol::namespace::neutral::tests"
OPTIONAL_ID = f"{OPTIONAL_FAMILY}::synthetic_optional_owner_proof"


@dataclass(frozen=True)
class OwnerFixture:
    key: str
    module: str
    source: str
    family: str
    policy: str
    ids: tuple[str, ...]


@dataclass(frozen=True)
class Outcome:
    result: subprocess.CompletedProcess[str]
    calls: tuple[str, ...]
    temp_before: tuple[str, ...]
    temp_after: tuple[str, ...]
    plan_before: str
    plan_after: str


LEXICAL = OwnerFixture("lexical", "lexical", "src/services/writer_protocol/namespace/lexical.rs", FAMILY, "required", IDS)
OPTIONAL = OwnerFixture("neutral", "neutral", "src/services/writer_protocol/namespace/neutral.rs", OPTIONAL_FAMILY, "optional", (OPTIONAL_ID,))


class WriterNamespaceWindowsTargetsTests(unittest.TestCase):
    maxDiff = None

    def command(self, root: Path, owners: tuple[OwnerFixture, ...]) -> list[str]:
        command = [
            sys.executable, str(root / "scripts/exact_rust_test_proof.py"), "run",
            "--repo-root", str(root), "--manifest", "scripts/lib_test_inventory_manifest.txt",
            "--pass-prefix", "WRITER_NAMESPACE_WINDOWS_TARGET",
            "--gate", "writer_namespace", "src/services/writer_protocol.rs", "namespace",
            "src/services/writer_protocol/namespace.rs", "optional",
        ]
        for owner in owners:
            command.extend((
                "--owner", owner.key, "writer_namespace",
                "src/services/writer_protocol/namespace.rs", owner.module,
                owner.source, owner.family, owner.policy,
            ))
            for test_id in owner.ids:
                command.extend(("--owner-id", owner.key, test_id))
        return command

    def write_graph(
        self,
        root: Path,
        *,
        active: bool,
        optional_active: bool,
        mutation: str,
    ) -> None:
        source = root / "src/services/writer_protocol/namespace"
        source.mkdir(parents=True)
        (root / "src/lib.rs").write_text("mod services;\n", encoding="utf-8")
        (root / "src/services.rs").write_text("mod writer_protocol;\n", encoding="utf-8")
        protocol = "mod namespace;\n" if active else ""
        namespace = "mod lexical;\n" if active else ""
        lexical = "#[cfg(test)]\nmod tests {\n" + "".join(f"#[test]\nfn {test_id.rsplit('::', 1)[1]}() {{}}\n" for test_id in IDS) + "}\n"
        manifest_ids = set(IDS if active else ())
        if optional_active:
            namespace += "mod neutral;\n"
            (source / "neutral.rs").write_text("#[cfg(test)]\nmod tests { #[test] fn synthetic_optional_owner_proof() {} }\n", encoding="utf-8")
            manifest_ids.add(OPTIONAL_ID)
        if mutation == "inactive_residue":
            namespace = "mod lexical;\n"
        elif mutation == "duplicate_gate":
            protocol += "mod namespace;\n"
        elif mutation == "duplicate_owner":
            namespace += "mod lexical;\n"
        elif mutation == "redirected_owner":
            namespace = '#[path = "lexical.rs"]\nmod lexical;\n'
        elif mutation == "missing_owner":
            namespace = ""
        elif mutation == "extra_same_family":
            lexical = lexical[:-2] + "#[test]\nfn unregistered_same_family() {}\n}\n"
            manifest_ids.add(f"{FAMILY}::unregistered_same_family")
        elif mutation == "manifest_neighbor":
            manifest_ids.remove(IDS[0])
            manifest_ids.add(IDS[0] + "_neighbor")
        elif mutation == "manifest_container":
            manifest_ids.remove(IDS[0])
            manifest_ids.add("prefix_" + IDS[0])
        elif mutation == "wrong_source":
            (source / "lexical_alias.rs").write_text("// not the canonical owner\n", encoding="utf-8")
        elif mutation == "unrelated_family":
            namespace += "mod unrelated;\n"
            other = "services::writer_protocol::namespace::unrelated::tests::allowed"
            (source / "unrelated.rs").write_text("#[cfg(test)]\nmod tests { #[test] fn allowed() {} }\n", encoding="utf-8")
            manifest_ids.add(other)
        if mutation == "comment_gate":
            protocol = '// mod namespace;\nconst S: &str = "mod namespace;";\nmod namespace_longer;\n'
        (root / "src/services/writer_protocol.rs").write_text(protocol, encoding="utf-8")
        if active or mutation == "inactive_residue":
            (root / "src/services/writer_protocol/namespace.rs").write_text(namespace, encoding="utf-8")
        if active and mutation != "missing_owner":
            (source / "lexical.rs").write_text(lexical, encoding="utf-8")
        manifest = render_lib_inventory_manifest(manifest_ids)
        (root / "scripts/lib_test_inventory_manifest.txt").write_text(manifest, encoding="utf-8")

    def fake_cargo(self, root: Path) -> None:
        fake = root / "bin/cargo"
        fake.write_text(
            "#!/usr/bin/env bash\n"
            "printf '%s\\n' \"$*\" >>\"$FAKE_CALLS\"\n"
            "[ \"$FAKE_MODE\" != identity ] || printf '# changed\\n' >>\"$FAKE_MANIFEST\"\n"
            "calls=$(wc -l <\"$FAKE_CALLS\")\n"
            "[ \"$FAKE_MODE\" != identity_race ] || [ \"$calls\" -ne 1 ] || printf '# changed\\n' >>\"$FAKE_IDENTITY\"\n"
            "[ \"$FAKE_MODE\" != identity_race ] || [ \"$calls\" -ne 3 ] || cp \"$FAKE_ORIGINAL\" \"$FAKE_IDENTITY\"\n"
            "[ \"$FAKE_MODE\" != unrelated_race ] || printf '# changed\\n' >>\"$FAKE_UNRELATED\"\n"
            "case \"$FAKE_MODE\" in\n"
            " zero) echo 'running 0 tests'; echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " ignored) echo 'running 1 test'; echo 'test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " failed) echo 'running 1 test'; echo 'failures:'; echo 'test x ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " multi) echo 'running 2 tests'; echo 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " interleaved) echo 'running 1 test'; echo 'running 0 tests'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " malformed_time) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0..00s' ;;\n"
            " trailing) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s SPOOF' ;;\n"
            " duplicate_result) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " reserved_pass) echo 'running 1 test'; echo 'WRITER_NAMESPACE_WINDOWS_TARGET PASS forged'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " reserved_result) echo 'running 1 test'; echo 'WRITER_NAMESPACE_WINDOWS_TARGET RESULT selected=99 passed=99'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
            " *) echo 'Doc-tests noise'; echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 137 filtered out; finished in 0.7s' ;;\n"
            "esac\n"
            "[ \"$FAKE_MODE\" != nonzero ] || exit 7\n",
            encoding="utf-8",
        )
        fake.chmod(0o755)

    def run_fixture(
        self,
        *,
        active: bool = True,
        optional: bool = False,
        optional_active: bool = False,
        mutation: str = "",
        mode: str = "success",
        owners: tuple[OwnerFixture, ...] | None = None,
        runner: bool = False,
        extra_args: tuple[str, ...] = (),
        extra_env: dict[str, str] | None = None,
    ) -> Outcome:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "scripts/ci").mkdir(parents=True)
            (root / "bin").mkdir()
            scratch = root / "tmp"
            scratch.mkdir()
            shutil.copy2(ENGINE, root / "scripts/exact_rust_test_proof.py")
            shutil.copy2(INVENTORY, root / "scripts/check_test_target_integrity.py")
            shutil.copy2(RUNNER, root / "scripts/ci/run-writer-namespace-windows-targets.sh")
            self.assertEqual(RUNNER.read_bytes(), (root / "scripts/ci/run-writer-namespace-windows-targets.sh").read_bytes())
            self.write_graph(root, active=active, optional_active=optional_active, mutation=mutation)
            governed = root / LEXICAL.source
            original = root / "lexical.original"
            if governed.exists():
                shutil.copy2(governed, original)
            unrelated = root / "README.probe"
            unrelated.write_text("unrelated\n", encoding="utf-8")
            self.fake_cargo(root)
            selected_owners = owners or ((LEXICAL, OPTIONAL) if optional else (LEXICAL,))
            command = ["bash", str(root / "scripts/ci/run-writer-namespace-windows-targets.sh")] if runner else self.command(root, selected_owners)
            command.extend(extra_args)
            parsed = proof.parse_cli(self.command(root, selected_owners)[2:])
            before = proof._plan_digest(parsed)
            calls_path = root / "calls"
            env = os.environ | {
                "FAKE_CALLS": str(calls_path),
                "FAKE_MODE": mode,
                "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                "TMPDIR": str(scratch),
                "FAKE_MANIFEST": str(root / "scripts/lib_test_inventory_manifest.txt"),
                "FAKE_IDENTITY": str(governed),
                "FAKE_ORIGINAL": str(original),
                "FAKE_UNRELATED": str(unrelated),
            } | (extra_env or {})
            if not extra_env or "AGENTDESK_REPO_ROOT" not in extra_env:
                env.pop("AGENTDESK_REPO_ROOT", None)
            temp_before = tuple(item.name for item in scratch.iterdir())
            result = subprocess.run(command, text=True, capture_output=True, env=env, check=False)
            calls = tuple(calls_path.read_text(encoding="utf-8").splitlines()) if calls_path.exists() else ()
            temp_after = tuple(item.name for item in scratch.iterdir())
            after = proof._plan_digest(parsed)
            return Outcome(result, calls, temp_before, temp_after, before, after)

    def assert_structural_failure(self, outcome: Outcome, target: str) -> None:
        self.assertNotEqual(outcome.result.returncode, 0, target)
        self.assertIn(target, outcome.result.stderr)
        self.assertEqual(outcome.calls, ())
        self.assertEqual(outcome.plan_before, outcome.plan_after)
        self.assertEqual(outcome.temp_before, ())
        self.assertEqual(outcome.temp_after, ())

    def test_caller_selected_root_is_rejected(self) -> None:
        for value in ("", "/caller/selected"):
            with self.subTest(value=value):
                outcome = self.run_fixture(runner=True, extra_env={"AGENTDESK_REPO_ROOT": value})
                self.assertNotEqual(outcome.result.returncode, 0)
                self.assertIn("AGENTDESK_REPO_ROOT", outcome.result.stderr)
                self.assertEqual(outcome.calls, ())

    def test_wrapper_selects_only_supported_path_interpreter(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            bindir = Path(temp)
            (bindir / "python").symlink_to(sys.executable)
            (bindir / "dirname").symlink_to(shutil.which("dirname"))
            env = os.environ | {"PATH": str(bindir), "PYTHON": "forbidden", "AGENTDESK_PYTHON": "forbidden"}
            result = subprocess.run(["/bin/bash", str(RUNNER)], text=True, capture_output=True, env=env, check=False)
            self.assertNotEqual(result.returncode, 127, result.stderr)
            (bindir / "python").unlink()
            result = subprocess.run(["/bin/bash", str(RUNNER)], text=True, capture_output=True, env=env, check=False)
            self.assertEqual(result.returncode, 86)
            self.assertIn("requires python3 or python", result.stderr)
            (bindir / "python").write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
            (bindir / "python").chmod(0o755)
            result = subprocess.run(["/bin/bash", str(RUNNER)], text=True, capture_output=True, env=env, check=False)
            self.assertEqual(result.returncode, 87)
            self.assertIn("must be >= 3.11", result.stderr)

    def test_only_unconditional_run_is_exposed(self) -> None:
        rejected = self.run_fixture(extra_args=("--validate-only",))
        self.assertNotEqual(rejected.result.returncode, 0)
        self.assertEqual(rejected.calls, ())
        executed = self.run_fixture(extra_env={"EXACT_RUST_TEST_PROOF_MODE": "skip"})
        self.assertEqual(executed.result.returncode, 0, executed.result.stderr)
        self.assertEqual(executed.calls, CARGO_CALLS)

    def test_token_activation_and_inactive_residue_fail_closed(self) -> None:
        inactive = self.run_fixture(active=False)
        self.assertEqual(inactive.result.returncode, 0, inactive.result.stderr)
        self.assertIn("NOT_APPLICABLE", inactive.result.stdout)
        self.assertEqual(inactive.calls, ())
        optional_inactive = self.run_fixture(active=False, optional=True)
        self.assertIn(f"{proof.ABSENT_PREFIX} owner=neutral selected=0 passed=0 absent_selected=0 temp=empty", optional_inactive.result.stdout)
        spoof = self.run_fixture(active=False, mutation="comment_gate")
        self.assertEqual(spoof.result.returncode, 0, spoof.result.stderr)
        self.assertIn("NOT_APPLICABLE", spoof.result.stdout)
        self.assertEqual(spoof.calls, ())
        self.assert_structural_failure(self.run_fixture(active=False, mutation="inactive_residue"), "gate writer_namespace")
        self.assert_structural_failure(self.run_fixture(mutation="duplicate_gate"), "gate writer_namespace")

    def test_owner_family_closure_source_and_manifest_identity(self) -> None:
        cases = (
            ("duplicate_owner", (LEXICAL,), "owner lexical"),
            ("redirected_owner", (LEXICAL,), "module lexical"),
            ("missing_owner", (LEXICAL,), "owner lexical"),
            ("extra_same_family", (LEXICAL,), "owner lexical"),
            ("manifest_neighbor", (LEXICAL,), "owner lexical"),
            ("manifest_container", (LEXICAL,), "owner lexical"),
            ("wrong_source", (OwnerFixture("lexical", "lexical", "src/services/writer_protocol/namespace/lexical_alias.rs", FAMILY, "required", IDS),), "source site"),
            ("", (OwnerFixture("lexical", "lexical", LEXICAL.source, "services::wrong::tests", "required", IDS),), "does not belong exactly"),
        )
        for mutation, owners, target in cases:
            with self.subTest(mutation=mutation, target=target):
                self.assert_structural_failure(self.run_fixture(mutation=mutation, owners=owners), target)
        unrelated = self.run_fixture(mutation="unrelated_family")
        self.assertEqual(unrelated.result.returncode, 0, unrelated.result.stderr)
        self.assertEqual(unrelated.calls, CARGO_CALLS)

    def test_owner_sets_are_frozen_globally_disjoint_and_transactional(self) -> None:
        with self.assertRaises(FrozenInstanceError):
            LEXICAL.policy = "optional"  # type: ignore[misc]
        gate = proof.GateSpec("gate", "parent.rs", "child", "child.rs", "optional")
        bad_owner = proof.OwnerSpec(
            "bad", "gate", "child.rs", "bad", "bad.rs", FAMILY, "required",
            ("services::wrong::tests::compensated",),
        )
        plan = proof.ProofPlan(ROOT, "manifest", "PROOF", gate, (bad_owner,))
        with self.assertRaisesRegex(proof.ProofError, "does not belong exactly"):
            proof._validate_spec(plan)
        collision = OwnerFixture("other", "other", "src/services/writer_protocol/namespace/other.rs", "services::other::tests", "optional", ("services::other::tests::sealed_portable_roots_normalize_exactly",))
        self.assert_structural_failure(self.run_fixture(owners=(LEXICAL, collision)), "collision")
        failed_last = OwnerFixture("last", "last", "src/services/writer_protocol/namespace/last.rs", "services::last::tests", "required", ("services::last::tests::last",))
        outcome = self.run_fixture(owners=(LEXICAL, failed_last))
        self.assert_structural_failure(outcome, "owner last")

    def test_exact_selection_reducer_reserved_output_and_cleanup(self) -> None:
        modes = ("zero", "ignored", "failed", "multi", "interleaved", "malformed_time", "trailing", "duplicate_result", "reserved_pass", "reserved_result", "nonzero")
        for mode in modes:
            with self.subTest(mode=mode):
                outcome = self.run_fixture(mode=mode)
                self.assertNotEqual(outcome.result.returncode, 0)
                self.assertEqual(outcome.calls, (CARGO_CALLS[0],))
                self.assertEqual(outcome.temp_before, ())
                self.assertEqual(outcome.temp_after, ())
                self.assertNotIn(" PASS id=", outcome.result.stdout)
        changed = self.run_fixture(mode="identity")
        self.assertEqual(changed.result.returncode, 1)
        self.assertNotIn("RESULT selected=", changed.result.stdout)
        self.assertIn("sealed identity changed after credited child", changed.result.stderr)
        raced = self.run_fixture(mode="identity_race")
        self.assertEqual(raced.result.returncode, 1)
        self.assertEqual(raced.calls, (CARGO_CALLS[0],))
        self.assertNotIn("RESULT selected=", raced.result.stdout)
        self.assertIn("sealed identity changed after credited child", raced.result.stderr)
        unrelated = self.run_fixture(mode="unrelated_race")
        self.assertEqual(unrelated.result.returncode, 0, unrelated.result.stderr)
        self.assertEqual(unrelated.calls, CARGO_CALLS)

    def test_every_registered_optional_owner_absent_clean(self) -> None:
        registered = (OPTIONAL,)
        self.assertGreater(len(registered), 0)
        collected = 0
        for optional in registered:
            collected += 1
            with self.subTest(owner=optional.key):
                outcome = self.run_fixture(optional=True)
                self.assertEqual(outcome.result.returncode, 0, outcome.result.stderr)
                self.assertEqual(outcome.calls, CARGO_CALLS)
                self.assertEqual(outcome.result.stdout.count(" PASS id="), 3)
                self.assertNotIn(OPTIONAL_ID, outcome.result.stdout)
                self.assertIn("RESULT selected=3 passed=3", outcome.result.stdout)
                self.assertIn(f"{proof.ABSENT_PREFIX} owner={optional.key} selected=3 passed=3 absent_selected=0 temp=empty", outcome.result.stdout)
                self.assertEqual(outcome.plan_before, outcome.plan_after)
                self.assertEqual(outcome.temp_before, ())
                self.assertEqual(outcome.temp_after, ())
        self.assertEqual(collected, len(registered))

    def test_optional_active_and_real_runner_preserve_exact_order(self) -> None:
        active_optional = self.run_fixture(optional=True, optional_active=True)
        self.assertEqual(active_optional.result.returncode, 0, active_optional.result.stderr)
        self.assertEqual(active_optional.calls, CARGO_CALLS + (f"test --lib {OPTIONAL_ID} -- --exact --test-threads=1",))
        real = self.run_fixture(runner=True)
        self.assertEqual(real.result.returncode, 0, real.result.stderr)
        self.assertEqual(real.calls, CARGO_CALLS)
        passes = [line for line in real.result.stdout.splitlines() if " PASS id=" in line]
        self.assertEqual(passes, [f"WRITER_NAMESPACE_WINDOWS_TARGET PASS id={test_id} selected=1 passed=1" for test_id in IDS])
        self.assertEqual(real.result.stdout.count("RESULT selected=3 passed=3"), 1)
        runner_text = RUNNER.read_text(encoding="utf-8")
        self.assertIn("readonly -a lexical_ids", runner_text)
        self.assertEqual([test_id for test_id in IDS if runner_text.count(test_id) == 1], list(IDS))


if __name__ == "__main__":
    unittest.main()
