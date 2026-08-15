"""Single lexical pin for whole-file skips in the two #5071 writer gates.

The ordinary basename skips and the higher-risk resolver skips are listed
separately for review, then exposed as one immutable set.  Every exclusion is
compared with this repo-relative lexical set before either gate scans files.

The reused resolver is deliberately unchanged.  It does not guarantee at
least these seven measured Rust forms: a comment between ``#[path]`` and
``mod``; a macro-generated production ``mod``; ``cfg(not(test))`` plus
``include!``; ``cfg(any(test, feature))`` plus ``include!``;
``cfg_attr(path = ...)``; a raw-string ``#[path]``; or an ungated
``include!``.

This pin guarantees membership, not compiler-backed reachability.  In
particular, a content change can make an already pinned file production-
reachable without changing the pinned path set.  Compiler-backed reachability
is follow-up slice work.  The lexical scan root is ``src/``; call sites in files
reached by ``#[path]``/``include!`` targets resolving outside ``src/`` are not
seen.  Fail-closed handling for that boundary is follow-up slice work.  Symlink
rejection is lexical rather than atomic: a post-enumeration filesystem
replacement is outside the guarantee; CI assumes a static checkout while a
gate runs.
"""

from __future__ import annotations

import importlib.util
import os
import sys
from collections.abc import Callable, Iterable
from pathlib import Path


# Mechanical ``tests.rs`` / ``*_tests.rs`` basename exclusions.
PINNED_BASENAME_TEST_FILES = frozenset(
    {
        "src/db/auto_queue/tests.rs",
        "src/db/automation_candidates/verdict_tests.rs",
        "src/db/dispatched_sessions/canonical_identity_pg_tests.rs",
        "src/db/dispatched_sessions/tests.rs",
        "src/db/intake_outbox_dispatch_stamp/tests.rs",
        "src/db/prompt_manifests/tests.rs",
        "src/db/scheduled_messages/postgres_tests.rs",
        "src/server/database_fixture_invariant_tests.rs",
        "src/server/routes/auto_queue_lifecycle_pg_tests.rs",
        "src/server/routes/scheduled_messages/postgres_tests.rs",
        "src/server/routes/tests/auto_queue_preflight_harness_tests.rs",
        "src/services/auto_queue/cleanup_tasks_pg_tests.rs",
        "src/services/auto_queue/runtime/clear_slot_sessions_pg_tests.rs",
        "src/services/automation_candidate_materializer/allowed_path_tests.rs",
        "src/services/automation_candidate_materializer/iteration_result_tests.rs",
        "src/services/claude_tui/hook_output_guard_tests.rs",
        "src/services/claude_tui/hook_server_memento_tests.rs",
        "src/services/cluster/intake_worker/dispatch_stamp_tests.rs",
        "src/services/discord/catch_up/classification_order_tests.rs",
        "src/services/discord/commands/inspect/tests.rs",
        "src/services/discord/formatting/replace_long_message_tests.rs",
        "src/services/discord/formatting/status_panel_v2_formatter_tests.rs",
        "src/services/discord/inflight/save_store/bridge_entry_guard_tests.rs",
        "src/services/discord/inflight/save_store/post_loop_identity_guard_tests.rs",
        "src/services/discord/outbound/turn_output_controller/fresh_send_tests.rs",
        "src/services/discord/placeholder_controller/queued_card_gate/tests.rs",
        "src/services/discord/placeholder_live_events/tests.rs",
        "src/services/discord/prompt_builder/dispatch_contract_tests.rs",
        "src/services/discord/recovery_engine/manual_rebind/post_adoption_guard_tests.rs",
        "src/services/discord/relay_coord_tests.rs",
        "src/services/discord/relay_recovery/tests.rs",
        "src/services/discord/router/intake_dispatch/tests.rs",
        "src/services/discord/router/message_handler/intake_turn/race_loss/mailbox_reaction_tests.rs",
        "src/services/discord/router/message_handler/intake_turn/race_loss/requeue_tests.rs",
        "src/services/discord/router/message_handler/intake_turn/dispatch_stamp/tests.rs",
        "src/services/discord/router/message_handler/session_strategy_lifecycle_tests.rs",
        "src/services/discord/runtime_bootstrap/gateway_lease_recovery_tests.rs",
        "src/services/discord/runtime_bootstrap/gateway_lease_tests.rs",
        "src/services/discord/runtime_bootstrap/intake_delivery_capability/tests.rs",
        "src/services/discord/runtime_bootstrap/intake_delivery_sweep/tests.rs",
        "src/services/discord/runtime_bootstrap/spawns_tests.rs",
        "src/services/discord/session_relay_sink/delivery_orchestration_tests.rs",
        "src/services/discord/status_panel_orphan_store_tests.rs",
        "src/services/discord/task_notification_delivery/tests.rs",
        "src/services/discord/tmux/task_notification_kind_restart_roundtrip_tests.rs",
        "src/services/discord/tmux_output_stream/provider_output_guard_tests.rs",
        "src/services/discord/tmux_watcher/completion_gate_tests.rs",
        "src/services/discord/tmux_watcher/panel_decisions_tests.rs",
        "src/services/discord/tmux_watcher/session_bound_ack_tests.rs",
        "src/services/discord/tmux_watcher/single_message_footer_tests.rs",
        "src/services/discord/tmux_watcher/supervisor_relay_tests.rs",
        "src/services/discord/tmux_watcher/terminal_direct_fallback_tests.rs",
        "src/services/discord/tmux_watcher/terminal_readiness_tests.rs",
        "src/services/discord/tmux_watcher/terminal_relay_plan_tests.rs",
        "src/services/discord/tmux_watcher/tests.rs",
        "src/services/discord/tmux_watcher/turn_identity_tests.rs",
        "src/services/discord/tmux_watcher/two_message_panel_tests.rs",
        "src/services/discord/tmux_watcher/utf8_chunk_decoder_tests.rs",
        "src/services/discord/tmux_watcher_registry_restore_tests.rs",
        "src/services/discord/tui_prompt_relay/tests.rs",
        "src/services/discord/turn_bridge/chunk_compose_tests.rs",
        "src/services/discord/turn_bridge/intake_settlement/tests.rs",
        "src/services/discord/turn_bridge/runtime_handoff_loop/tests.rs",
        "src/services/discord/turn_bridge/status_panel_tests.rs",
        "src/services/discord/turn_bridge/stream_loop/expected_identity_tests.rs",
        "src/services/discord/turn_bridge/stream_loop/tool_arms/authority_tests.rs",
        "src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests.rs",
        "src/services/discord/turn_bridge/voice_completion_tests.rs",
        "src/services/discord/turn_finalizer/finalize/tests/residue_tests.rs",
        "src/services/discord/turn_view_reconciler/tests.rs",
        "src/services/discord/voice_barge_in/tests/pcm_harness_tests.rs",
        "src/services/discord/watchers/lifecycle/restore_tests.rs",
        "src/services/discord/watchers/lifecycle/tests.rs",
        "src/services/message_outbox_circuit_authority_tests.rs",
        "src/services/message_outbox_recovery_tests.rs",
        "src/services/provider/provider_conformance_invariant_tests.rs",
        "src/services/provider_output_guard_tests.rs",
        "src/services/scheduled_messages/postgres_tests.rs",
        "src/services/tui_prompt_dedupe/tests.rs",
    }
)

