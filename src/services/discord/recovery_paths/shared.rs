//! Cross-path recovery helpers (issue #1074).
//!
//! This module collects helpers that the three recovery paths (restart /
//! runtime / manual rebind) all need. It intentionally starts very small —
//! the goal of issue #1074's first landing is to create the SSoT surface and
//! migration target, not to relocate every helper at once.
//!
//! Helpers that live here must be:
//!   - pure or nearly pure (no lifecycle state mutation),
//!   - used by at least two of the three paths, or
//!   - explicitly documented as the canonical owner.
//!
//! See `docs/recovery-paths.md` for the path contract.
