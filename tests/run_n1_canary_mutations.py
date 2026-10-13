"""Execute guard-removal mutants in copies, and prove worktree bytes stay unchanged."""
import hashlib
import os
from pathlib import Path
import shutil
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROD = [ROOT / "scripts/n1_turn_canary_oracle.py", ROOT / "scripts/ci-script-checks.sh",
        *sorted((ROOT / "scripts/n1_canary").glob("*.*"))]
MUTANTS = [
    ("http-truncation-uncaught", "discord.py", "URLError, HTTPException", "URLError",
     "tests.test_n1_canary_collection.CLIFixture.test_truncated_http_body_reaches_cli_unknown"),
    ("remove-pg-ci-fixture", "ci-script-checks.sh", '"$PYTHON" tests/run_n1_canary_pg_fixture.py tests.test_n1_canary_pg', "true",
     "tests.test_n1_canary_collection.EffectBoundary.test_ci_registration_is_in_a_required_shard"),
    ("empty-collection-as-visible", "discord.py", "if not visibility_proven(result):", "if False:",
     "tests.test_n1_canary_collection.DiscordContracts.test_empty_history_requires_independent_visibility_evidence"),
    ("empty-fixture-as-visible", "discord.py", "and visibility_proven(value)", "and True",
     "tests.test_n1_canary_collection.CLIFixture.test_empty_history_fixture_without_visibility_is_unknown"),
    ("uri-as-pgdatabase", "evidence.py",
     'env = {**os.environ, "PGCONNECT_TIMEOUT": "5", "PGOPTIONS": PG_OPTIONS}',
     'env = {**os.environ, "PGDATABASE": dsn, "PGCONNECT_TIMEOUT": "5", "PGOPTIONS": PG_OPTIONS}',
     "tests.test_n1_canary_pg.URIConnection.test_uri_reaches_expected_database_and_enforces_readonly"
     if os.environ.get("AGENTDESK_REQUIRE_PG") == "1" else
     "tests.test_n1_canary_collection.PGContracts.test_uri_uses_explicit_dbname_and_pins_connection_options"),
    ("uri-options-override", "evidence.py",
     'pairs = [(key, value) for key, value in pairs if key not in {"options", "connect_timeout"}]',
     'return dsn',
     "tests.test_n1_canary_pg.URIConnection.test_uri_reaches_expected_database_and_enforces_readonly"
     if os.environ.get("AGENTDESK_REQUIRE_PG") == "1" else
     "tests.test_n1_canary_collection.PGContracts.test_uri_uses_explicit_dbname_and_pins_connection_options"),
    ("unavailable-to-zero", "evidence.py", "return summary, {}, reasons\n",
     "summary.update(dropped_events=0, open_placeholder_attempts=0)\n            return summary, {}, reasons\n",
     "tests.test_n1_canary_oracle.Fixture.test_unavailable_is_not_zero"),
    ("ignore-drop", "evidence.py",
     'healthy = all(s["dropped_events"] == 0 and s["sink_errors"] == 0 and not s["observer_unhealthy"] for s in snaps)',
     "healthy = True", "tests.test_n1_canary_oracle.Fixture.test_drop_marker_is_not_ignored"),
    ("incomplete-open-as-number", "evidence.py", "if healthy and complete:", "if healthy:",
     "tests.test_n1_canary_oracle.Fixture.test_incomplete_open_attempts_is_not_a_number"),
    ("unproduced-unknown-as-zero", "evidence.py", "unknown_outcome_events=None", "unknown_outcome_events=0",
     "tests.test_n1_canary_oracle.Fixture.test_valid_inputs_still_cannot_pass"),
    ("ignore-manifest-version", "manifest.py", 'manifest["manifest_version"] == table["manifest_version"]', "True",
     "tests.test_n1_canary_oracle.Fixture.test_unknown_missing_manifest_fields_and_versions"),
    ("event-time-as-confirmation", "evidence.py", 'event.get("actual_confirmed_at_us")', 'event.get("observed_at_us")',
     "tests.test_n1_canary_oracle.Fixture.test_event_time_cannot_replace_actual_confirmation"),
    ("ignore-binding-transition", "evidence.py", "any(row.get(k) != v for k, v in expected.items())", "False",
     "tests.test_n1_canary_oracle.Fixture.test_source_and_binding_a_b_a_cannot_be_hidden_by_endpoints"),
    ("pg-readonly-off", "evidence.py", "default_transaction_read_only=on", "default_transaction_read_only=off",
     "tests.test_n1_canary_collection.PGContracts.test_select_only_query_and_readonly_connection_are_executed"),
    ("pg-write", "evidence.py", 'PG_SELECT = """SELECT', 'PG_SELECT = """DELETE FROM public.intake_outbox; SELECT',
     "tests.test_n1_canary_collection.PGContracts.test_select_only_query_and_readonly_connection_are_executed"),
    ("partial-discord-as-complete", "discord.py", 'except Unknown as error:\n        result["reasons"]',
     'except Unknown as error:\n        result["complete"] = True\n        result["reasons"]',
     "tests.test_n1_canary_collection.DiscordContracts.test_page_budget_partial_and_individual_404"),
    ("rate-limit-as-empty", "discord.py", 'except HTTPError as error:\n            raise Unknown',
     'except HTTPError as error:\n            if error.code == 429:\n                return []\n            raise Unknown',
     "tests.test_n1_canary_collection.DiscordContracts.test_short_page_requires_terminal_boundary_or_empty_page"),
    ("ignore-discord-boundary", "discord.py", "if ids != sorted(set(ids), reverse=True) or any(i >= cursor for i in ids):",
     "if False:", "tests.test_n1_canary_collection.DiscordContracts.test_page_boundaries_limits_and_http_errors_are_unknown"),
    ("discord-post", "discord.py", 'method="GET"', 'method="POST"',
     "tests.test_n1_canary_collection.EffectBoundary.test_production_has_only_audited_io"),
    ("file-delete", "n1_turn_canary_oracle.py", '    print(json.dumps(result, sort_keys=True))',
     '    Path("/fixture-only-never-executed").unlink()\n    print(json.dumps(result, sort_keys=True))',
     "tests.test_n1_canary_collection.EffectBoundary.test_production_has_only_audited_io"),
    ("cli-pass", "n1_turn_canary_oracle.py", "code = finish(result)", "code = 0",
     "tests.test_n1_canary_oracle.Fixture.test_valid_inputs_still_cannot_pass"),
    ("remove-ci-wiring", "ci-script-checks.sh",
     '"$PYTHON" -m unittest tests.test_n1_canary_oracle tests.test_n1_canary_collection', "true",
     "tests.test_n1_canary_collection.EffectBoundary.test_ci_registration_is_in_a_required_shard"),
]
if os.environ.get("AGENTDESK_REQUIRE_PG") == "1":
    MUTANTS.extend([
        ("text-channel-bigint-reverted", "evidence.py", "WHERE channel_id = :'channel'",
         "WHERE channel_id = :'channel'::bigint", "tests.test_n1_canary_pg.URIConnection.test_uri_reaches_expected_database_and_enforces_readonly"),
        ("numeric-channel-normalization-removed", "evidence.py", 'row["channel_id"] = int(row["channel_id"])',
         "pass", "tests.test_n1_canary_pg.URIConnection.test_text_channels_reach_numeric_origin_audit"),
    ])


