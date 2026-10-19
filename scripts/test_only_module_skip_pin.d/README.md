# Writer-gate skip pin fragments

New resolver skip pins for the two writer gates go here, never into
`scripts/test_only_module_skip_pin_paths.py`. `scripts/skip_pin_registry.py` reads
that legacy resolver set and every fragment here as one registry. Old combined
legacy sets remain readable. Basename skips follow the `tests.rs` / `*_tests.rs`
rule and reject production module mounts; they need no new path pins.

- One file per lane slice, named `<node>-<issue>-<slice>.txt` in lowercase, for
  example `mac-mini-6950-p1.txt`. Only this README and such files may sit in this
  directory; any other entry (a `.text` typo, a subdirectory, a symlink) fails the
  gates, so no row can hide in a file the reader skips.
- One row per line, sorted by path: `resolver | src/...rs` for a production-looking
  file the shared resolver proves test-only. Existing `basename | src/...rs` rows
  remain readable for compatibility. Lines starting with `#` are comments.
- A path may be pinned once across the legacy sets and all fragments. A second row
  fails as a duplicate (same kind) or a conflict (other kind), naming both `file:line`.
- `python3 scripts/check_legacy_supervision_census.py --sync-skips --fragment <node>-<issue>-<slice>`
  writes the measured new pins to that fragment (`scripts/prepare_gate_review.py --fragment`
  passes it through). Without `--fragment`, each new path gets its own
  `auto-0-<hash>.txt`. `--promote PATH` removes a promoted fragment row;
  `--remove-deleted PATH` (alias `--delete-pin`) removes a missing fragment path.
  Both options validate the request before writing. A legacy row is only
  reported by `file:line`, for deletion by hand in the same change.

Lane rule:

> 등록은 `<목록>.d/<node>-<issue>-<slice>.txt`에만 추가하고, 새 회귀 시험은 새 파일·모듈로 작성하며, giant 생산 줄 수를 늘리지 않는다; 기존 key 이동은 원본 삭제와 조각 추가를 같은 PR에 넣는다.
