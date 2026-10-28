#!/usr/bin/env python3
"""Generate the preparation IDs from the repository's current Rust source universe."""
import importlib.util
import json
from pathlib import Path
import sys

root = Path(__file__).resolve().parents[2]
source = root / "scripts/ci/rust_test_sources.py"
spec = importlib.util.spec_from_file_location("s5_rust_sources", source)
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
rows = {}
for platform, (compiled, ignored) in module.platform_tests(root).items():
    for test_id in compiled:
        if "::input_owner::" not in test_id and "::s5_fixtures::" not in test_id and "::s5_effect_tests::" not in test_id:
            continue
        row = rows.setdefault(test_id, {
            "id": test_id,
            "platforms": [],
            "run_mode": "pg15-required" if test_id.endswith("_pg") else "unfiltered-lib",
            "path_gate": "ci-pr.yml Rust src/** or tests/**; Linux archive all(); PG15 exact IDs for _pg",
            "ignored": False,
        })
        row["platforms"].append(platform)
        assert test_id not in ignored
assert rows, "preparation must register positive test IDs"
output = {"platform_meaning": "source compilation universe; Linux execution required, macOS/Windows native execution separate", "generated_by": "python3 tests/input_owner_rollback/generate_required.py",
          "source": "scripts/ci/rust_test_sources.py platform_tests; ci-pr.yml archive and PG15 jobs",
          "tests": sorted(rows.values(), key=lambda row: row["id"])}
(root / "tests/input_owner_rollback/required-tests.json").write_text(json.dumps(output, indent=2) + "\n")
print(f"S5_REQUIRED_IDS {len(rows)}")
