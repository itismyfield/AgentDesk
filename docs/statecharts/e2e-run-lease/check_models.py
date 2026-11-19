#!/usr/bin/env python3
"""Run the bounded TLC matrix and keep machine-readable, fail-closed results."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
from concurrent.futures import ThreadPoolExecutor
import tempfile


RUN_MUTANTS = {"none", "ttl_takeover", "non_atomic", "parent_unlock", "missing_fd",
               "duplicate_child", "no_pending", "clear_on_idle", "send_after_release",
               "leak_fd", "foreign_cleanup", "crash_no_reclaim"}
# Runner settings (off, one-run budget) and their mutants exist only for the run lease.
RUNNER_CONFIGS = {"off", "ignore_off", "budget_one", "skip_reserve", "over_budget", "advance_on_crash"}
TRANSACTION_CONFIGS = {"tx-good", "tx-off", "tx-null", "tx-old-off", "tx-old-null",
    "tx-unknown-zero", "tx-between-only", "tx-consume-deferred", "tx-apply-first",
    "tx-no-recover", "tx-result-regress", "tx-status-first", "tx-alert-drop", "tx-double-apply", "tx-orphan-no-pause"}
CONSUMER_CONFIGS = {"consumer-good", "consumer-nodata-green", "consumer-off-green", "consumer-stale-green", "consumer-old-head-green", "consumer-zero-green", "consumer-usage-green", "consumer-driver-schema-green", "consumer-qualifier-green", "consumer-usage-crash", "consumer-no-budget-heartbeat", "consumer-hide-pending", "consumer-stale-masks-head", "consumer-unobserved-fail"}
INSTANCE_MUTANTS = RUN_MUTANTS - {"no_pending", "clear_on_idle", "send_after_release"}
RUN_MUTANTS |= RUNNER_CONFIGS | TRANSACTION_CONFIGS | CONSUMER_CONFIGS
EXPECTED_CONFIGS = ({f"run-{mut}.cfg" for mut in RUN_MUTANTS}
                    | {f"instance-{mut}.cfg" for mut in INSTANCE_MUTANTS}
                    | {f"{kind}-live-{mut}.cfg" for kind in ("run", "instance")
                       for mut in ("none", "leak_fd")})


def select_configs(base):
    candidates = {path.name: path for path in base.glob('*.cfg')}
    actual = set(candidates)
    if actual != EXPECTED_CONFIGS:
        raise ValueError(f"incomplete TLC matrix: missing={sorted(EXPECTED_CONFIGS - actual)} "
                         f"unexpected={sorted(actual - EXPECTED_CONFIGS)}")
    return [candidates[name] for name in sorted(EXPECTED_CONFIGS)]


def self_test_selection():
    cases = [
        ('complete', EXPECTED_CONFIGS, True),
        ('empty_0', set(), False),
        ('missing_one', EXPECTED_CONFIGS - {'run-none.cfg'}, False),
        ('unknown_singleton', {'unknown.cfg'}, False),
        ('additional_unknown', EXPECTED_CONFIGS | {'unknown.cfg'}, False),
    ]
    results = []
    for name, names, accepted in cases:
        with tempfile.TemporaryDirectory(prefix='i8120-tlc-selection-') as temp:
            base = Path(temp)
            for config in names:
                (base / config).touch()
            try:
                selected = select_configs(base)
                actual = len(selected) == len(EXPECTED_CONFIGS)
            except ValueError:
                actual = False
            results.append({'name': name, 'accepted': actual,
                            'expected': accepted, 'passed': actual == accepted})
    print(json.dumps({'selection_tests': results}), flush=True)
    return 0 if all(row['passed'] for row in results) else 1


BASE = Path(__file__).resolve().parent
CHECKER = BASE.parents[2] / 'scripts/ci/statecharts_tlc.py'
SPEC = importlib.util.spec_from_file_location('lease_common_tlc', CHECKER)
common = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = common
SPEC.loader.exec_module(common)


def load_expectations(root):
    rows = common.read_expected(root / 'expected.tsv')
    # admission/ is its own model; this adapter owns only the top-level lease configs.
    scoped = {cfg: value for cfg, value in rows.items()
              if cfg.startswith('e2e-run-lease/') and cfg.count('/') == 1}
    wanted = {f'e2e-run-lease/{name}' for name in EXPECTED_CONFIGS}
    if set(scoped) != wanted:
        raise ValueError(f'incomplete canonical lease expectations: missing={sorted(wanted - set(scoped))} '
                         f'unexpected={sorted(set(scoped) - wanted)}')
    return scoped


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--java', default=os.environ.get('JAVA', 'java'))
    parser.add_argument('--jar', default=os.environ.get('TLA2TOOLS'))
    parser.add_argument('--out', type=Path)
    parser.add_argument('--timeout', type=float, default=300)
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--self-test-selection', action='store_true')
    args = parser.parse_args()
    if args.self_test_selection:
        return self_test_selection()
    if args.out is None or not args.jar or not Path(args.jar).is_file():
        parser.error('--out and an existing --jar or TLA2TOOLS are required')
    if args.jobs < 1 or args.timeout <= 0:
        parser.error('--jobs and --timeout must be positive')
    try:
        configs = select_configs(BASE)
        expected = load_expectations(BASE.parent)
    except (ValueError, OSError, common.CheckError) as error:
        parser.error(str(error))
    args.out.mkdir(parents=True, exist_ok=False)
    spec_hash = hashlib.sha256((BASE / 'LeaseLifetime.tla').read_bytes()).hexdigest()

    def check(cfg):
        relative = f'e2e-run-lease/{cfg.name}'
        module, rules = expected[relative]
        result = common.check_cfg(BASE.parent, relative, module, rules, args.java,
                                  Path(args.jar).resolve(), args.timeout)
        problems = result.problems()
        row = dict(config=cfg.name, expected_rules=sorted(rules), broken_rules=sorted(result.broken),
                   error=result.error, problems=problems, matches_expectation=not problems,
                   runs=result.runs, seconds=result.seconds, spec_sha256=spec_hash,
                   config_sha256=hashlib.sha256(cfg.read_bytes()).hexdigest())
        (args.out / (cfg.stem + '.log')).write_text(json.dumps(row, indent=2) + '\n')
        return row

    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        results = list(pool.map(check, configs))
    for row in results:
        print(json.dumps(row), flush=True)
    (args.out / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
    return 0 if all(row['matches_expectation'] for row in results) else 1


if __name__ == '__main__':
    raise SystemExit(main())
