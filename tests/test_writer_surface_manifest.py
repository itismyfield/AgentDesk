"""Mutation tests for the exact dormant writer-surface census."""
from __future__ import annotations
import importlib.util, json, sys, unittest
from pathlib import Path

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/"scripts"))
SPEC=importlib.util.spec_from_file_location("writer_surface",ROOT/"scripts/check_writer_surface_manifest.py")
checker=importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(checker)
BASE=(ROOT/checker.MANIFEST).read_text(encoding="utf-8")

def mutate(ident: str, **changes: str) -> str:
    rows=[json.loads(line) for line in BASE.splitlines()]
    for row in rows:
        if row["id"]==ident: row.update(changes)
    return "\n".join(json.dumps(row,separators=(",",":")) for row in rows)+"\n"

class WriterSurfaceManifestTests(unittest.TestCase):
    def test_real_tree_passes(self) -> None:
        self.assertEqual(checker.check(ROOT),[])

    def test_each_critical_row_deletion_fails(self) -> None:
        for ident in checker.EXPECTED:
            with self.subTest(ident=ident):
                text="\n".join(line for line in BASE.splitlines() if json.loads(line)["id"]!=ident)
                self.assertTrue(checker.check_manifest_text(text))

    def test_identity_and_native_disposition_remaps_fail(self) -> None:
        cases=(
            mutate("process-create",symbol="decoy"),
            mutate("claude-native-path",disposition="DormantManaged"),
            mutate("gemini-no-local",provider="Claude"),
            mutate("opencode-no-local",artifact="RelayJsonl"),
            mutate("unsupported-unknown",disposition="Observed"),
        )
        for text in cases:
            with self.subTest(text=text[:80]): self.assertTrue(checker.check_manifest_text(text))

    def test_operation_columns_fail_closed(self) -> None:
        for ident,field in (("rotating-reopen","reopen"),("owned-rotate","rotate"),("truncate","truncate"),("temp-cleanup","cleanup")):
            with self.subTest(ident=ident):
                self.assertTrue(checker.check_manifest_text(mutate(ident,**{field:""})))

    def test_comments_strings_and_unused_macros_are_not_declarations(self) -> None:
        symbol="missing_surface"
        decoys=(f"// fn {symbol}() {{}}",f'const X: &str = "fn {symbol}() {{}}";',f"macro_rules! unused {{ () => {{ fn {symbol}() {{}} }} }}")
        for source in decoys:
            with self.subTest(source=source): self.assertFalse(checker.declaration_exists(source,symbol))

    def test_live_declarations_count(self) -> None:
        self.assertTrue(checker.declaration_exists("pub(crate) fn live_surface() {}","live_surface"))
        self.assertTrue(checker.declaration_exists("struct LiveSurface;","LiveSurface"))

if __name__=="__main__": unittest.main()
