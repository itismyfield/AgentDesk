"""Unit tests for scripts/check_api_docs_coverage.py."""

from __future__ import annotations

import importlib.util
import sys
import textwrap
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

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

    def test_parser_follows_docs_inventory_child_module(self) -> None:
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            docs = tmp_path / "docs.rs"
            docs.write_text("mod inventory;\n", encoding="utf-8")
            inventory_dir = tmp_path / "docs"
            inventory_dir.mkdir()
            (inventory_dir / "inventory.rs").write_text(
                textwrap.dedent(
                    """
                    pub(super) fn all_endpoints() {
                        vec![
                            ep("GET", "/api/from-inventory", "docs", "description"),
                        ];
                    }
                    """
                ).lstrip("\n"),
                encoding="utf-8",
            )

            self.assertEqual(
                CHECKER.parse_docs_endpoints(docs),
                [pair("GET", "/api/from-inventory")],
            )

    def test_parser_follows_inventory_endpoint_parts(self) -> None:
        with TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            docs = tmp_path / "docs.rs"
            docs.write_text("mod inventory;\n", encoding="utf-8")
            inventory_dir = tmp_path / "docs"
            inventory_dir.mkdir()
            (inventory_dir / "inventory.rs").write_text(
                textwrap.dedent(
                    """
                    mod endpoints;

                    pub(super) fn all_endpoints() {
                        endpoints::all()
                    }
                    """
                ).lstrip("\n"),
                encoding="utf-8",
            )
            endpoint_parts = inventory_dir / "inventory" / "endpoints"
            endpoint_parts.mkdir(parents=True)
            (endpoint_parts / "mod.rs").write_text(
                textwrap.dedent(
                    """
                    mod part_01;
                    mod part_02;
                    mod part_03;

                    fn all() {
                        endpoints.extend(part_01::endpoints());
                        endpoints.extend(part_02::endpoints());
                    }
                    """
                ).lstrip("\n"),
                encoding="utf-8",
            )
            (endpoint_parts / "part_01.rs").write_text(
                'fn endpoints() { vec![ep("GET", "/api/one", "docs", "description")]; }\n',
                encoding="utf-8",
            )
            (endpoint_parts / "part_02.rs").write_text(
                'fn endpoints() { vec![ep("POST", "/api/two", "docs", "description")]; }\n',
                encoding="utf-8",
            )
            (endpoint_parts / "part_03.rs").write_text(
                (
                    'fn endpoints() { vec![ep("DELETE", "/api/not-extended", '
                    '"docs", "description")]; }\n'
                ),
                encoding="utf-8",
            )

            self.assertEqual(
                CHECKER.parse_docs_endpoints(docs),
                [pair("GET", "/api/one"), pair("POST", "/api/two")],
            )

    def test_mounted_route_collection_includes_v1_router(self) -> None:
        mounted = set(CHECKER.collect_mounted_api_endpoints())

        self.assertIn(pair("GET", "/api/v1/overview"), mounted)

    def test_generated_route_inventory_includes_v1_router(self) -> None:
        route_inventory = CHECKER.inventory.generated_route_inventory()

        self.assertIn("| `GET` | `/api/v1/overview` |", route_inventory)

    def test_route_inventory_is_isolated_idempotent_and_renders_collector_output(self) -> None:
        inventory = CHECKER.inventory
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            routes_root = root / "src" / "server" / "routes"
            domains_root = routes_root / "domains"
            domains_root.mkdir(parents=True)
            (routes_root / "mod.rs").write_text(
                "fn compose_api_router(state: AppState) -> ApiRouter {\n"
                "    Router::new().merge(domains::synthetic::router(state))\n"
                "}\n",
                encoding="utf-8",
            )
            (domains_root / "synthetic.rs").write_text(
                "async fn route_fixture_r6_unique_handler() {}\n"
                "fn router(state: AppState) -> ApiRouter {\n"
                "    Router::new().route(\n"
                '        "/route-fixture-r6-unique",\n'
                "        get(route_fixture_r6_unique_handler),\n"
                "    )\n"
                "}\n",
                encoding="utf-8",
            )
            (root / "src" / "server" / "mod.rs").write_text(
                "fn server_fixture_marker() {}\n", encoding="utf-8"
            )

            def forbidden_giant_path(*_args: object, **_kwargs: object) -> None:
                raise AssertionError("route-only generation entered giant-file validation")

            with patch.object(inventory, "REPO_ROOT", root), patch.object(
                inventory, "collect_modules", forbidden_giant_path
            ), patch.object(
                inventory, "build_giant_registrations", forbidden_giant_path
            ):
                first = inventory.generated_route_inventory()
                second = inventory.generated_route_inventory()

        self.assertEqual(first, second)
        self.assertIn("| `GET` | `/api/route-fixture-r6-unique` |", first)
        self.assertIn("`route_fixture_r6_unique_handler`", first)

    def test_full_generation_delegates_route_inventory_after_valid_giant_check(self) -> None:
        inventory = CHECKER.inventory
        calls: list[str] = []

        def valid_giant_check(
            _modules: list[object], *, allow_overdue: bool = False
        ) -> list[object]:
            self.assertFalse(allow_overdue)
            calls.append("giant")
            return []

        def route_inventory_spy() -> str:
            calls.append("route")
            return "route-delegation-sentinel\n"

        with patch.object(
            inventory, "build_giant_registrations", valid_giant_check
        ), patch.object(inventory, "generated_route_inventory", route_inventory_spy):
            documents = inventory.generated_documents()

        self.assertEqual(calls, ["giant", "route"])
        self.assertEqual(
            documents[inventory.GENERATED_DOCS_DIR / "route-inventory.md"],
            "route-delegation-sentinel\n",
        )

    def test_overdue_giant_failure_precedes_route_generation(self) -> None:
        inventory = CHECKER.inventory
        calls: list[str] = []

        def overdue_giant_check(
            _modules: list[object], *, allow_overdue: bool = False
        ) -> list[object]:
            self.assertFalse(allow_overdue)
            calls.append("giant")
            raise inventory.ParseError("synthetic shrink deadline is overdue")

        def forbidden_route_generation() -> str:
            calls.append("route")
            raise AssertionError("route generation ran after an overdue giant failure")

        with patch.object(
            inventory, "build_giant_registrations", overdue_giant_check
        ), patch.object(
            inventory, "generated_route_inventory", forbidden_route_generation
        ):
            with self.assertRaisesRegex(inventory.ParseError, "deadline is overdue"):
                inventory.generated_documents()

        self.assertEqual(calls, ["giant"])

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