def main():
    original = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in PROD}
    failures = []
    for label, filename, before, after, test in MUTANTS:
        with tempfile.TemporaryDirectory(prefix="n1-canary-mutant-") as tmp:
            dest = Path(tmp)
            shutil.copytree(ROOT / "scripts/n1_canary", dest / "scripts/n1_canary", ignore=shutil.ignore_patterns("__pycache__"))
            shutil.copy2(ROOT / "scripts/n1_turn_canary_oracle.py", dest / "scripts/n1_turn_canary_oracle.py")
            shutil.copy2(ROOT / "scripts/ci-script-checks.sh", dest / "scripts/ci-script-checks.sh")
            (dest / "tests").mkdir()
            (dest / "tests/__init__.py").write_text("")
            for name in ("test_n1_canary_oracle.py", "test_n1_canary_collection.py", "test_n1_canary_pg.py"):
                shutil.copy2(ROOT / "tests" / name, dest / "tests" / name)
            shutil.copytree(ROOT / "tests/fixtures/n1_canary_o1", dest / "tests/fixtures/n1_canary_o1")
            path = dest / "scripts" / filename if filename in ("n1_turn_canary_oracle.py", "ci-script-checks.sh") else dest / "scripts/n1_canary" / filename
            source = path.read_text()
            if before not in source:
                raise AssertionError(f"mutant location missing: {label}")
            path.write_text(source.replace(before, after, 1))
            if label == "uri-as-pgdatabase":
                path.write_text(path.read_text().replace('"--dbname", pg_connection(dsn), ', '', 1))
            run = subprocess.run([sys.executable, "-m", "unittest", "-v", test], cwd=dest,
                                 capture_output=True, text=True, timeout=30)
            assertion_red = (run.returncode == 1 and "FAIL:" in run.stderr and "AssertionError" in run.stderr
                             and re.search(r"^ERROR: test_", run.stderr, re.M) is None)
            print(f"{label}: {'RED (assertion)' if assertion_red else 'MUTANT SURVIVED/INVALID'}", flush=True)
            print(run.stderr, flush=True)
            if not assertion_red:
                failures.append(label)
    if original != {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in PROD}:
        raise AssertionError("worktree bytes changed")
    print(f"mutation summary: {len(MUTANTS)} executed, {len(failures)} survived; worktree bytes identical")
    return bool(failures)


if __name__ == "__main__":
    sys.exit(main())
