//! Phase 0 stub. Phase 1 owns the SIGINT/SIGKILL escalation cascade so a
//! Discord stop request kills `claude-e`, the child `claude` PTY, and any
//! MCP server children without orphans.
