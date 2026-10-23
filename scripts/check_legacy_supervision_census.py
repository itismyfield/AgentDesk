#!/usr/bin/env python3
"""B1 dormant lexical gate; shared cfg/prose classification is not Rust name resolution.
Unknown primitive helpers, aliases/reexports, macros and external symbol references fail closed.
"""
import hashlib
import argparse
import re
import sys
from collections import Counter
from pathlib import Path
try:
    from scripts import check_durable_frontier_writer_call_sites as classifier
    from scripts import skip_pin_registry as registry
except ModuleNotFoundError:
    import check_durable_frontier_writer_call_sites as classifier
    import skip_pin_registry as registry

D = "src/services/discord/"
R = D + "runtime_bootstrap/boot_retirement"
ENTRIES = {}
# Boot activation sites are pinned per function so unrelated edits elsewhere in a file pass;
# "*" pins a whole file.
PROTECTED = {
    'src/cli/dcserver.rs': {'handle_dcserver': 'cd0fa633d7bbf6ed71ab6887cba145d5d2cc01e6255ed5668ea4173659b9cd66'},
    'src/services/discord/runtime_bootstrap.rs': {'run_bot': 'd344938663af32989de5acd9ded03d887042e1ee587754cc1d9084c77418e9ca'},
    'src/services/discord/inflight/removal/boot_reaper.rs': {
        'run_once': '6364c6e13da83bbf08092a6460de03dda10281c0af81b5f1293ab51b3ace9858',
        'prepare_before_reap': '9de0e7d478987fc0464a1753bee32171dc54a76cbebd4f8db4a7df6b47068f3e',
        'reap_inflight_rows_at_boot_blocking': '33ea1fcffa8493b9269880c2b9d4d10d9ccd2fafb316e7c9b8eca353b72913f1',
        'reap_inflight_rows_at_boot_with_guard': 'b4caa2fdf84b50fb3ff8e255d9712b791bfd07ea69e0aee9a8bb08f1a3d8e284',
        'reap_inflight_rows_after_preparation': 'f3fee466ddc3d3925b5bfee8805a6678875db72874f30c51a9a74fee38d693bd'},
    'src/services/discord/tui_direct_pending_start/turn_retirement.rs': {'confirm_at_boot': '835b358746ebb4b1dd99bf662b73923f4904b73ce0b973b9aea84fa1d128ad07'},
    'src/services/discord/runtime_bootstrap/o_writer_host.rs': {'adopted': '0fe2f48e1465b84da5a3b7155df25e012efc1510762c78a4c61ae55ea48f2a23'},
    'src/services/discord/health/snapshot.rs': {'build_health_snapshot_with_options': 'f51ee799255b84bf55b1d715bc1fe898507c5aab6808bee975e047608d3203f5'}}
SYMBOL = re.compile(r"\b(?:Boot(?:Cohort|Slot|Publication|WorkOnce|Roster|Bot|Selection|Result|RetirementHealth)|boot_retirement|boot_status|install_process|wait_released|publish_with)\b")
# The deleted RETIRED gate (#6325 23C): any production return of its owner or readers fails.
RETIRED_GATE = re.compile(r"\b(?:legacy_supervision|legacy_retired|retired_in|RetiredForTest)\b")
FN = re.compile(r"\bfn\s+(\w+)")

def digest(code):
    return hashlib.sha256(re.sub(r"\s+", "", code).encode()).hexdigest()

def function_text(code, name):
    """One `fn name` item, from its line start to the matching brace; `code` is production text,
    whose strings and comments are already blanked, so braces inside them never count."""
    found = list(re.finditer(r"\bfn\s+" + re.escape(name) + r"\b", code))
    if len(found) != 1:
        raise ValueError(f"{len(found)} definitions")
    start = code.rfind("\n", 0, found[0].start()) + 1
    depth = nest = 0
    for at in range(found[0].end(), len(code)):
        nest += {"(": 1, "[": 1, ")": -1, "]": -1}.get(code[at], 0) if depth == 0 else 0
        if code[at] == ";" and depth == nest == 0:
            raise ValueError("no body")
        depth += {"{": 1, "}": -1}.get(code[at], 0)
        if code[at] == "}" and depth == 0:
            return code[start:at + 1]
    raise ValueError("unbalanced body")

