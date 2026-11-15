#!/usr/bin/env python3
"""Regenerate only the continuous planning manifest's YAML catalog snapshot."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import shlex
import sys

import yaml

ROOT = Path(__file__).resolve().parents[2]
SCENARIOS = Path('tests/e2e/tui_relay/scenarios')
MANIFEST = Path('docs/e2e-continuous/bundles.schema1.json')


def build_catalog(root: Path, scenario_dir: Path | None = None) -> dict:
    root = Path(root).resolve()
    scenario_dir = root / SCENARIOS if scenario_dir is None else scenario_dir
    paths = sorted(scenario_dir.glob('*.yaml'))
    if not paths:
        raise ValueError(f'empty scenario inventory: {scenario_dir}')
    result = {}
    for path in paths:
        raw = path.read_bytes()
        scenario = yaml.safe_load(raw)
        if not isinstance(scenario, dict):
            raise ValueError(f'scenario must be a mapping: {path}')
        identifier = scenario.get('id')
        if not isinstance(identifier, str) or not identifier:
            raise ValueError(f'missing scenario id: {path}')
        if identifier in result:
            raise ValueError(f'duplicate scenario id {identifier}: {path}')
        coverage = scenario.get('coverage_class')
        cells = scenario.get('cells')
        if not isinstance(coverage, str) or not isinstance(cells, list) or any(not isinstance(cell, str) for cell in cells):
            raise ValueError(f'invalid coverage_class or cells: {path}')
        result[identifier] = dict(path=path.relative_to(root).as_posix(),
                                  sha256=hashlib.sha256(raw).hexdigest(),
                                  coverage_class=coverage, cells=cells)
    return dict(sorted(result.items()))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--scenario-dir', type=Path, default=SCENARIOS)
    parser.add_argument('--manifest', type=Path, default=MANIFEST)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--check', action='store_true', help='reject catalog drift without writing')
    mode.add_argument('--write', action='store_true', help='regenerate only scenario_catalog')
    args = parser.parse_args(argv)
    root = args.root.resolve()
    scenario_dir = args.scenario_dir if args.scenario_dir.is_absolute() else root / args.scenario_dir
    manifest = args.manifest if args.manifest.is_absolute() else root / args.manifest
    try:
        plan = json.loads(manifest.read_text())
        if not isinstance(plan, dict) or not isinstance(plan.get('scenario_catalog'), dict):
            raise ValueError(f'manifest lacks a scenario_catalog object: {manifest}')
        generated = build_catalog(root, scenario_dir)
        if plan['scenario_catalog'] == generated:
            print(f'CATALOG_OK scenarios={len(generated)}')
            return 0
        if args.check:
            command = ['python3', 'scripts/e2e/generate_continuous_catalog.py', '--write', '--root', str(root),
                       '--scenario-dir', str(scenario_dir), '--manifest', str(manifest)]
            print(f'catalog drift: {manifest}\nRegenerate: {shlex.join(command)}', file=sys.stderr)
            return 1
        plan['scenario_catalog'] = generated
        manifest.write_text(json.dumps(plan, ensure_ascii=False, indent=2) + '\n')
        print(f'CATALOG_UPDATED scenarios={len(generated)} manifest={manifest}')
        return 0
    except (OSError, ValueError, yaml.YAMLError) as error:
        print(f'catalog generation failed: {error}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
