import importlib.util
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("hrot_sender", ROOT / "scripts/check_hrot_sender_surface.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)

OTHER = "src/services/discord/other.rs"
ACTOR = "src/services/tui_o/writer/actor/transition.rs"


class SenderSurfaceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.sources = guard.tree()

    def with_source(self, path, text):
        sources = dict(self.sources)
        sources[path] = sources.get(path, "") + text
        return guard.errors(sources)

    def test_tree_passes(self):
        self.assertEqual(guard.errors(self.sources), [])

    def test_another_sender_is_red(self):
        for path, text in (
            (OTHER, "use crate::services::tui_o::writer::transition_port::Operation;\n"
                    "fn f(p: P) { p.request(Operation::Inspect(r), c); }\n"),
            (ACTOR, "\nfn f(p: P) { p.request(Operation::Inspect(r), c); }\n"),
            (ACTOR, "\nfn f(p: P) { p.request_as(&p, o, c); }\n"),
            (OTHER, "use crate::services::tui_o::writer::host::process_readiness;\n"
                    "fn f() { process_readiness().transition(1); }\n"),
            (OTHER, "fn f(r: R) { r.with_transition_reply(1, &p, reply, |_| ()); }\n"),
            (ACTOR, "\nfn f() { let (p, r) = TransitionPort::new(1, g, w); }\n"),
            (ACTOR, "\nuse super::transition_port::Operation as Op;\n"),
        ):
            with self.subTest(text=text[-60:]):
                self.assertTrue(self.with_source(path, text))

    def test_route_second_send_is_red(self):
        text = "\nasync fn f(p: P) { p.request(o, c).await; }\n"
        self.assertTrue(self.with_source(guard.ROUTE, text))

    def test_apply_is_built_only_in_the_route(self):
        built = "\nfn f(r: R, d: D) -> Operation { Operation::Apply(make(r), d) }\n"
        self.assertEqual(self.with_source(guard.ROUTE, built), [])
        for path in (ACTOR, OTHER):
            with self.subTest(path=path):
                self.assertTrue(self.with_source(path, built))
        arm = "\nfn f(o: Operation) { match o { Operation::Apply(r, (d)) => (), _ => () } }\n"
        self.assertEqual(self.with_source(ACTOR, arm), [])

    def test_test_gated_and_comment_mentions_pass(self):
        gated = ("\n#[cfg(test)]\nfn f(p: P) { p.request(Operation::Inspect(r), c); }\n"
                 "// p.request_as(&p, Operation::Apply(r), c)\n")
        self.assertEqual(self.with_source(ACTOR, gated), [])


if __name__ == "__main__":
    unittest.main()
