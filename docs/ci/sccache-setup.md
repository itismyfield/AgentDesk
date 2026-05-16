# sccache Setup

`sccache` caches `rustc` outputs across builds. Combined with per-worktree
`target/` directories (which are intentionally not shared to avoid Cargo
lockouts under parallel campaign runs), it lets concurrent worktrees share
compiled dependency crates and recover full cache hits after a clean.

Tracking issue: [#1090](https://github.com/itismyfield-org/agentdesk/issues/1090)
("sccache 도입 + 빌드 경로 반영").

---

## 1. Install

| Platform | Command |
|----------|---------|
| macOS (Apple Silicon / Intel) | `brew install sccache` |
| Linux (Ubuntu / Debian) | `cargo install sccache --locked` (or `apt install sccache` on newer releases) |
| Windows | `cargo install sccache --locked` |
| GitHub Actions | Already wired via `mozilla-actions/sccache-action@v0.0.9` (see `.github/workflows/ci-*.yml`) |

Verify after install:

```bash
sccache --version
sccache --show-stats   # should report a fresh cache (zero hits / zero misses)
```

On macOS the Homebrew binary lives at `/opt/homebrew/bin/sccache`. The helper
`setup_sccache_env` in `scripts/_defaults.sh` prepends that directory to `PATH`
when the binary is present but the directory is not already on `PATH`.

---

## 2. Configuration Surface

`sccache` is activated in three layers. Each layer degrades gracefully when the
binary is absent — no hard-fail.

### 2.1 `.cargo/config.toml` (checked in)

```toml
[build]
rustc-wrapper = "./scripts/rustc-wrapper-sccache.sh"
incremental = false
```

Every `cargo` invocation inside this repo — whether from a human shell, a
campaign worktree, or an agent session — picks up the wrapper automatically.

The wrapper (`scripts/rustc-wrapper-sccache.sh`) prefers an explicit `SCCACHE_BIN`
override, then falls back to `command -v sccache`, then `/opt/homebrew/bin/sccache`,
then a plain `rustc` exec. This is the key fix for the historical
"`No such file or directory` for `sccache`" failure that forced agents and
developers to prefix every cargo call with `RUSTC_WRAPPER=`. Bare
`cargo build`/`cargo check`/`cargo test` now Just Works regardless of whether
sccache is installed.

Escape hatch: `RUSTC_WRAPPER_DISABLE=1 cargo build` skips the wrapper entirely.

> **Gotcha**: `SCCACHE_CACHE_SIZE` cannot be set via `config.toml [env]` in a
> way that reaches `sccache` itself — the wrapper reads its own env from the
> process environment, not from Cargo's injected vars. Set it via shell scripts
> (see §2.2) or the calling launcher.

### 2.2 Shell env (release build path)

`scripts/_defaults.sh :: setup_sccache_env` exports:

| Variable | Default | Purpose |
|----------|---------|---------|
| `SCCACHE_DIR` | `$HOME/.cache/sccache` | Cache location |
| `SCCACHE_CACHE_SIZE` | `10G` | Eviction ceiling |
| `RUSTC_WRAPPER` | resolved `sccache` binary | Signals Cargo to wrap rustc |

Callers:

- `scripts/build-release.sh` — exports before `cargo build --release`, soft-fail if sccache missing.
- `scripts/deploy-release.sh` — same, prior to building the agentdesk binary for release promotion.

If sccache is not installed, both scripts **print a warning and continue** with
`RUSTC_WRAPPER=""` + `CARGO_BUILD_RUSTC_WRAPPER=""` explicitly cleared (so the
`.cargo/config.toml` value does not leak through and cause a hard-fail).

### 2.3 CI (`.github/workflows/ci-*.yml`)

`RUSTC_WRAPPER: sccache` is set at the workflow `env:` level in `ci-main.yml`,
`ci-pr.yml`, and `ci-nightly.yml`. Each Rust build job adds a
`Setup sccache` step:

```yaml
- name: Setup sccache
  uses: mozilla-actions/sccache-action@v0.0.9
```

Cache storage is backed by GitHub Actions cache (automatic when using the
action) — no manual GCS/S3 wiring needed.

---

## 3. Env Var Matrix

| Scope | `RUSTC_WRAPPER` | Incremental | `SCCACHE_DIR` | `SCCACHE_CACHE_SIZE` | Source |
|-------|-----------------|-------------|---------------|----------------------|--------|
| Local dev (bare `cargo build`) | `sccache` | disabled | `$HOME/.cache/sccache` (sccache default) | sccache default (10G advised) | `.cargo/config.toml` |
| Campaign worktree build | `sccache` | disabled | `$HOME/.cache/sccache` | sccache default | `.cargo/config.toml` |
| `scripts/build-release.sh` | resolved `sccache` path | disabled | `$HOME/.cache/sccache` | `10G` | `.cargo/config.toml` + `setup_sccache_env` |
| `scripts/deploy-release.sh` | resolved `sccache` path | disabled | `$HOME/.cache/sccache` | `10G` | `.cargo/config.toml` + `setup_sccache_env` |
| CI (`ci-*.yml`) | `sccache` | disabled | provided by `sccache-action` | provided by `sccache-action` | `.cargo/config.toml` + workflow `env:` + action |

To override per-session: `SCCACHE_DIR=/path SCCACHE_CACHE_SIZE=20G cargo build`.

---

## 4. Measuring Cache Hit Rate

Run after any build sequence:

```bash
sccache --show-stats
```

Key rows:

- `Compile requests` — total rustc invocations observed.
- `Cache hits` / `Cache misses` — should trend to >60% hits once 2–3 worktrees
  have built the same deps.
- `Non-cacheable calls` — build scripts, linker invocations, etc. These do not
  count against hit rate.

Reset stats between measurements:

```bash
sccache --zero-stats
# ... run builds ...
sccache --show-stats
```

### 4.1 Deployment verification (deferred post-install)

Per #1090 DoD: measure **≥60% deps cache hit rate across parallel worktree
builds**.

Procedure once sccache is installed on the build host:

```bash
sccache --zero-stats
# Kick off 3–4 parallel campaign worktree builds, then:
sccache --show-stats | tee sccache-stats-$(date +%Y%m%d-%H%M).txt
```

Success criterion: `Cache hits / (Cache hits + Cache misses) >= 0.6` measured
only over cacheable compile requests.

This measurement is explicitly deferred to post-deployment ops; config-side
work (this PR) lands the plumbing only.

---

## 4.2 CI cache budget (GHA backend)

GitHub Actions cache has a **10GB per-repo quota** with LRU eviction. The
sccache GHA backend (`SCCACHE_GHA_ENABLED=true`) writes one cache entry per
rustc invocation, sharded under `sccache/` keys. Anything else cached via
`actions/cache@v4` competes for the same 10GB quota.

We previously cached the entire `target/` directory alongside sccache; each
`target/` cache key is 0.8–1.2GB and several lanes (full_non_pg, postgres,
recovery, lint, fast-tests) created independent keys. Total cache footprint
ballooned to ~20GB, well above the 10GB cap, and GHA's LRU started evicting
sccache shards constantly. Observed effect: **Rust cache hit rate collapsed
from 99.80% to 0.00% within a few hours of merges**, even though every
workflow continued to run `mozilla-actions/sccache-action`.

The fix in this repo: keep `actions/cache@v4` for `~/.cargo/registry` +
`~/.cargo/git` only (small, low-churn). Let sccache own all compiled rustc
output. This keeps the GHA quota dominated by small sccache shards, which is
the documented best practice for `mozilla-actions/sccache-action`.

Verification: every Rust job ends with an explicit `sccache --show-stats` step
so the hit-rate trend is visible directly in CI logs without needing to grep
through the action's post-step output.

---

## 5. Troubleshooting

- **`error: process didn't exit successfully` for the rustc wrapper** — the
  wrapper script (`scripts/rustc-wrapper-sccache.sh`) should never hard-fail
  on a missing sccache binary; it falls back to a plain rustc exec. If you
  hit this anyway, confirm the script is executable
  (`chmod +x scripts/rustc-wrapper-sccache.sh`) and run with
  `RUSTC_WRAPPER_DISABLE=1 cargo …` to confirm the wrapper is the culprit.
- **No hit-rate improvement across worktrees** — confirm each worktree sees
  the same `SCCACHE_DIR`. By default it is `$HOME/.cache/sccache`, which is
  shared across worktrees.
- **`sccache` spawns but no cache activity** — check `sccache --show-stats`
  for `Non-cacheable calls`; proc-macro crates and some build scripts are not
  cacheable. Cargo also requires `CARGO_INCREMENTAL=0` (which `.cargo/config.toml`
  enables via `incremental = false`) — sccache cannot cache incremental rustc
  invocations.
- **CI Rust hit rate stuck at 0%** — see §4.2. Most commonly the GHA cache
  budget is being consumed by something other than sccache (e.g. `target/`
  added back to `actions/cache@v4`).
