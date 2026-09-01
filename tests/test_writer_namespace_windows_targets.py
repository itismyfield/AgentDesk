from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "scripts/ci/run-writer-namespace-windows-targets.sh"
LEXICAL_IDS = (
    "services::writer_protocol::namespace::lexical::tests::sealed_portable_roots_normalize_exactly",
    "services::writer_protocol::namespace::lexical::tests::unsupported_prefixes_and_escape_components_fail_closed",
    "services::writer_protocol::namespace::lexical::tests::normalized_candidates_preserve_case_separators_and_root_boundaries",
)
CATALOG_IDS = (
    "services::writer_protocol::namespace::catalog::tests::canonical_and_legacy_session_aliases_share_exact_authority_key",
    "services::writer_protocol::namespace::catalog::tests::sealed_roots_issue_only_exact_reviewed_artifact_bindings",
    "services::writer_protocol::namespace::catalog::tests::duplicate_and_overlapping_catalog_bindings_are_rejected_atomically",
    "services::writer_protocol::namespace::catalog::tests::catalog_bindings_are_deterministic_and_injective",
    "services::writer_protocol::namespace::catalog::tests::unknown_roots_and_artifacts_never_receive_fallback_identity",
)
IDS = LEXICAL_IDS + CATALOG_IDS


class WriterNamespaceWindowsTargetsTests(unittest.TestCase):
    def run_fixture(self, *, active: bool = False, catalog: bool = False, mode: str = "success", mutation: str = "", target: int = 0) -> tuple[subprocess.CompletedProcess[str], int]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src/services/writer_protocol/namespace").mkdir(parents=True)
            (root / "scripts").mkdir()
            (root / "bin").mkdir()
            protocol = "mod namespace;\n" if active else ""
            namespace = "mod lexical;\n" if active else ""
            namespace += "mod catalog;\n" if catalog else ""
            lexical = "\n".join(f"fn {test_id.rsplit('::', 1)[1]}() {{}}" for test_id in LEXICAL_IDS) + "\n"
            catalog_source = "\n".join(f"fn {test_id.rsplit('::', 1)[1]}() {{}}" for test_id in CATALOG_IDS) + "\n" if catalog else None
            manifest_ids = (LEXICAL_IDS if active else ()) + (CATALOG_IDS if catalog else ())
            manifest = "\n".join(manifest_ids) + ("\n" if manifest_ids else "")
            if mutation == "inactive_partial":
                manifest = LEXICAL_IDS[0] + "\n"
            elif mutation == "missing_manifest":
                manifest = "\n".join(LEXICAL_IDS[1:]) + "\n"
            elif mutation == "missing_lexical":
                lexical = None
            elif mutation == "duplicate_function":
                lexical += f"fn {LEXICAL_IDS[0].rsplit('::', 1)[1]}() {{}}\n"
            elif mutation == "duplicate_manifest":
                manifest += LEXICAL_IDS[0] + "\n"
            elif mutation in ("inactive_catalog_owner", "catalog_owner_only"): catalog_source = "\n"
            elif mutation == "inactive_catalog_manifest": manifest += CATALOG_IDS[target] + "\n"
            elif mutation == "duplicate_catalog_activation": namespace += "mod catalog;\n"
            elif mutation == "missing_catalog_owner": catalog_source = None
            elif mutation == "missing_catalog_manifest": manifest = manifest.replace(CATALOG_IDS[target] + "\n", "")
            elif mutation == "duplicate_catalog_manifest":
                manifest += CATALOG_IDS[target] + "\n"
            elif mutation == "missing_catalog_function":
                catalog_source = catalog_source.replace(f"fn {CATALOG_IDS[target].rsplit('::', 1)[1]}() {{}}\n", "")
            elif mutation == "duplicate_catalog_function":
                catalog_source += f"fn {CATALOG_IDS[target].rsplit('::', 1)[1]}() {{}}\n"
            elif mutation in ("wrong_catalog_owner", "duplicate_catalog_owner"):
                function = f"fn {CATALOG_IDS[target].rsplit('::', 1)[1]}() {{}}\n"
                lexical += function
                if mutation == "wrong_catalog_owner":
                    catalog_source = catalog_source.replace(function, "")
            (root / "src/services/writer_protocol.rs").write_text(protocol)
            if active or mutation == "inactive_owner":
                (root / "src/services/writer_protocol/namespace.rs").write_text(namespace)
            if lexical is not None and active:
                (root / "src/services/writer_protocol/namespace/lexical.rs").write_text(lexical)
            if catalog_source is not None:
                (root / "src/services/writer_protocol/namespace/catalog.rs").write_text(catalog_source)
            (root / "scripts/lib_test_inventory_manifest.txt").write_text(manifest)
            fake = root / "bin/cargo"
            fake.write_text(
                "#!/usr/bin/env bash\n"
                "n=$(($(cat \"$FAKE_COUNT\" 2>/dev/null || echo 0)+1)); echo $n >\"$FAKE_COUNT\"\n"
                "eval \"id=\${FAKE_ID_$n-}\"; [ -n \"$id\" ] || exit 9\n"
                "[ \"$*\" = \"test --lib $id -- --exact --test-threads=1\" ] || exit 9\n"
                "case \"$FAKE_MODE\" in\n"
                " zero) echo 'running 0 tests'; echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " ignored) echo 'running 1 test'; echo 'test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " failed) echo 'running 1 test'; echo 'failures:'; echo 'test x ... FAILED'; echo 'test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " multi) echo 'running 2 tests'; echo 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " contradictory) echo 'running 2 tests'; echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " interleaved) echo 'running 1 test'; echo 'Doc-tests noise'; echo 'running 0 tests'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " spoof) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 1 failed; 1 ignored; SPOOF' ;;\n"
                " measured) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 1 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " malformed_time) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0..00s' ;;\n"
                " extra_tokens) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s SPOOF' ;;\n"
                " duplicate_result) echo 'running 1 test'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s'; echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s' ;;\n"
                " *) case $n in 1) tail='0 filtered out; finished in 0.00s' ;; 2) tail='137 filtered out; finished in 0.7s' ;; *) tail='9 filtered out; finished in 12.345678s' ;; esac; echo 'Doc-tests agentdesk'; echo 'note: running 2 tests elsewhere'; echo 'running 1 test'; echo \"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; $tail\" ;;\n"
                "esac\n"
                "[ \"$FAKE_MODE\" != nonzero ] || exit 7\n"
            )
            fake.chmod(0o755)
            count = root / "count"
            env = os.environ | {
                "AGENTDESK_REPO_ROOT": str(root), "FAKE_MODE": mode,
                "FAKE_COUNT": str(count), "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
            }
            env.update({f"FAKE_ID_{index}": test_id for index, test_id in enumerate(IDS, 1)})
            result = subprocess.run(["bash", str(RUNNER)], text=True, capture_output=True, env=env)
            calls = int(count.read_text()) if count.exists() else 0
            return result, calls

    def test_catalog_activation_and_identity_fail_closed(self) -> None:
        clean, calls = self.run_fixture()
        self.assertEqual((clean.returncode, calls), (0, 0))
        self.assertIn("NOT_APPLICABLE", clean.stdout)
        for mutation in ("inactive_partial", "inactive_owner", "inactive_catalog_owner", "inactive_catalog_manifest"):
            self.assertNotEqual(self.run_fixture(mutation=mutation)[0].returncode, 0)
        for mutation in ("missing_lexical", "missing_manifest", "duplicate_function", "duplicate_manifest"):
            self.assertNotEqual(self.run_fixture(active=True, mutation=mutation)[0].returncode, 0)
        for mutation in ("catalog_owner_only", "inactive_catalog_manifest"):
            self.assertNotEqual(self.run_fixture(active=True, mutation=mutation)[0].returncode, 0)
        for mutation in ("duplicate_catalog_activation", "missing_catalog_owner"):
            self.assertNotEqual(self.run_fixture(active=True, catalog=True, mutation=mutation)[0].returncode, 0)
        for mutation in ("missing_catalog_manifest", "duplicate_catalog_manifest", "missing_catalog_function", "duplicate_catalog_function"):
            for target in range(len(CATALOG_IDS)):
                with self.subTest(mutation=mutation, target=target):
                    self.assertNotEqual(self.run_fixture(active=True, catalog=True, mutation=mutation, target=target)[0].returncode, 0)

    def test_catalog_owner_associations_are_exact(self) -> None:
        for mutation in ("wrong_catalog_owner", "duplicate_catalog_owner"):
            for target in range(len(CATALOG_IDS)):
                with self.subTest(mutation=mutation, target=target):
                    self.assertNotEqual(self.run_fixture(active=True, catalog=True, mutation=mutation, target=target)[0].returncode, 0)

    def test_exact_eight_selection_reducer(self) -> None:
        lexical, calls = self.run_fixture(active=True)
        self.assertEqual((lexical.returncode, calls), (0, 3), lexical.stderr)
        self.assertNotIn("namespace::catalog::", lexical.stdout)
        success, calls = self.run_fixture(active=True, catalog=True)
        self.assertEqual((success.returncode, calls), (0, 8), success.stderr)
        records = [line for line in success.stdout.splitlines() if line.startswith("WRITER_NAMESPACE_WINDOWS_TARGET PASS")]
        expected = [f"WRITER_NAMESPACE_WINDOWS_TARGET PASS id={test_id} selected=1 passed=1" for test_id in IDS]
        self.assertEqual(records, expected)

    def test_exact_eight_result_grammar_fail_closed(self) -> None:
        for mode in ("zero", "ignored", "failed", "multi", "contradictory", "interleaved", "spoof", "measured", "malformed_time", "extra_tokens", "duplicate_result", "nonzero"):
            with self.subTest(mode=mode):
                self.assertNotEqual(self.run_fixture(active=True, catalog=True, mode=mode)[0].returncode, 0)


if __name__ == "__main__":
    unittest.main()
