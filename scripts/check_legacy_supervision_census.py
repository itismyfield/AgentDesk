#!/usr/bin/env python3
"""B1 dormant lexical gate; shared cfg/prose classification is not Rust name resolution.
Unknown primitive helpers, aliases/reexports, macros and external symbol references fail closed.
"""
import hashlib
import re
import sys
from collections import Counter
from pathlib import Path
try:
    from scripts import check_durable_frontier_writer_call_sites as classifier
except ModuleNotFoundError:
    import check_durable_frontier_writer_call_sites as classifier

D = "src/services/discord/"
R = D + "runtime_bootstrap/boot_retirement"
H = D + "health/legacy_supervision/boot_status.rs"
ENTRIES = {
    R + ".rs": "new install_process install_in",
    R + "/cohort.rs": "new epoch update try_start_confirmation mark_timed_out wait_released snapshot begin work_once arrive_reaped exclude_no_runtime transition fail drop",
    R + "/completion.rs": "value matches new run_once start join_work receive",
    R + "/publication.rs": "matches new confirm publish_with seal",
    H: "",
}
PROTECTED = {
    'src/cli/dcserver.rs': '61e850c1d3ea0acf4725c8ce4fa109f64918fbf3cf451d43b3330a9b6d660fb3',
    'src/services/discord/runtime_bootstrap.rs': '2c76094a3c6bb098ee0b4eccd87416a7927fe80f52739995ca4c56efc2490d0a',
    'src/services/discord/inflight/removal/boot_reaper.rs': '14b7475c4471438af7667b50f7053d5cc86fba5d10f3d5407253f6b92dc81b1e',
    'src/services/discord/tui_direct_pending_start/turn_retirement.rs': 'fb01deafde3c9bc0b55c2590a294fa1f8f5a6c6b94e3e54441dd0fcadf8273b9',
    'src/services/discord/runtime_bootstrap/o_writer_host.rs': '52d696ebffbf91e7d24ea8344ff9d034ba9b158ceceb6afe13add7349488abe0',
    'src/services/discord/health/legacy_supervision.rs': '1c1497db24b104d698762e3d24c51e224acce622e0cc4255806b1d02dd55a4b3',
    'src/services/discord/health/snapshot.rs': 'a389f92170602c561a021ab6f36d4db543648817b4da8b6f5d90f2ba4eb9a035'}
SYMBOL = re.compile(r"\b(?:Boot(?:Cohort|Slot|Publication|WorkOnce|Roster|Bot|Selection|Result|RetirementHealth)|boot_retirement|boot_status|install_process|wait_released|publish_with)\b")
FN = re.compile(r"\bfn\s+(\w+)")

def digest(code):
    return hashlib.sha256(re.sub(r"\s+", "", code).encode()).hexdigest()

def audit(sources, entries=ENTRIES, protected=PROTECTED, skips=frozenset()):
    errors = []
    for path, expected in entries.items():
        code = sources.get(path, "")
        if Counter(FN.findall(code)) != Counter(expected.split()):
            errors.append(f"{path}: primitive entry manifest drift")
        if re.search(r"\b(?:macro_rules|include|ctor|env|unsafe|Command)\b", code):
            errors.append(f"{path}: unclassified activation syntax")
    for path, expected in protected.items():
        if digest(sources.get(path, "")) != expected:
            errors.append(f"{path}: protected boot/RETIRED body changed")
    tests = {R + "/" + name + "_tests.rs" for name in ("cohort", "completion", "publication")}
    for path, code in sources.items():
        if path.startswith(R + "/") and path not in entries and path not in tests:
            errors.append(f"{path}: unclassified primitive file")
        if path in entries or path in skips or path in tests:
            continue
        pattern = SYMBOL if not ("RETIRED" in code and "legacy_supervision" in code) else re.compile(SYMBOL.pattern + r"|\bRETIRED\b")
        for match in pattern.finditer(code):
            owners = FN.findall(code[:match.start()])
            line = code[:match.start()].count("\n") + 1
            errors.append(f"{path}:{line}:{owners[-1] if owners else '<item>'}: external {match.group()}")
    return errors

def check(root):
    try:
        files, skips = classifier._scan_inputs(root, classifier.PINNED_TEST_ONLY_MODULE_FILES)
        sources = {p.relative_to(root).as_posix(): classifier._production_text(p) for p in files}
        errors = audit(sources, skips={p.relative_to(root).as_posix() for p in skips})
    except (OSError, RuntimeError) as error:
        errors = [str(error)]
    if errors:
        return False, "B1_DORMANT=FAIL\n" + "\n".join(errors)
    names = "canonical_retired_production_writers boot_cohort_application_install_calls boot_barrier_application_wait_calls boot_publication_application_calls typed_completion_live_reaper_connections boot_health_endpoint_connections"
    return True, "B1_DORMANT=PASS\n" + "\n".join(name + "=0" for name in names.split()) + "\nactivation_ready=false"

def sync_skips(root):
    pin = classifier._SKIP_PIN
    files = pin._lexical_rust_files(root, Path("src"))
    basename = {p for p in files if classifier.is_test_file(p.name)}
    resolved = pin._INVENTORY.test_only_module_files(production_files=[p for p in files if p not in basename], all_files=files)
    path = root / "scripts/test_only_module_skip_pin.py"
    source = path.read_text()
    for name, paths in (("PINNED_BASENAME_TEST_FILES", basename), ("PINNED_RESOLVER_TEST_ONLY_FILES", resolved)):
        measured = {p.relative_to(root).as_posix() for p in paths}
        actual = {p.relative_to(root).as_posix() for p in basename | resolved}
        if pin.PINNED_TEST_ONLY_MODULE_FILES - actual:
            raise RuntimeError("refusing pin removal")
        end = source.index("\n    }\n)", source.index(name + " ="))
        additions = "".join('\n        "' + p + '",' for p in sorted(measured - pin.PINNED_TEST_ONLY_MODULE_FILES))
        source = source[:end] + additions + source[end:]
    path.write_text(source)


if __name__ == "__main__":
    if sys.argv[1:] == ["--sync-skips"]:
        sync_skips(Path.cwd())
        sys.exit(0)
    ok, report = check(Path.cwd())
    print(report)
    sys.exit(0 if ok else 1)
