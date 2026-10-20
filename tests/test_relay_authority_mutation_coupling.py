from __future__ import annotations

import subprocess
import unittest

from tests import test_relay_authority_mutations as fixtures


class RelayAuthorityMutationCouplingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = fixtures.RelayAuthorityMutationScriptTests()
        self.addCleanup(self.fixture.doCleanups)

    def renamed_fixture(self):
        root = self.fixture.copy_fixture()
        # A bin name precedes the package name so extraction must respect the table.
        (root / "Cargo.toml").write_text(
            '[[bin]]\nname = "agentdesk"\n[package]\nname = "relay-oracle-fixture"\n'
        )
        return root

    def test_renamed_package_compilation_is_killed(self) -> None:
        root = self.renamed_fixture()
        cargo = self.fixture.write_fake_cargo(root)
        cargo.write_text(cargo.read_text().replace("agentdesk", "relay-oracle-fixture"))

        result = self.fixture.run_script_with_fake_cargo(root, cargo)

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(result.stdout.count("CACHE_PROOF "), fixtures.MUTATION_COUNT)
        self.assertEqual(result.stdout.count("status=KILLED"), fixtures.MUTATION_COUNT)
        self.fixture.assert_sources_restored(self, root)

    def test_renamed_package_fresh_marker_invalidates_cache_proof(self) -> None:
        root = self.renamed_fixture()
        cargo = self.fixture.write_fake_cargo(root)
        cargo.write_text(cargo.read_text().replace(
            "Compiling agentdesk v0.1.0 (fake)",
            "Compiling relay-oracle-fixture v0.1.0 (fake)\\n"
            "   Fresh relay-oracle-fixture v0.1.0 (fake)",
        ))

        result = self.fixture.run_script_with_fake_cargo(root, cargo)

        self.assertEqual(result.returncode, 96, result.stdout + result.stderr)
        self.assertIn("compile_count=1", result.stderr)
        self.assertNotIn("MUTATION_SUMMARY", result.stdout)
        self.fixture.assert_sources_restored(self, root)

    def test_renamed_package_build_failure_is_build_broken(self) -> None:
        root = self.renamed_fixture()
        cargo = self.fixture.write_fake_cargo(root)
        cargo.write_text(
            "#!/bin/bash\n" + fixtures.BUILD_BROKEN_RUNNER.replace(
                "agentdesk", "relay-oracle-fixture"
            )
        )

        result = self.fixture.run_script_with_fake_cargo(root, cargo)

        self.assertEqual(result.returncode, 95, result.stdout + result.stderr)
        self.assertIn("CACHE_PROOF ", result.stdout)
        self.assertIn("compile_ok=no", result.stdout)
        self.assertIn("status=BUILD-BROKEN", result.stderr)
        self.assertNotIn("status=KILLED", result.stdout)
        self.assertNotIn("MUTATION_SUMMARY", result.stdout)
        self.fixture.assert_sources_restored(self, root)

    def test_unreadable_or_empty_package_name_fails_before_mutations(self) -> None:
        for manifest in (None, "[package]\n", '[package]\nname = ""\n',
                         '[package]\nname = "   "\n', '[package]\nname = 123\n',
                         '[package\nname = "agentdesk"\n'):
            with self.subTest(manifest=manifest):
                root = self.fixture.copy_fixture()
                path = root / "Cargo.toml"
                if manifest is None:
                    path.unlink()
                else:
                    path.write_text(manifest)
                runner = self.fixture.write_runner(root, fixtures.KILLED_RUNNER)

                result = self.fixture.run_script(root, runner)

                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("could not read Cargo.toml [package].name", result.stderr)
                self.assertNotIn("MUTATION_SHARD", result.stdout)
                self.assertFalse((root / "target/relay-authority-mutations.lock").exists())
                self.fixture.assert_sources_restored(self, root)

    def test_m8_anchor_matches_once_and_rejects_a_renamed_local(self) -> None:
        root = self.fixture.copy_fixture()
        script = (root / fixtures.MUTATION_SCRIPT).read_text()
        registration = script.split('  M8 "$TERMINAL_HANDOFF" \\\n', 1)[1].split(
            "\nregister_mutation", 1
        )[0]
        expected = subprocess.run(
            ["/bin/bash", "-euc", "register_mutation() { printf '%s' \"$1\"; }\n"
             + "register_mutation " + registration],
            check=True, text=True, capture_output=True,
        ).stdout
        source = root / fixtures.TERMINAL_HANDOFF
        original = source.read_text()
        self.assertEqual(original.count(expected), 1)
        self.assertIn("terminal_not_delivered", expected)
        renamed = original.replace("terminal_not_delivered", "terminal_delivery_missing")
        self.assertEqual(renamed.count(expected), 0)
        source.write_text(renamed)
        runner = self.fixture.write_runner(root, 'echo UNEXPECTED_RUNNER >&2\n'
                                           + fixtures.KILLED_RUNNER)

        result = self.fixture.run_script(root, runner, {
            "RELAY_AUTHORITY_MUTATION_SHARD_INDEX": "2",
            "RELAY_AUTHORITY_MUTATION_SHARD_TOTAL": "7",
        })

        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("matches=0", result.stderr)
        self.assertNotIn("UNEXPECTED_RUNNER", result.stderr)
        self.assertNotIn("MUTATION_SUMMARY", result.stdout)
        self.assertEqual(source.read_text(), renamed)


if __name__ == "__main__":
    unittest.main()