def protected_errors(path, code, pins):
    errors = []
    for name, expected in pins.items():
        try:
            actual = digest(code if name == "*" else function_text(code, name))
        except ValueError as error:
            errors.append(f"{path}::{name}: protected function extraction failed ({error})")
            continue
        if actual != expected:
            errors.append(f"{path}::{name}: protected boot/RETIRED body changed; "
                          f"after review re-pin {name!r}: {actual!r}")
    return errors

def audit(sources, entries=ENTRIES, protected=PROTECTED, skips=frozenset()):
    errors = []
    for path, expected in entries.items():
        code = sources.get(path, "")
        if Counter(FN.findall(code)) != Counter(expected.split()):
            errors.append(f"{path}: primitive entry manifest drift")
        if re.search(r"\b(?:macro_rules|include|ctor|env|unsafe|Command)\b", code):
            errors.append(f"{path}: unclassified activation syntax")
    for path, pins in protected.items():
        if path not in sources or not pins:
            errors.append(f"{path}: protected file missing or unpinned")
        errors.extend(protected_errors(path, sources.get(path, ""), pins))
    tests = {R + "/" + name + "_tests.rs" for name in ("cohort", "completion", "publication")}
    for path, code in sources.items():
        if path.startswith(R + "/") and path not in entries and path not in tests:
            errors.append(f"{path}: unclassified primitive file")
        if path in entries or path in skips or path in tests:
            continue
        if "/legacy_supervision" in path:
            errors.append(f"{path}: deleted RETIRED owner restored")
        pattern = re.compile(SYMBOL.pattern + "|" + RETIRED_GATE.pattern)
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

def sync_skips(root, promoted_paths=(), deleted_paths=(), *, fragment=None):
    """Validate basename mounts, then sync resolver fragments without rewriting legacy pins."""
    pin = classifier._SKIP_PIN
    files = pin._lexical_rust_files(root, Path("src"))
    basename = {p for p in files if classifier.is_test_file(p.name)}
    resolved = pin._INVENTORY.test_only_module_files(production_files=[p for p in files if p not in basename], all_files=files)
    pin.validate_basename_mounts(files, basename, resolved)
    measured = {p.relative_to(root).as_posix(): "resolver" for p in resolved}
    prior = registry.load(root)
    for path in basename:
        rel = path.relative_to(root).as_posix()
        if rel in prior.rows and prior.rows[rel].payload == "basename":
            measured[rel] = "basename"
    existing = {p.relative_to(root).as_posix() for p in files}
    return registry.sync(root, measured, existing, promoted_paths, fragment, deleted=deleted_paths)


if __name__ == "__main__":
    if sys.argv[1:2] == ["--sync-skips"]:
        parser = argparse.ArgumentParser(description="Synchronize lexical test-only pins")
        parser.add_argument('--promote', action='append', default=[], metavar='PATH',
                            help='explicitly remove this existing production module from the skip pin')
        parser.add_argument('--remove-deleted', '--delete-pin', action='append', default=[], metavar='PATH',
                            help='explicitly remove this deleted file from the skip pin')
        parser.add_argument('--fragment', metavar='NODE-ISSUE-SLICE',
                            help='skip-pin fragment for new pins (default: one auto fragment per new path)')
        args = parser.parse_args(sys.argv[2:])
        manual, written = sync_skips(Path.cwd(), args.promote, args.remove_deleted, fragment=args.fragment)
        for line in (*(f"skip pin fragment written: {path}" for path in written), *manual):
            print(line, file=sys.stderr if manual else sys.stdout)
        sys.exit(1 if manual else 0)
    ok, report = check(Path.cwd())
    print(report)
    sys.exit(0 if ok else 1)
