#!/usr/bin/env python3
"""Run the bounded TLC matrix and keep machine-readable, fail-closed results."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


RUN_MUTANTS = {"none", "ttl_takeover", "non_atomic", "parent_unlock", "missing_fd",
               "duplicate_child", "no_pending", "clear_on_idle", "send_after_release",
               "leak_fd", "foreign_cleanup"}
INSTANCE_MUTANTS = RUN_MUTANTS - {"no_pending", "clear_on_idle", "send_after_release"}
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
        ('complete_23', EXPECTED_CONFIGS, True),
        ('empty_0', set(), False),
        ('missing_22', EXPECTED_CONFIGS - {'run-none.cfg'}, False),
        ('unknown_singleton', {'unknown.cfg'}, False),
        ('additional_unknown_24', EXPECTED_CONFIGS | {'unknown.cfg'}, False),
    ]
    results = []
    for name, names, accepted in cases:
        with tempfile.TemporaryDirectory(prefix='i8120-tlc-selection-') as temp:
            base = Path(temp)
            for config in names:
                (base / config).touch()
            try:
                selected = select_configs(base)
                actual = len(selected) == 23
            except ValueError:
                actual = False
            results.append({'name': name, 'accepted': actual,
                            'expected': accepted, 'passed': actual == accepted})
    print(json.dumps({'selection_tests': results}), flush=True)
    return 0 if all(row['passed'] for row in results) else 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--java', default=os.environ.get('JAVA', 'java'))
    parser.add_argument('--jar', default=os.environ.get('TLA2TOOLS'))
    parser.add_argument('--out', type=Path)
    parser.add_argument('--self-test-selection', action='store_true')
    args = parser.parse_args()
    if args.self_test_selection:
        return self_test_selection()
    if args.out is None:
        parser.error('--out is required')
    if not args.jar:

        parser.error('--jar or TLA2TOOLS is required')
    base = Path(__file__).resolve().parent
    try:
        configs = select_configs(base)
    except ValueError as error:
        parser.error(str(error))
    args.out.mkdir(parents=True, exist_ok=False)
    spec = base / 'LeaseLifetime.tla'
    expected = {
        'none': None, 'ttl_takeover': 'NoHolderTheft', 'non_atomic': 'NoHolderTheft',
        'parent_unlock': 'NoHolderTheft', 'missing_fd': 'WriterHasInheritedFd',
        'duplicate_child': 'NoDualWriter', 'no_pending': 'UncertainSendJournaled',
        'clear_on_idle': 'UncertainSendJournaled', 'send_after_release': 'NoSendAfterRelease',
        'leak_fd': 'NextRequestProgress', 'foreign_cleanup': 'NoForeignCleanup',
    }
    results = []
    for cfg in configs:
        mutant = cfg.stem.split('-', 2)[-1] if '-live-' in cfg.stem else cfg.stem.split('-', 1)[1]
        command = [args.java, '-Xmx512m', '-XX:+UseParallelGC', '-cp', args.jar, 'tlc2.TLC',
                   '-workers', '1', '-metadir', str(args.out / (cfg.stem + '-states')),
                   '-config', str(cfg), str(spec)]
        result = subprocess.run(command, text=True, capture_output=True, timeout=60)
        output = result.stdout + result.stderr
        (args.out / (cfg.stem + '.log')).write_text(output)
        violation = re.search(r'Invariant (\w+) is violated|Temporal properties were violated', output)
        rule = violation.group(1) if violation and violation.group(1) else None
        if violation and rule is None:
            properties = re.findall(r'^PROPERTY (\w+)$', cfg.read_text(), re.MULTILINE)
            rule = properties[0] if len(properties) == 1 else 'temporal'
        states = re.search(r'(\d+) states generated, (\d+) distinct states found', output)
        good = ('Model checking completed. No error has been found.' in output and result.returncode == 0)
        bad = 'NextRequestStarts' if '-live-leak_fd' in cfg.stem else expected[mutant]
        matches = good if bad is None else (result.returncode != 0 and bool(violation)
                                             and (rule == bad or (bad == 'NextRequestProgress' and rule == 'temporal')))
        row = {'config': cfg.name, 'expected_rule': bad, 'actual_rule': rule,
               'exit': result.returncode, 'status': 'GREEN' if good else 'RED' if violation else 'ERROR',
               'matches_expectation': matches,
               'states_generated': int(states[1]) if states else None,
               'distinct_states': int(states[2]) if states else None,
               'spec_sha256': hashlib.sha256(spec.read_bytes()).hexdigest(),
               'config_sha256': hashlib.sha256(cfg.read_bytes()).hexdigest(),
               'log': str(args.out / (cfg.stem + '.log'))}
        results.append(row)
        print(json.dumps(row), flush=True)
    (args.out / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
    return 0 if all(row['matches_expectation'] for row in results) else 1


if __name__ == '__main__':
    raise SystemExit(main())
