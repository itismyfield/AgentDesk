#!/usr/bin/env python3
"""#6210 r6 PR-0 초안: relay 부작용 호출 지점 inventory pin (커밋하지 않은 초안).

precommit-block 3-1 근거: 호출 지점 집합은 런타임 경로가 아니라 소스의 성질이라, 동작 테스트는 자기가 아는 경로만 밟는다. r3~r5에서 매번 표 밖 sender가 나온 공백이 그것이다.

무엇을 막는가
    gen_inventory.py 가 현재 소스에서 뽑은 호출 지점 키 집합을 baseline(inventory.tsv)과 비교한다.
      - baseline 에 없는 새 키  → 실패: "inventory.tsv 에 분류를 추가하라"
      - 소스에서 사라진 키      → 실패: "baseline 을 갱신하라"
      - baseline 행의 판정 열(관리채널 도달·(B) 요소·현재 retire 주체)이 비어 있음 → 실패
      - '불명' 행 수가 baseline 헤더의 상한(UNKNOWN_CEILING)보다 많아짐 → 실패(래칫)
    키는 (범주, 파일, 함수, callee, 순번) 이다. 줄 번호는 넣지 않는다: 무관한 편집으로 줄이 밀려도
    통과하고, 같은 함수 안에서 같은 callee 호출이 하나 늘거나 줄면 순번이 바뀌어 잡힌다.

    이름 규칙에 안 걸린 fn 은 호출부를 한 단계만 따라가므로(gen_inventory 머리의 "한 단계 규칙"),
    wrappers.tsv 의 fn 목록(kind, 범주, 이름)도 같이 고정한다. primitive 를 품은 새 fn 은 그 fn 의
    호출부가 inventory 에 잡히지 않더라도 여기서 새 이름으로 드러난다.

실행
    python3 scripts/check_relay_side_effect_inventory.py            # 저장소 루트에서
    python3 -m pytest scripts/check_relay_side_effect_inventory.py  # 같은 검사를 pytest 로
    환경변수 RELAY_INVENTORY_REPO 로 저장소를, RELAY_INVENTORY_DIR 로 baseline 디렉터리를, RELAY_INVENTORY_GEN 으로 생성기 경로를 바꾼다.

baseline 갱신 절차(PR 작성자)
    1. python3 <gen> <repo> --meta wrappers.tsv > inventory.new.tsv
    2. 새 키 행의 판정 열을 채워 inventory.tsv 에 옮긴다(기계 열은 생성기 출력 그대로 둔다).
    3. 사라진 키는 inventory.tsv 에서 지운다. 리뷰어는 이 diff 로 sender 추가·삭제를 본다.
"""
from __future__ import annotations

import csv
import importlib.util
import io
import os
import sys
from collections import Counter
from pathlib import Path

sys.dont_write_bytecode = True

REPO = Path(os.environ.get("RELAY_INVENTORY_REPO") or (
    Path(__file__).resolve().parents[1] if Path(__file__).parent.name == "scripts" else Path.cwd()
)).resolve()
BASELINE_DIR = Path(os.environ.get("RELAY_INVENTORY_DIR", REPO / "scripts" / "relay_inventory"))
GEN_PATH = Path(os.environ.get("RELAY_INVENTORY_GEN", BASELINE_DIR / "gen_inventory.py"))

KEY_COLUMNS = ("범주", "file", "함수", "callee", "순번")
JUDGMENT_COLUMNS = ("관리채널 도달", "(B) 요소", "현재 retire 주체")
REACH_VALUES = frozenset({"예", "아니오", "불명"})
# baseline 작성 시점의 '불명' 행 수. 줄일 때만 같이 내린다.
UNKNOWN_CEILING = int(os.environ.get("RELAY_INVENTORY_UNKNOWN_CEILING", "661"))


def _load_generator():
    spec = importlib.util.spec_from_file_location("relay_gen_inventory", GEN_PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _key(row: dict) -> tuple:
    return (row["범주"], row["file:line"].rsplit(":", 1)[0], row["함수"], row["callee"], row["순번"])


def _read_tsv(text: str) -> list[dict]:
    return list(csv.DictReader(io.StringIO(text), delimiter="\t", quoting=csv.QUOTE_NONE))


def current_inventory() -> tuple[list[dict], list[tuple]]:
    gen = _load_generator()
    citations = BASELINE_DIR / "r5_citations.tsv"
    rows, wrappers, untraced, _files = gen.build_rows(REPO, citations)
    inv = _read_tsv(gen.render(rows))
    fns = [("wrapper", cat, name) for (cat, name) in wrappers]
    fns += [(kind, cat, name) for (cat, name), (_rel, kind) in untraced.items()]
    return inv, sorted(fns)


def baseline_inventory() -> tuple[list[dict], list[tuple]]:
    inv = _read_tsv((BASELINE_DIR / "inventory.tsv").read_text(encoding="utf-8"))
    fns = []
    for row in _read_tsv((BASELINE_DIR / "wrappers.tsv").read_text(encoding="utf-8")):
        fns.append((row["kind"], row["cat"], row["name"]))
    return inv, sorted(fns)


def _diff(cur: list, base: list) -> tuple[list, list]:
    c, b = Counter(cur), Counter(base)
    return sorted((c - b).elements()), sorted((b - c).elements())


def problems() -> list[str]:
    cur_rows, cur_fns = current_inventory()
    base_rows, base_fns = baseline_inventory()
    out: list[str] = []

    added, vanished = _diff([_key(r) for r in cur_rows], [_key(r) for r in base_rows])
    cur_by_key = {_key(r): r for r in cur_rows}
    for k in added:
        r = cur_by_key[k]
        out.append(
            f"new relay side-effect site {r['file:line']} [{k[0]}] {k[2]} → {k[3]} (#{k[4]}, {r['seed 종류']}): "
            "classify it in inventory.tsv (관리채널 도달 / (B) 요소 / 현재 retire 주체)"
        )
    for k in vanished:
        out.append(f"inventory key no longer in source {k}: update baseline inventory.tsv")

    added_fn, vanished_fn = _diff(cur_fns, base_fns)
    for kind, cat, name in added_fn:
        out.append(f"new {kind} fn [{cat}] {name}: add it to wrappers.tsv and classify its callers")
    for kind, cat, name in vanished_fn:
        out.append(f"{kind} fn [{cat}] {name} no longer found: update baseline wrappers.tsv")

    unknown = 0
    for r in base_rows:
        for col in JUDGMENT_COLUMNS:
            if not (r.get(col) or "").strip():
                out.append(f"baseline row {r['file:line']} {r['함수']} has empty {col}")
        if r.get("관리채널 도달") not in REACH_VALUES:
            out.append(f"baseline row {r['file:line']} has reach {r.get('관리채널 도달')!r} (want 예/아니오/불명)")
        unknown += r.get("관리채널 도달") == "불명"
    if unknown > UNKNOWN_CEILING:
        out.append(f"'불명' rows grew to {unknown} (ceiling {UNKNOWN_CEILING}): decide the new ones instead of parking them")
    return out


def test_relay_side_effect_inventory_is_classified():
    found = problems()
    assert not found, "\n".join(found)


def main() -> int:
    found = problems()
    for line in found:
        print(line, file=sys.stderr)
    if found:
        print(f"relay side-effect inventory: {len(found)} problem(s)", file=sys.stderr)
        return 1
    print("relay side-effect inventory: OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
