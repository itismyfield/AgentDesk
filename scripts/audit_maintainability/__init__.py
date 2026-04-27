"""Maintainability audit checks for AgentDesk.

This package implements per-check modules invoked by
``scripts/audit_maintainability.py``. Each check lives in its own module under
``checks/`` so the audit harness itself stays small (per #1282 risk note).
"""

from __future__ import annotations

__all__ = ["checks"]
