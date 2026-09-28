"""Items mapping reads one sealed fake session; no compiler runs."""
from __future__ import annotations

import json
import unittest
from pathlib import Path

from tests import test_h2_session as harness
import h2_items as items

FIXTURE = harness.ROOT / "tests/fixtures/h2_items"
SOURCE = (FIXTURE / "lib.rs").read_text(encoding="utf-8")
RECORDS = json.loads((FIXTURE / "items.json").read_text(encoding="utf-8"))
DIAGNOSTICS = json.loads((FIXTURE / "clippy.json").read_text(encoding="utf-8"))
LISTING = ("git", "ls-files", "-c", "-o", "--exclude-standard", "-z")
FILE = "rust/library.rs"


def encode(crlf: bool) -> bytes:
    return b"\xef\xbb\xbf" + SOURCE.replace("\n", "\r\n").encode() if crlf else SOURCE.encode()


def locate(text: str, raw: bytes) -> int:
    needle = text.encode()
    assert raw.count(needle) == 1, text
    return raw.index(needle)


def line(raw: bytes, offset: int) -> int:
    return raw.count(b"\n", 0, offset) + 1


def row(record: dict, raw: bytes) -> list:
    lo = locate(record["anchor"], raw)
    kind = record["kind"]
    def_kind = record.get("def_kind", "Fn" if kind in ("fn", "nested_fn") else "AssocFn")
    return [FILE, lo, lo + len(record["anchor"].encode()), kind, record.get("path"), record.get("reason"),
            record["display"], line(raw, lo), record["def"], record.get("parent", 0), def_kind, "!" in record["anchor"]]


def diagnostic(case: dict, raw: bytes) -> dict:
    prefix, rest = case["anchor"].split("«")
    inner = rest.split("»")[0]
    lo = locate(case["anchor"].replace("«", "").replace("»", ""), raw) + len(prefix.encode())
    span = dict(file_name=FILE, byte_start=lo, byte_end=lo + len(inner.encode()), line_start=line(raw, lo),
                is_primary=True, expansion=None)
    if "!" in inner:  # the expanded span points at sink(); only its outermost call site is the site
        sink = locate("pub fn sink", raw)
        span = dict(file_name=FILE, byte_start=sink, byte_end=sink + 6, line_start=line(raw, sink),
                    is_primary=True, expansion=dict(span=span, macro_decl_name=inner.split("!")[0] + "!"))
    message = dict(message="disallowed", code=dict(code="clippy::disallowed_methods"), level="warning", spans=[span])
    return dict(reason="compiler-message", package_id="path+file:///fixture#0.0.0", message=message)


