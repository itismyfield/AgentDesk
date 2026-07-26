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
                            "test": True,
                            "doctest": True,
                        },
                        {
                            "name": "agentdesk",
                            "kind": ["bin"],
                            "test": True,
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

    def test_duplicate_source_names_remain_ambiguous(self) -> None:
        owners = manifest.source_owners(
            {
                "first::tests": {"shared::tests::case"},
                "second::tests": {"shared::tests::case"},
            }
        )

        self.assertNotIn("shared::tests::case", owners)
        self.assertEqual(
            manifest.resolve_owner("shared::tests::case", owners)["kind"],
            "generated_or_unresolved",
        )

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
                "resolution": "generated_or_macro_expanded_source",
                "module": "services::relay::tests",
                "source_test_id": None,
            },
        )

    def test_unresolved_ownership_is_explicitly_classified(self) -> None:
        self.assertEqual(
            manifest.classify_unresolved_owner(
                "services::discord::inflight::stall_recovery_tests::"
                "flake_isolation_4361::case"
            )["resolution"],
            "included_or_nested_test_source",
        )
        self.assertEqual(
            manifest.classify_unresolved_owner(
                "services::discord::voice_barge_in::tests::pcm_harness_tests::case"
            )["resolution"],
            "nested_external_test_source",
        )
        self.assertEqual(
            manifest.classify_unresolved_owner("smoke_test::smoke_health_and_agents")[
                "resolution"
            ],
            "integration_source_outside_library_scanner",
        )

    def test_separates_doctests_and_marks_zero_test_binary_non_vacuous_false(self) -> None:
        result = self.build()

        self.assertFalse(result["doctests"]["included"])
        self.assertEqual(len(result["doctests"]["exclusions"]), 1)
        targets = {target["target_id"]: target for target in result["targets"]}
        binary = targets["Cargo.toml#agentdesk::bin::agentdesk"]
        self.assertEqual(binary["status"], "listed")
        self.assertEqual(binary["test_count"], 0)
        self.assertFalse(binary["non_vacuous"])
        self.assertIn("binary entry point", binary["zero_test_allowance"])

    def test_environment_is_provenance_not_cross_host_authority(self) -> None:
        linux = manifest.build_manifest(
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
        macos = manifest.build_manifest(
            self.metadata,
            self.messages,
            self.listings,
            self.source,
            self.root,
            environment={
                "host": "darwin",
                "rustc_release": "1.94.1",
                "rustc_host": "aarch64-apple-darwin",
            },
        )

        self.assertNotEqual(linux["environment"], macos["environment"])
        self.assertEqual(
            linux["summary"]["records_sha256"],
            macos["summary"]["records_sha256"],
        )
        self.assertEqual(
            manifest.authority_projection(linux),
            manifest.authority_projection(macos),
        )
        self.assertEqual(
            linux["authority_scope"],
            {
                "records_and_targets": "cross_host",
                "environment": "provenance_only",
                "pinned_rustc_release": "1.94.1",
            },
        )

    def test_check_allows_provenance_environment_change_only(self) -> None:
        expected = self.build()
        tracked = self.build()
        tracked["environment"]["host"] = "different-host"
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "manifest.json"
            path.write_text(manifest.render_manifest(tracked), encoding="utf-8")
            manifest.check_manifest(expected, path)

    def test_hash_and_summary_change_when_compiler_listing_changes(self) -> None:
        before = self.build()
        self.listings["Cargo.toml#agentdesk::lib::agentdesk"] = (
            self.listings["Cargo.toml#agentdesk::lib::agentdesk"]
            .replace("2 tests, 0 benchmarks\n", "")
            + "services::relay::tests::new_case: test\n"
            + "3 tests, 0 benchmarks\n"
        )
        after = self.build()

        self.assertNotEqual(
            before["summary"]["records_sha256"],
            after["summary"]["records_sha256"],
        )
        self.assertEqual(
            after["summary"]["test_count"], before["summary"]["test_count"] + 1
        )

    def test_current_terse_listing_without_summary_is_supported(self) -> None:
        self.assertEqual(
            manifest.parse_libtest_listing("suite::case: test\n"),
            ("suite::case",),
        )

    def test_malformed_libtest_format_fails_closed(self) -> None:
        for listing in (
            "libtest format changed\n",
            "services::relay::tests::stable_case: test\n2 tests, 0 benchmarks\n",
            "0 tests, 1 benchmark\n",
        ):
            with self.subTest(listing=listing):
                with self.assertRaises(ValueError):
                    manifest.parse_libtest_listing(listing)

    def test_metadata_target_without_compiler_artifact_fails_closed(self) -> None:
        with self.assertRaisesRegex(ValueError, "no test executable"):
            manifest.build_manifest(
                self.metadata,
                self.messages[:1],
                {"Cargo.toml#agentdesk::lib::agentdesk": self.listings[
                    "Cargo.toml#agentdesk::lib::agentdesk"
                ]},
                self.source,
                self.root,
            )

    def test_unexpected_or_stale_executable_fails_closed(self) -> None:
        unexpected = json.dumps(
            {
                "reason": "compiler-artifact",
                "package_id": "foreign-package",
                "target": {"name": "foreign", "kind": ["test"]},
                "profile": {"test": True},
                "executable": "/repo/target/debug/deps/foreign",
            }
        )
        expected = manifest.expected_test_targets(
            self.metadata, manifest.package_labels(self.metadata, self.root)
        )
        with self.assertRaisesRegex(ValueError, "unexpected non-workspace"):
            manifest.parse_test_executables([unexpected], expected)

        with tempfile.TemporaryDirectory() as temp:
            target_root = Path(temp)
            stale = json.loads(self.messages[0])
            stale["executable"] = str(target_root / "missing")
            with self.assertRaisesRegex(ValueError, "missing or outside"):
                manifest.parse_test_executables(
                    [json.dumps(stale), self.messages[1]], expected, target_root
                )

    def test_missing_target_listing_fails_closed(self) -> None:
        del self.listings["Cargo.toml#agentdesk::bin::agentdesk"]

        with self.assertRaisesRegex(ValueError, "listing target mismatch"):
            self.build()

    def test_unexpected_zero_target_fails_closed(self) -> None:
        self.listings["Cargo.toml#agentdesk::lib::agentdesk"] = (
            "0 tests, 0 benchmarks\n"
        )

        with self.assertRaisesRegex(ValueError, "unexpectedly listed zero tests"):
            self.build()

    def test_manifest_target_semantic_drift_fails(self) -> None:
        expected = self.build()
        tracked = self.build()
        tracked["targets"][0]["zero_test_allowance"] = "edited reason"
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "manifest.json"
            path.write_text(manifest.render_manifest(tracked), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "fresh compiler output"):
                manifest.check_manifest(expected, path)

    def test_stale_manifest_record_and_summary_drift_fail(self) -> None:
        expected = self.build()
        tracked = self.build()
        tracked["tests"][0]["test_name"] = "edited-together"
        tracked["summary"]["records_sha256"] = manifest.content_sha256(
            tracked["tests"]
        )
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "manifest.json"
            path.write_text(manifest.render_manifest(tracked), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "fresh compiler output"):
                manifest.check_manifest(expected, path)

    def test_check_manifest_detects_mutation(self) -> None:
        expected = self.build()
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "manifest.json"
            path.write_text(manifest.render_manifest(expected), encoding="utf-8")
            manifest.check_manifest(expected, path)
            path.write_text(
                manifest.render_manifest(expected).replace(
                    "stable_case", "mutated_case", 1
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(ValueError, "record hash is stale"):
                manifest.check_manifest(expected, path)

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

    def test_ci_compile_lane_runs_fresh_authority_check_via_just(self) -> None:
        workflow = (REPO_ROOT / ".github/workflows/ci-pr.yml").read_text(
            encoding="utf-8"
        )
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        scripts = (REPO_ROOT / "scripts/ci-script-checks.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn("toolchain: \"1.94.1\"", workflow)
        self.assertEqual(workflow.count("run: just check-compiler-test-manifest"), 1)
        fast_job = workflow[
            workflow.index("  check_fast:") : workflow.index("  check_fast_cross_os:")
        ]
        self.assertNotIn(
            "- name: cargo check\n        run: just cargo-check", fast_job
        )
        self.assertIn(
            "check-compiler-test-manifest:\n"
            "    python3 scripts/generate_compiler_test_manifest.py --check",
            justfile,
        )
        self.assertIn(
            '"$PYTHON" -m unittest tests.test_compiler_test_manifest', scripts
        )
        self.assertNotIn("generate_compiler_test_manifest.py --check", scripts)

    def test_edited_generator_and_manifest_cannot_self_approve_without_ci_step(
        self,
    ) -> None:
        workflow = (REPO_ROOT / ".github/workflows/ci-pr.yml").read_text(
            encoding="utf-8"
        )
        mutated = workflow.replace(
            "run: just check-compiler-test-manifest", "run: true", 1
        )
        self.assertNotIn("run: just check-compiler-test-manifest", mutated)
        self.assertIn("run: just check-compiler-test-manifest", workflow)

    def test_compile_contract_uses_fresh_isolated_target_output(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            'TemporaryDirectory(prefix="compiler-test-manifest-target-")', source
        )
        self.assertIn('environment["CARGO_TARGET_DIR"] = str(artifact_root)', source)
        self.assertIn("resolve(strict=True)", source)
        self.assertIn("relative_to(canonical_root)", source)
        self.assertNotIn("Cargo reused stale test executable", source)


if __name__ == "__main__":
    unittest.main()