# Production-looking basenames classified as test-only by the shared resolver.
PINNED_RESOLVER_TEST_ONLY_FILES = frozenset(
    {
        "src/db/auto_queue/test_support.rs",
        "src/db/fixture_target.rs",
        "src/dispatch/test_support.rs",
        "src/high_risk_recovery.rs",
        "src/server/routes/tests/preflight_harness/types.rs",
        "src/server/routes/tests/preflight_harness/validation.rs",
        "src/services/discord/inflight/invariant_test_capture.rs",
        "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4361.rs",
        "src/services/discord/inflight/stall_recovery_tests/flake_isolation_4422.rs",
        "src/services/discord/relay_recovery/tests/circuit_breaker_apply.rs",
        "src/services/discord/tui_prompt_relay/local_model_queue_wake_e2e.rs",
    }
)

if PINNED_BASENAME_TEST_FILES & PINNED_RESOLVER_TEST_ONLY_FILES:
    raise RuntimeError("writer-gate basename and resolver pins must be disjoint")

PINNED_TEST_ONLY_MODULE_FILES = (
    PINNED_BASENAME_TEST_FILES | PINNED_RESOLVER_TEST_ONLY_FILES
)

_UPDATE_HINT = (
    "Review basename and resolver classification, then update only "
    "scripts/test_only_module_skip_pin.py; both gates and tests derive their "
    "path/count expectation from that single file."
)

_SYMLINK_HINT = (
    "Remove the symlink or replace it with a regular in-tree file; do not add "
    "it to the writer-gate skip pin."
)


def _load_inventory_generator():
    name = "generate_inventory_docs"
    if name in sys.modules:
        return sys.modules[name]
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).resolve().parent / "generate_inventory_docs.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("cannot load scripts/generate_inventory_docs.py")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


_INVENTORY = _load_inventory_generator()


