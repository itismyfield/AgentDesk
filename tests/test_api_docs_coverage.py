"""Unit tests for scripts/check_api_docs_coverage.py."""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "check_api_docs_coverage.py"

_SPEC = importlib.util.spec_from_file_location("check_api_docs_coverage", SCRIPT_PATH)
CHECKER = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
sys.modules[_SPEC.name] = CHECKER
_SPEC.loader.exec_module(CHECKER)


def pair(method: str, path: str) -> CHECKER.EndpointPair:
    return CHECKER.EndpointPair(method, path)


class ApiDocsCoverageTest(unittest.TestCase):
    def test_reports_missing_and_stale_pairs(self) -> None:
        report = CHECKER.build_coverage_report(
            mounted=[pair("GET", "/api/mounted")],
            docs=[pair("POST", "/api/stale")],
            allowlist={},
        )

        self.assertEqual(report.missing, (pair("GET", "/api/mounted"),))
        self.assertEqual(report.stale, (pair("POST", "/api/stale"),))

    def test_path_parameter_names_compare_by_shape(self) -> None:
        report = CHECKER.build_coverage_report(
            mounted=[pair("GET", "/api/items/{id}")],
            docs=[pair("GET", "/api/items/{segment}")],
            allowlist={},
        )

        self.assertTrue(report.is_clean(), CHECKER.format_report(report))

    def test_allowlist_requires_non_empty_reason(self) -> None:
        report = CHECKER.build_coverage_report(
            mounted=[pair("GET", "/api/internal")],
            docs=[],
            allowlist={("GET", "/api/internal"): "   "},
        )

        self.assertIn(
            "GET /api/internal: allowlist reason must be non-empty",
            report.allowlist_errors,
        )

    def test_allowlist_is_exact_and_rejects_globs(self) -> None:
        report = CHECKER.build_coverage_report(
            mounted=[pair("GET", "/api/items/{id}")],
            docs=[],
            allowlist={
                ("GET", "/api/items/{segment}"): "wrong parameter name",
                ("GET", "/api/admin/*"): "too broad",
            },
        )

        self.assertEqual(report.missing, (pair("GET", "/api/items/{id}"),))
        self.assertIn(pair("GET", "/api/items/{segment}"), report.unused_allowlist)
        self.assertIn(pair("GET", "/api/admin/*"), report.unused_allowlist)
        self.assertIn(
            "GET /api/admin/*: allowlist entries must be exact, not globs",
            report.allowlist_errors,
        )

    def test_allowlist_entry_becomes_unused_when_docs_cover_route(self) -> None:
        report = CHECKER.build_coverage_report(
            mounted=[pair("GET", "/api/internal")],
            docs=[pair("GET", "/api/internal")],
            allowlist={("GET", "/api/internal"): "internal-only"},
        )

        self.assertEqual(report.unused_allowlist, (pair("GET", "/api/internal"),))

    def test_parses_docs_ep_entries(self) -> None:
        with TemporaryDirectory() as tmp:
            docs = Path(tmp) / "docs.rs"
            docs.write_text(
                textwrap.dedent(
                    """
                    fn ep(method: &'static str, path: &'static str) {}

                    fn all_endpoints() {
                        vec![
                            ep(
                                "GET",
                                "/api/example/{id}",
                                "category",
                                "description",
                            ),
                            ep("POST", "/api/other", "category", "description"),
                        ];
                    }
                    """
                ).lstrip("\n"),
                encoding="utf-8",
            )

            self.assertEqual(
                CHECKER.parse_docs_endpoints(docs),
                [pair("GET", "/api/example/{id}"), pair("POST", "/api/other")],
            )

    def test_parser_ignores_ep_entries_outside_all_endpoints(self) -> None:
        with TemporaryDirectory() as tmp:
            docs = Path(tmp) / "docs.rs"
            docs.write_text(
                textwrap.dedent(
                    """
                    fn all_endpoints() {
                        vec![ep("GET", "/api/documented", "category", "description")];
                    }

                    #[cfg(test)]
                    mod tests {
                        fn helper() {
                            let _ = ep("POST", "/api/test-only", "category", "description");
                        }
                    }
                    """
                ).lstrip("\n"),
                encoding="utf-8",
            )

            self.assertEqual(
                CHECKER.parse_docs_endpoints(docs),
                [pair("GET", "/api/documented")],
            )

    def test_mounted_route_collection_includes_v1_router(self) -> None:
        mounted = set(CHECKER.collect_mounted_api_endpoints())

        self.assertIn(pair("GET", "/api/v1/overview"), mounted)

    def test_generated_route_inventory_includes_v1_router(self) -> None:
        route_inventory = CHECKER.inventory.generated_documents()[
            CHECKER.inventory.GENERATED_DOCS_DIR / "route-inventory.md"
        ]

        self.assertIn("| `GET` | `/api/v1/overview` |", route_inventory)

    def test_mounted_route_source_paths_follow_compose_api_router(self) -> None:
        with TemporaryDirectory() as tmp:
            routes_root = Path(tmp) / "src" / "server" / "routes"
            domains_root = routes_root / "domains"
            domains_root.mkdir(parents=True)
            (domains_root / "mounted.rs").write_text("", encoding="utf-8")
            (domains_root / "unmounted.rs").write_text("", encoding="utf-8")
            (routes_root / "v1.rs").write_text("", encoding="utf-8")
            routes_mod = routes_root / "mod.rs"
            routes_mod.write_text(
                textwrap.dedent(
                    """
                    fn compose_api_router(state: AppState) -> ApiRouter {
                        Router::new()
                            .merge(domains::mounted::router(state.clone()))
                            .merge(v1::router(state))
                    }
                    """
                ).lstrip("\n"),
                encoding="utf-8",
            )

            source_paths = CHECKER.inventory.mounted_api_route_source_paths(
                routes_mod, routes_root
            )

            self.assertEqual(
                [path.relative_to(routes_root).as_posix() for path in source_paths],
                ["domains/mounted.rs", "v1.rs"],
            )

    def test_current_repo_api_docs_coverage_passes(self) -> None:
        report = CHECKER.build_coverage_report()

        self.assertTrue(report.is_clean(), CHECKER.format_report(report))


if __name__ == "__main__":
    unittest.main()
