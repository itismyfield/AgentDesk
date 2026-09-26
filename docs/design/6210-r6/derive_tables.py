#!/usr/bin/env python3
"""inventory.tsv 에서 summary.md 의 표를 결정적으로 뽑는다(소스를 다시 스캔하지 않는다).

    python3 derive_tables.py [inventory.tsv] [r5_citations.tsv]

출력(같은 디렉터리):
    coverage.tsv       r5·리뷰 인용 행마다 inventory 적중 수. 적중 0 행이 summary.md 의 '적중 0 사유' 표 대상
    unknown_by_fn.tsv  '불명' 행을 (사유, 모듈, file::함수) 로 묶은 것
    missing_by_fn.tsv  r5 행 = '누락' 을 (범주, file::함수) 로 묶은 것
    stats.txt          범주별 계수·seed 종류·도달·누락 계수
"""
import csv
import sys
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
inv_path = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "inventory.tsv"
cit_path = Path(sys.argv[2]) if len(sys.argv) > 2 else HERE / "r5_citations.tsv"
out_dir = inv_path.parent

rows = list(csv.DictReader(inv_path.open(encoding="utf-8"), delimiter="\t", quoting=csv.QUOTE_NONE))
by_file = defaultdict(list)
for r in rows:
    f, ln = r["file:line"].rsplit(":", 1)
    r["_file"], r["_line"] = f, int(ln)
    by_file[f].append(r)


def module_of(rel: str) -> str:
    parts = rel.split("/")
    if parts[:3] == ["src", "services", "discord"] and len(parts) > 4:
        return parts[3]
    if parts[:2] == ["src", "services"] and len(parts) > 3:
        return parts[2]
    return "/".join(parts[1:3]).removesuffix(".rs")


# coverage.tsv
cov = ["tag\tsource\tpath\tspans\thits\ttag_rows_in_file"]
zero = 0
for line in cit_path.read_text(encoding="utf-8").splitlines():
    if not line.strip() or line.startswith("#"):
        continue
    tag, src, path, spans = line.split("\t")[:4]
    hits = 0
    for sp in (s.strip() for s in spans.split(",")):
        if not sp:
            continue
        if sp.startswith("fn:"):
            hits += sum(1 for r in by_file.get(path, []) if r["함수"] == sp[3:])
        else:
            a, _, b = sp.partition("-")
            lo, hi = int(a), int(b or a)
            hits += sum(1 for r in by_file.get(path, []) if lo <= r["_line"] <= hi)
    tagged = sum(
        1 for r in by_file.get(path, [])
        if tag in (r["r5 행"] + "," + r["리뷰 태그"]).replace("(fn)", "").split(",")
    )
    zero += hits == 0
    cov.append(f"{tag}\t{src}\t{path}\t{spans}\t{hits}\t{tagged}")
(out_dir / "coverage.tsv").write_text("\n".join(cov) + "\n", encoding="utf-8")

# unknown_by_fn.tsv
unk = defaultdict(lambda: [0, set()])
for r in rows:
    if r["관리채널 도달"] == "불명":
        k = (r["도달 사유"], module_of(r["_file"]), f"{r['_file']}::{r['함수']}")
        unk[k][0] += 1
        unk[k][1].add(r["범주"])
lines = ["도달 사유\t모듈\tfile::함수\t행수\t범주"]
for (why, mod, fn), (n, cats) in sorted(unk.items()):
    lines.append(f"{why}\t{mod}\t{fn}\t{n}\t{''.join(sorted(cats))}")
(out_dir / "unknown_by_fn.tsv").write_text("\n".join(lines) + "\n", encoding="utf-8")

# missing_by_fn.tsv
miss = defaultdict(lambda: [0, set(), set()])
for r in rows:
    if r["r5 행"] == "누락":
        k = (r["범주"], f"{r['_file']}::{r['함수']}")
        miss[k][0] += 1
        miss[k][1].add(r["관리채널 도달"])
        if r["리뷰 태그"] != "-":
            miss[k][2].update(r["리뷰 태그"].split(","))
lines = ["범주\tfile::함수\t행수\t도달\t리뷰 태그"]
for (cat, fn), (n, reach, tags) in sorted(miss.items()):
    lines.append(f"{cat}\t{fn}\t{n}\t{','.join(sorted(reach))}\t{','.join(sorted(tags)) or '-'}")
(out_dir / "missing_by_fn.tsv").write_text("\n".join(lines) + "\n", encoding="utf-8")

# stats.txt
st = []
cats = sorted({r["범주"] for r in rows})
st.append(f"rows {len(rows)} " + " ".join(f"{c}={sum(r['범주'] == c for r in rows)}" for c in cats))
for c in cats:
    cr = [r for r in rows if r["범주"] == c]
    seeds = Counter(r["seed 종류"] for r in cr)
    reach = Counter(r["관리채널 도달"] for r in cr)
    missing = [r for r in cr if r["r5 행"] == "누락"]
    bare = [r for r in missing if r["리뷰 태그"] == "-"]
    fns = {(r["_file"], r["함수"]) for r in cr}
    fns_s = {(r["_file"], r["함수"]) for r in cr if r["r5 행"] != "누락"}
    st.append(
        f"{c}: seed {dict(sorted(seeds.items()))} | reach {dict(sorted(reach.items()))} | "
        f"누락 {len(missing)} (리뷰 태그도 없음 {len(bare)}) | fn {len(fns)} (S 태그 fn 없음 {len(fns - fns_s)})"
    )
files = {r["_file"] for r in rows}
s_files = {r["_file"] for r in rows if r["r5 행"] != "누락"}
cited = {ln.split("\t")[2] for ln in cit_path.read_text(encoding="utf-8").splitlines() if ln.strip() and not ln.startswith("#")}
st.append(f"files {len(files)}; 인용 전혀 없음 {len(files - cited)}; S 적중 없음 {len(files - s_files)}")
st.append(f"누락 {sum(r['r5 행'] == '누락' for r in rows)}; 불명 {sum(r['관리채널 도달'] == '불명' for r in rows)}")
st.append(f"citation rows {len(cov) - 1}; zero-hit {zero}")
(out_dir / "stats.txt").write_text("\n".join(st) + "\n", encoding="utf-8")
print("\n".join(st))