def skip_pin_drift(
    excluded_paths: Iterable[str],
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> str | None:
    """Compare lexical repo-relative paths in both directions."""

    actual = frozenset(Path(path).as_posix() for path in excluded_paths)
    expected = frozenset(Path(path).as_posix() for path in pinned_paths)
    if actual == expected:
        return None
    lines = ["FAIL: writer-gate whole-file skip set drift."]
    scan_only = sorted(actual - expected)
    pin_only = sorted(expected - actual)
    if scan_only:
        lines.append("  scan-only (newly skipped): " + ", ".join(scan_only))
    if pin_only:
        lines.append("  pin-only (no longer skipped): " + ", ".join(pin_only))
    lines.append("  " + _UPDATE_HINT)
    return "\n".join(lines)


def _lexical_rust_files(root: Path, scan_root: Path) -> list[Path]:
    """Enumerate every regular file and reject symlinks/non-``.rs`` files."""

    source = root / scan_root
    symlinks: list[str] = []
    non_rust: list[str] = []
    if source.is_symlink():
        symlinks.append(scan_root.as_posix())
    files: list[Path] = []
    if source.is_dir() and not symlinks:
        for directory, dirnames, filenames in os.walk(source, followlinks=False):
            directory_path = Path(directory)
            for name in sorted((*dirnames, *filenames)):
                path = directory_path / name
                if path.is_symlink():
                    symlinks.append(path.relative_to(root).as_posix())
            for name in sorted(filenames):
                path = directory_path / name
                if path.is_symlink() or not path.is_file():
                    # The ``not path.is_file()`` half is this enumerator's sole
                    # non-fail-closed branch: git checkouts cannot carry a FIFO,
                    # while the old read-text path would block on one. Symlinks
                    # were recorded above and still fail closed; only this
                    # non-regular case is silently omitted here.
                    continue
                files.append(path)
                if not name.endswith(".rs"):
                    non_rust.append(path.relative_to(root).as_posix())
    if symlinks:
        raise RuntimeError(
            "FAIL: writer gates reject file or directory symlinks under src/: "
            + ", ".join(sorted(symlinks))
            + ". "
            + _SYMLINK_HINT
        )
    if non_rust:
        raise RuntimeError(
            "FAIL: writer gates reject non-.rs regular files under src/: "
            + ", ".join(sorted(non_rust))
            + ". The writer gate cannot classify non-.rs files under src/; "
            "remove them or extend the gate policy."
        )
    return sorted(files)


def validated_scan_files(
    root: Path,
    scan_root: Path,
    is_test_file: Callable[[str], bool],
    *,
    pinned_paths: Iterable[str] = PINNED_TEST_ONLY_MODULE_FILES,
) -> tuple[list[Path], frozenset[Path]]:
    """Enumerate, classify, pin-check, and return all files plus whole skips."""

    pinned = frozenset(Path(path).as_posix() for path in pinned_paths)
    all_files = _lexical_rust_files(root, scan_root)
    basename_skips = {path for path in all_files if is_test_file(path.name)}
    production_files = [path for path in all_files if path not in basename_skips]
    if not production_files:
        if all_files:
            detail = (
                "all enumerated regular .rs files are basename-classified "
                "test-only files"
            )
        else:
            detail = "the lexical src/ enumeration found no regular .rs files"
        raise RuntimeError(
            "FAIL: writer-gate production file list is empty; the shared "
            "test-only resolver was not invoked because its empty-input "
            "fallback would scan the repository instead of this lexical src/ "
            "enumeration. "
            + detail
            + ". Restore a production .rs file under src/ or "
            "extend the gate policy."
        )
    resolved_to_lexical = {path.resolve(): path for path in all_files}
    resolver_results = _INVENTORY.test_only_module_files(
        production_files=production_files,
        all_files=all_files,
        read_text_fn=lambda path: path.read_text(encoding="utf-8"),
    )
    resolver_skips: set[Path] = set()
    unmapped: list[str] = []
    for result in resolver_results:
        lexical = resolved_to_lexical.get(result.resolve())
        if lexical is None:
            unmapped.append(result.as_posix())
        else:
            resolver_skips.add(lexical)
    if unmapped:
        raise RuntimeError(
            "FAIL: test-only resolver returned paths outside lexical src/ enumeration: "
            + ", ".join(sorted(unmapped))
            + ". "
            + _UPDATE_HINT
        )

    whole_skips = frozenset(basename_skips | resolver_skips)
    lexical_skips = {
        path.relative_to(root).as_posix() for path in whole_skips
    }
    drift = skip_pin_drift(lexical_skips, pinned)
    pinned_count = len(pinned)
    if len(whole_skips) != pinned_count:
        census = (
            "FAIL: writer-gate skipped census differs from pin count "
            f"({len(whole_skips)} skipped, {pinned_count} pinned).\n  {_UPDATE_HINT}"
        )
        raise RuntimeError(f"{drift}\n{census}" if drift else census)
    if drift:
        raise RuntimeError(drift)
    return all_files, whole_skips
