"""Tests for the immutable compiler-generated Rust test baseline (#4906)."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts/generate_compiler_test_manifest.py"
_spec = importlib.util.spec_from_file_location("generate_compiler_test_manifest", SCRIPT)
assert _spec and _spec.loader
manifest = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = manifest
_spec.loader.exec_module(manifest)


class CompilerTestManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path("/repo")
        self.package_id = "path+file:///repo#agentdesk@0.1.2"
        self.metadata = {
            "packages": [
                {
                    "id": self.package_id,
                    "name": "agentdesk",
                    "version": "0.1.2",
                    "manifest_path": "/repo/Cargo.toml",
                    "targets": [
                        {
                            "name": "agentdesk",
                            "kind": ["lib"],
                            "doctest": True,
                        },
                        {
                            "name": "agentdesk",
                            "kind": ["bin"],
                            "doctest": False,
                        },
                    ],
                }
            ]
        }
        self.messages = [
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "package_id": self.package_id,
                    "target": {"name": "agentdesk", "kind": ["lib"]},
                    "profile": {"test": True},
                    "executable": "/repo/target/debug/deps/agentdesk-lib",
                }
            ),
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "package_id": self.package_id,
                    "target": {"name": "agentdesk", "kind": ["bin"]},
                    "profile": {"test": True},
                    "executable": "/repo/target/debug/deps/agentdesk-bin",
                }
            ),
        ]
        self.listings = {
            "Cargo.toml#agentdesk::lib::agentdesk": (
                "services::relay::tests::stable_case: test\n"
                "services::relay::tests::generated_case: test\n"
                "2 tests, 0 benchmarks\n"
            ),
            "Cargo.toml#agentdesk::bin::agentdesk": "0 tests, 0 benchmarks\n",
        }
        self.source = {
            "services::relay::tests": {
                "services::relay::tests::stable_case",
            }
        }

    def build(self) -> dict[str, object]:
        return manifest.build_manifest(
            self.metadata,
            self.messages,
            self.listings,
            self.source,
            self.root,
        )

    def test_stable_ids_ignore_compiler_message_listing_order_and_version(self) -> None:
        first = self.build()
        shuffled = manifest.build_manifest(
            self.metadata,
            reversed(self.messages),
            {
                key: "\n".join(reversed(value.splitlines())) + "\n"
                for key, value in reversed(tuple(self.listings.items()))
            },
            self.source,
            self.root,
        )

        self.assertEqual(
            manifest.render_manifest(first), manifest.render_manifest(shuffled)
        )
        upgraded_metadata = json.loads(json.dumps(self.metadata))
        upgraded_metadata["packages"][0]["version"] = "9.9.9"
        version_changed = manifest.build_manifest(
            upgraded_metadata,
            self.messages,
            self.listings,
            self.source,
            self.root,
        )
        self.assertEqual(
            [record["id"] for record in first["tests"]],
            [record["id"] for record in version_changed["tests"]],
        )
        ids = [record["id"] for record in first["tests"]]
        self.assertEqual(ids, sorted(ids))
        self.assertTrue(all(":" in identity for identity in ids))

    def test_records_source_and_unknown_generated_ownership_explicitly(self) -> None:
        generated = {
            record["test_name"]: record["owner"] for record in self.build()["tests"]
        }

        self.assertEqual(
            generated["services::relay::tests::stable_case"]["kind"], "source"
        )
        self.assertEqual(
            generated["services::relay::tests::generated_case"],
            {
                "kind": "generated_or_unresolved",
                "resolution": "no_unambiguous_source_item",
                "module": None,
                "source_test_id": None,
            },
        )

    def test_separates_doctests_and_marks_zero_test_binary_non_vacuous_false(self) -> None:
        result = self.build()

        self.assertFalse(result["doctests"]["included"])
        self.assertEqual(len(result["doctests"]["exclusions"]), 1)
        targets = {target["target_id"]: target for target in result["targets"]}
        binary = targets["Cargo.toml#agentdesk::bin::agentdesk"]
        self.assertEqual(binary["test_count"], 0)
        self.assertFalse(binary["non_vacuous"])

    def test_manifest_records_platform_and_toolchain_scope(self) -> None:
        result = manifest.build_manifest(
            self.metadata,
            self.messages,
            self.listings,
            self.source,
            self.root,
            environment={
                "host": "linux",
                "rustc_release": "1.94.1",
                "rustc_host": "x86_64-unknown-linux-gnu",
            },
        )

        self.assertEqual(
            result["environment"],
            {
                "host": "linux",
                "rustc_release": "1.94.1",
                "rustc_host": "x86_64-unknown-linux-gnu",
            },
        )

    def test_hash_and_summary_change_when_compiler_listing_changes(self) -> None:
        before = self.build()
        self.listings["Cargo.toml#agentdesk::lib::agentdesk"] += (
            "services::relay::tests::new_case: test\n"
        )
        after = self.build()

        self.assertNotEqual(
            before["summary"]["records_sha256"],
            after["summary"]["records_sha256"],
        )
        self.assertEqual(
            after["summary"]["test_count"], before["summary"]["test_count"] + 1
        )

    def test_missing_target_listing_fails_closed(self) -> None:
        del self.listings["Cargo.toml#agentdesk::bin::agentdesk"]

        with self.assertRaisesRegex(ValueError, "missing libtest listing"):
            self.build()

    def test_check_bytes_detects_mutation(self) -> None:
        expected = manifest.render_manifest(self.build())
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "manifest.json"
            path.write_text(expected, encoding="utf-8")
            manifest.check_bytes(expected, path)
            path.write_text(
                expected.replace("stable_case", "mutated_case", 1), encoding="utf-8"
            )

            with self.assertRaisesRegex(ValueError, "does not match compiler output"):
                manifest.check_bytes(expected, path)

    def test_compile_contract_is_one_all_target_no_run_invocation(self) -> None:
        self.assertEqual(manifest.COMPILE_COMMAND.count("cargo"), 1)
        self.assertIn("--all-targets", manifest.COMPILE_COMMAND)
        self.assertIn("--no-run", manifest.COMPILE_COMMAND)
        self.assertNotIn("--doc", manifest.COMPILE_COMMAND)

    def test_repository_manifest_is_canonical_and_self_hashes(self) -> None:
        path = REPO_ROOT / manifest.MANIFEST_REL
        payload = json.loads(path.read_text(encoding="utf-8"))

        self.assertEqual(
            path.read_text(encoding="utf-8"), manifest.render_manifest(payload)
        )
        self.assertEqual(
            payload["summary"]["records_sha256"],
            manifest.content_sha256(payload["tests"]),
        )
        self.assertEqual(payload["summary"]["test_count"], len(payload["tests"]))
        self.assertEqual(payload["summary"]["target_count"], len(payload["targets"]))

    def test_ci_script_checks_runs_focused_fixture_suite(self) -> None:
        script = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(encoding="utf-8")
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_compiler_test_manifest', script
        )
        self.assertNotIn("generate_compiler_test_manifest.py --check", script)


if __name__ == "__main__":
    unittest.main()