class Items(unittest.TestCase):
    def setUp(self):
        self.h = harness.Session("run_session")
        self.h.setUp()
        self.addCleanup(self.h.doCleanups)
        self.h.answers[LISTING] = "crate/Cargo.toml\0crate/rust/library.rs\0conf/clippy.toml\0"

    def seal(self, *, crlf=False, normalized=False, rows=None, cases=DIAGNOSTICS, name="run") -> Path:
        raw = encode(crlf)
        self.h.lib.write_bytes(raw)
        coords = SOURCE.encode() if normalized else raw
        body_rows = rows if rows is not None else [row(r, coords) for r in RECORDS]

        def mutate(run, proof, claim, events):
            request = json.loads((run / "request.json").read_text())
            header = dict(schema=1, run_id=request["run_id"], nonce=request["nonce"], kind="canary-items",
                          root=str(self.h.crate), crate="fixture", cfg_clippy=True)
            body = (json.dumps(header) + "\n" + "".join(json.dumps(r) + "\n" for r in body_rows)).encode()
            (run / "items.jsonl").write_bytes(body)
            proof.update(items_sha256=harness.s.collect.digest(body), items_records=len(body_rows))
            (run / "items.jsonl.sha256").write_text(json.dumps(dict(sha256=proof["items_sha256"], records=len(body_rows))))
            events.extend(diagnostic(case, raw) for case in cases)
        self.h.mutate = mutate
        return Path(self.h.run_session(name)["manifest"])

    def load(self, manifest: Path):
        return items.load(manifest, crate=self.h.crate)

    def rewrite(self, manifest: Path, files: dict, **fields) -> None:
        value = json.loads(manifest.read_text())
        for name, body in files.items():
            (manifest.parent / name).write_bytes(body)
            value["digests"][name] = harness.s.collect.digest(body)
        value["proof"] = json.loads((manifest.parent / "session.json").read_text())
        value["request"] = json.loads((manifest.parent / "request.json").read_text())
        value.update(fields)
        harness.s.collect.write_json(manifest, value)

    def rewrite_items(self, manifest: Path, body: bytes, records: int) -> None:
        proof = json.loads((manifest.parent / "session.json").read_text())
        proof.update(items_sha256=harness.s.collect.digest(body), items_records=records)
        self.rewrite(manifest, {"items.jsonl": body, "session.json": json.dumps(proof).encode(),
                                "items.jsonl.sha256": json.dumps(dict(sha256=proof["items_sha256"], records=records)).encode()})

    def assert_reason(self, reason, call, *args):
        with self.assertRaises(items.MappingError) as caught:
            call(*args)
        self.assertEqual(caught.exception.reason, reason)

    def test_fixture_sites_map_in_lf_and_bom_crlf(self):
        for crlf in (False, True):
            with self.subTest(crlf=crlf):
                loaded = self.load(self.seal(crlf=crlf, name=f"run-{crlf}"))
                raw = self.h.lib.read_bytes()
                self.assertEqual(raw.startswith(b"\xef\xbb\xbf") and b"\r\n" in raw, crlf)
                self.assertEqual(len(loaded.messages), len(DIAGNOSTICS))
                for message, case in zip(loaded.messages, DIAGNOSTICS):
                    span = items.primary(message)
                    if "error" in case:
                        self.assert_reason(case["error"], items.resolve, loaded, span)
                    else:
                        self.assertEqual(items.resolve(loaded, span), tuple(case["expect"]), case["anchor"])

    def test_normalized_offsets_fail_as_coord_on_bom_crlf(self):
        raw, normal = encode(True), SOURCE.encode()
        lo = locate("{ sink()", raw) + 2
        decoy = items.map_site([dict(zip(harness.s.modmap.ITEM_FIELDS, row(r, normal))) for r in RECORDS[:4]], lo, lo + 6)
        self.assertEqual(decoy["display"], "decoy")
        self.assert_reason("coord", self.load, self.seal(crlf=True, normalized=True))
        lf = self.load(self.seal(normalized=True, name="lf"))
        self.assertEqual(items.resolve(lf, items.primary(lf.messages[0])), ("fixture::inside", None))

    def test_site_and_record_coordinates_are_checked(self):
        loaded = self.load(self.seal(crlf=True))
        span = items.primary(loaded.messages[0])
        for change in (dict(line_start=span["line_start"] + 1), dict(byte_end=len(self.h.lib.read_bytes()) + 1),
                       dict(byte_start=span["byte_start"] - 40)):
            self.assert_reason("coord", items.resolve, loaded, {**span, **change})
        for bad in ({**span, "byte_start": True}, {**span, "byte_start": span["byte_end"] + 1}, {**span, "file_name": None},
                    {**span, "expansion": {"span": "x"}}):
            with self.assertRaises(items.MeasureError):
                items.resolve(loaded, bad)
        raw = encode(False)
        long = row(RECORDS[0], raw)
        long[2] = len(raw) + 1
        self.assert_reason("coord", self.load, self.seal(rows=[long], cases=(), name="beyond"))

    def test_unmapped_unsealed_and_spanless_sites_fail(self):
        outside = row(RECORDS[0], SOURCE.encode())
        outside[0] = "gen/out.rs"
        (self.h.crate / "gen").mkdir()
        (self.h.crate / "gen/out.rs").write_text(SOURCE)
        loaded = self.load(self.seal(rows=[row(r, SOURCE.encode()) for r in RECORDS] + [outside[:8] + [99] + outside[9:]]))
        span = items.primary(loaded.messages[0])
        gap = self.h.lib.read_bytes().index("/* 한글".encode())
        self.assert_reason("no-item", items.resolve, loaded, {**span, "byte_start": gap, "byte_end": gap + 2, "line_start": 3})
        self.assert_reason("no-item", items.resolve, loaded, {**span, "file_name": "rust/other.rs"})
        self.assert_reason("unsealed", items.resolve, loaded, {**span, "file_name": "gen/out.rs"})
        for message in (dict(spans=[]), dict(spans=[{**span, "is_primary": False}]), dict()):
            with self.assertRaises(items.MeasureError):
                items.primary(message)

    def test_ties_are_order_independent_and_overlap_is_ambiguous(self):
        by_def = {r["def"]: dict(zip(harness.s.modmap.ITEM_FIELDS, row(r, SOURCE.encode()))) for r in RECORDS}
        for defs, reason in (((20, 21), "ambiguous:C,unrelated"), ((31, 33, 30), "ambiguous:E::A::{constant#0},unrelated2")):
            for order in (defs, defs[::-1]):
                self.assert_reason(reason, items.resolve_tie, [by_def[d] for d in order])
        self.assertEqual(items.resolve_tie([by_def[11], by_def[10]])["def"], 11)
        a, b = dict(by_def[1], lo=10, hi=30), dict(by_def[2], lo=12, hi=32)
        self.assert_reason("ambiguous:overlap", items.map_site, [a, b], 15, 20)

    def test_load_rejects_forged_or_foreign_sessions(self):
        other = self.h.root / "other"
        other.mkdir()
        (other / "Cargo.toml").write_text("")
        rows = [row(r, SOURCE.encode()) for r in RECORDS]
        cases = {
            "manifest kind": ("h2-session/2", lambda: self.rewrite(manifest, {}, kind="root")),
            "manifest schema": ("h2-session/2", lambda: self.rewrite(manifest, {}, schema="1")),
            "proof schema": ("h2-session/2", lambda: self.rewrite(manifest, {"session.json": json.dumps({**proof, "schema": "h2-session/1-cfg"}).encode()})),
            "request schema": ("h2-session/2", lambda: self.rewrite(manifest, {"request.json": json.dumps({k: v for k, v in request.items() if k != "schema"}).encode()})),
            "proof unit": ("unit", lambda: self.rewrite(manifest, {"session.json": json.dumps({**proof, "unit": {**proof["unit"], "lib": "/x.rs"}}).encode()})),
            "proof unit root": ("unit", lambda: self.rewrite(manifest, {"session.json": json.dumps({**proof, "unit": {**proof["unit"], "root": "/x"}}).encode()})),
            "proof unit test": ("unit", lambda: self.rewrite(manifest, {"session.json": json.dumps({**proof, "unit": {**proof["unit"], "test": True}}).encode()})),
            "request kind": ("h2-session/2", lambda: self.rewrite(manifest, {"request.json": json.dumps({**request, "kind": "root"}).encode()})),
            "proof nonce": ("nonce", lambda: self.rewrite(manifest, {"session.json": json.dumps({**proof, "nonce": "0" * 32}).encode()})),
            "inline proof": ("request/proof", lambda: (self.rewrite(manifest, {}), manifest.write_text(manifest.read_text().replace('"pid": 42', '"pid": 43')))),
            "items bytes": ("sealed digests", lambda: (run / "items.jsonl").write_bytes((run / "items.jsonl").read_bytes() + b"[]\n")),
            "clippy bytes": ("sealed digests", lambda: (run / "clippy.jsonl").write_text("{}\n")),
            "object records": ("session items", lambda: self.rewrite_items(manifest, (json.dumps(header) + "\n" + "".join(
                json.dumps(dict(zip(harness.s.modmap.ITEM_FIELDS, r))) + "\n" for r in rows)).encode(), len(rows))),
            "headerless JSONL": ("session items", lambda: self.rewrite_items(manifest, "".join(json.dumps(r) + "\n" for r in rows).encode(), len(rows) - 1)),
            "zero records": ("session items", lambda: self.rewrite_items(manifest, (json.dumps(header) + "\n").encode(), 0)),
            "partial": ("partial", lambda: (run / "items.jsonl.partial").write_text("")),
            "source changed": ("source changed", lambda: self.h.lib.write_bytes(SOURCE.encode() + b"\n")),
            "moved manifest": ("manifest path", lambda: None),
        }
        for label, (pattern, forge) in cases.items():
            with self.subTest(label):
                manifest = self.seal(name=label.replace(" ", "-"))
                run = manifest.parent
                proof, request = (json.loads((run / n).read_text()) for n in ("session.json", "request.json"))
                header = json.loads((run / "items.jsonl").read_bytes().splitlines()[0])
                forge()
                if label == "moved manifest":
                    moved = run / "copy.json"
                    moved.write_bytes(manifest.read_bytes())
                    manifest = moved
                with self.assertRaisesRegex(items.MeasureError, pattern):
                    self.load(manifest)
        with self.assertRaisesRegex(items.MeasureError, "unit"):
            items.load(self.seal(name="foreign"), crate=other)


if __name__ == "__main__":
    unittest.main()
