# Required-id exception fragments

New exceptions for the required-PR new-test-ID gate go here, never into
`scripts/test_lane_required_id_exceptions.txt`. `scripts/lane_new_id_registration.py`
reads that legacy file and every fragment here as one set.

- One file per lane slice, named `<node>-<issue>-<slice>.txt` in lowercase, for
  example `mac-mini-6950-p1.txt`. Only this README and such files may sit in this
  directory; any other entry (a `.text` typo, a subdirectory, a symlink) fails the
  gate, so no row can hide in a file the reader skips.
- The row format is unchanged, one per line and sorted by id within the file:
  `exact-test-id | mode | #issue | expires=YYYY-MM-DD | reason` (for `helper-module`,
  the exact module path). Lines starting with `#` are comments.
- An id may appear once across the legacy file and all fragments. A second row fails
  as a duplicate (same fields) or a conflict (other fields), naming both `file:line`.
- Expiry, staleness, mode and helper-module checks apply to every row wherever it
  lives, and findings point at the row's own `file:line`.

Lane rule:

> 등록은 `<목록>.d/<node>-<issue>-<slice>.txt`에만 추가하고, 새 회귀 시험은 새 파일·모듈로 작성하며, giant 생산 줄 수를 늘리지 않는다; 기존 key 이동은 원본 삭제와 조각 추가를 같은 PR에 넣는다.
