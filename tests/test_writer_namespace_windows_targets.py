from __future__ import annotations

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "scripts/ci/run-writer-namespace-windows-targets.sh"
IDS = (
    "services::writer_protocol::namespace::lexical::tests::sealed_portable_roots_normalize_exactly",
    "services::writer_protocol::namespace::lexical::tests::unsupported_prefixes_and_escape_components_fail_closed",
    "services::writer_protocol::namespace::lexical::tests::normalized_candidates_preserve_case_separators_and_root_boundaries",
)


class WriterNamespaceWindowsTargetsTests(unittest.TestCase):
    def run_fixture(self, *, active: bool = False, mode: str = "success", mutation: str = "") -> tuple[subprocess.CompletedProcess[str], int]:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src/services/writer_protocol/namespace").mkdir(parents=True)
            (root / "scripts").mkdir()
            (root / "bin").mkdir()
            protocol = "mod namespace;\n" if active else ""
            namespace = "mod lexical;\n" if active else ""
            lexical = "\n".join(f"fn {test_id.rsplit('::', 1)[1]}() {{}}" for test_id in IDS) + "\n"
            manifest = "\n".join(IDS) + "\n" if active else ""
            if mutation == "inactive_partial":
                manifest = IDS[0] + "\n"
            elif mutation == "missing_manifest":
                manifest = "\n".join(IDS[1:]) + "\n"
            elif mutation == "missing_lexical":
                lexical = None
            elif mutation == "duplicate_function":
                lexical += f"fn {IDS[0].rsplit('::', 1)[1]}() {{}}\n"
            elif mutation == "duplicate_manifest":
                manifest += IDS[0] + "\n"
            (root / "src/services/writer_protocol.rs").write_text(protocol)
            if active or mutation == "inactive_owner":
                (root / "src/services/writer_protocol/namespace.rs").write_text(namespace)
            if lexical is not None and active:
                (root / "src/services/writer_protocol/namespace/lexical.rs").write_text(lexical)
            (root / "scripts/lib_test_inventory_manifest.txt").write_text(manifest)
            fake = root / "bin/cargo"
            fake.write_text(
                "#!/usr/bin/env bash\n"
                "n=$(($(cat \"$FAKE_COUNT\" 2>/dev/null || echo 0)+1)); echo $n >\"$FAKE_COUNT\"\n"
                "case $n in 1) id=$FAKE_ID_1 ;; 2) id=$FAKE_ID_2 ;; 3) id=$FAKE_ID_3 ;; *) exit 9 ;; esac\n"
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
                " *) case $n in 1) tail='0 filtered out; finished in 0.00s' ;; 2) tail='137 filtered out; finished in 0.7s' ;; 3) tail='9 filtered out; finished in 12.345678s' ;; esac; echo 'Doc-tests agentdesk'; echo 'note: running 2 tests elsewhere'; echo 'running 1 test'; echo \"test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; $tail\" ;;\n"
                "esac\n"
                "[ \"$FAKE_MODE\" != nonzero ] || exit 7\n"
            )
            fake.chmod(0o755)
            count = root / "count"
            env = os.environ | {
                "AGENTDESK_REPO_ROOT": str(root), "FAKE_MODE": mode,
                "FAKE_COUNT": str(count), "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
                "FAKE_ID_1": IDS[0], "FAKE_ID_2": IDS[1], "FAKE_ID_3": IDS[2],
            }
            result = subprocess.run(["bash", str(RUNNER)], text=True, capture_output=True, env=env)
            calls = int(count.read_text()) if count.exists() else 0
            return result, calls

    def test_activation_and_identity_fail_closed(self) -> None:
        clean, calls = self.run_fixture()
        self.assertEqual((clean.returncode, calls), (0, 0))
        self.assertIn("NOT_APPLICABLE", clean.stdout)
        for mutation in ("inactive_partial", "inactive_owner"):
            self.assertNotEqual(self.run_fixture(mutation=mutation)[0].returncode, 0)
        for mutation in ("missing_lexical", "missing_manifest", "duplicate_function", "duplicate_manifest"):
            self.assertNotEqual(self.run_fixture(active=True, mutation=mutation)[0].returncode, 0)

    def test_exact_selection_reducer(self) -> None:
        for mode in ("zero", "ignored", "failed", "multi", "contradictory", "interleaved", "spoof", "measured", "malformed_time", "extra_tokens", "duplicate_result", "nonzero"):
            with self.subTest(mode=mode):
                self.assertNotEqual(self.run_fixture(active=True, mode=mode)[0].returncode, 0)
        success, calls = self.run_fixture(active=True)
        self.assertEqual((success.returncode, calls), (0, 3), success.stderr)
        self.assertEqual(success.stdout.count("WRITER_NAMESPACE_WINDOWS_TARGET PASS"), 3)


if __name__ == "__main__":
    unittest.main()
