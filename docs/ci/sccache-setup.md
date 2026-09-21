# sccache Setup

`sccache` caches `rustc` outputs across builds. Combined with per-worktree
`target/` directories (which are intentionally not shared to avoid Cargo
lockouts under parallel campaign runs), it lets concurrent worktrees share
compiled dependency crates and recover full cache hits after a clean.

Tracking issue: [#1090](https://github.com/itismyfield/AgentDesk/issues/1090)
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
`apply_sccache_env` in `scripts/build_token.py` (§2.4) applies the same prepend
under the same condition, to the child environment it builds.

---

## 2. Configuration Surface

`sccache` is activated in four layers. Each layer degrades gracefully when the
binary is absent — no hard-fail.

### 2.1 `.cargo/config.toml` (checked in)

```toml
[build]
rustc-wrapper = ""
incremental = false
```

`rustc-wrapper` is intentionally set to the empty string here. The previous
value of `"sccache"` broke every bare `cargo` invocation on machines without
sccache installed (Cargo errors with `No such file or directory (os error 2)`),
forcing agents and developers to prefix every command with `RUSTC_WRAPPER=`.
The empty string disables the wrapper by default and overrides any stale
parent `.cargo/config.toml` that still opts into sccache.

Cargo treats `rustc-wrapper = ""` as "no wrapper" on Cargo 1.85 and newer.
The repo CI toolchain is pinned to 1.94.1; local developers should use Cargo
1.85+ or delete the key locally if they must build with an older toolchain. A
wrapper script (`.sh`) is also not portable to native Windows Cargo
invocations, so the supported pattern is to let callers opt in via the
environment:

- CI sets `RUSTC_WRAPPER: sccache` at the workflow `env:` level (Linux +
  Windows lanes). The Mozilla sccache action installs the binary.
- Release/deploy scripts call `setup_sccache_env` from `scripts/_defaults.sh`,
  which conditionally exports `RUSTC_WRAPPER` only when sccache is found.
- Local developers add `export RUSTC_WRAPPER=sccache` to their shell rc after
  `brew install sccache` (or `cargo install sccache --locked` / `winget
  install Mozilla.sccache`).

`incremental = false` stays because sccache cannot cache incremental rustc
invocations and campaign worktrees are build-once-and-discard, so the
multi-GB `target/debug/incremental` per-worktree hit is pure waste.

> **Gotcha**: `SCCACHE_CACHE_SIZE` cannot be set via `config.toml [env]` in a
> way that reaches `sccache` itself — sccache reads its own env from the
> process environment, not from Cargo's injected vars. Set it via shell scripts
> (see §2.2) or the calling launcher.

### 2.2 Shell env (release, deploy and installer source builds)

`scripts/_defaults.sh :: setup_sccache_env` exports:

| Variable | Default | Purpose |
|----------|---------|---------|
| `SCCACHE_DIR` | `$HOME/.cache/sccache` | Cache location |
| `SCCACHE_CACHE_SIZE` | `40G` | Adjustable local disk-cache ceiling |
| `SCCACHE_IDLE_TIMEOUT` | `0` | Disable idle daemon exit; retain counters between builds |
| `RUSTC_WRAPPER` | resolved `sccache` binary | Signals Cargo to wrap rustc |

Callers:

- `scripts/build-release.sh` — exports before `cargo build --release`, soft-fail if sccache missing.
- `scripts/deploy-release.sh` — same, prior to building the agentdesk binary for release promotion.
- `scripts/install.sh` — sources `_defaults.sh` and calls the helper best-effort before its source build.

`scripts/build_token.py` also activates sccache, but it is **not** a caller of this
helper — it carries an independent copy of the same defaults, with a deliberately
different precedence rule. See §2.4.

For cache size and idle timeout, unset or empty values use the defaults; nonempty
caller values are passed through, including `10G`, `20G`, `900`, or idle `0`.
These helpers do not validate nonempty values. The 40G ceiling allows up to 30G
more local disk use than the previous 10G ceiling without preallocating it.
It aims to reduce cache eviction; a build-speed or hit-rate improvement has not
been measured for this change.

Size and idle settings take effect when the daemon starts. Exporting them does
not resize or reconfigure an already running daemon. A deliberate restart in a
quiet build window is needed to apply changed settings to that daemon; merging
this change alone does not restart it. Bare Cargo with a manually set wrapper
uses inherited settings or sccache's own defaults (10G and 600s), not this helper.

If sccache is not installed, both release scripts **print a warning and continue** with
`RUSTC_WRAPPER=""` + `CARGO_BUILD_RUSTC_WRAPPER=""` explicitly cleared (so the
`.cargo/config.toml` value does not leak through and cause a hard-fail).

### 2.3 CI (`.github/workflows/ci-*.yml`)

`RUSTC_WRAPPER: sccache` is set at the workflow `env:` level in `ci-main.yml`,
`ci-pr.yml`, and `ci-nightly.yml` for Linux/Windows jobs. Each Rust build job adds a
`Setup sccache` step:

```yaml
- name: Setup sccache
  uses: mozilla-actions/sccache-action@v0.0.9
```

Cache storage is backed by GitHub Actions cache (automatic when using the
action) — no manual GCS/S3 wiring needed.

macOS hosted jobs explicitly clear both `RUSTC_WRAPPER` and
`SCCACHE_GHA_ENABLED` before build steps so a Homebrew `sccache` binary does
not try to use the GitHub Actions backend without the action token. Trusted
self-hosted macOS jobs in `ci-macos-trusted.yml` do not use the GHA backend;
they clear `SCCACHE_GHA_ENABLED` and opt into the runner-local `sccache`
binary when installed.

### 2.4 Build token wrapper (campaign build path)

Campaign lanes can activate caching through `python3 scripts/build_token.py -- <cmd>`.
On successful activation, `apply_sccache_env` writes five keys on
the child environment only, on POSIX only (after the `win32` early return):
`RUSTC_WRAPPER`, `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `SCCACHE_IDLE_TIMEOUT`, and
`PATH`. The defaults match the shell helper: resolved absolute `sccache`,
`$HOME/.cache/sccache`, `40G`, and `0`; `PATH` carries the `/opt/homebrew/bin`
prepend of §1 under the same condition.

Its precedence rule is deliberately **not** `setup_sccache_env`'s. That helper is
imperative — a script calls it to turn sccache on, so overwriting `RUSTC_WRAPPER` is
the point of the call. `apply_sccache_env` is ambient: every campaign child gets it
unasked, so an existing caller decision stands. If either `RUSTC_WRAPPER` or
`CARGO_BUILD_RUSTC_WRAPPER` is present — **empty string included**, that being Cargo's
own spelling of "no wrapper" and the pair §2.2 has the release scripts clear — it
changes nothing: it does not fill missing size or idle settings for an existing
wrapper, even `RUSTC_WRAPPER=sccache`. That also makes it a no-op on CI lanes whose workflows set
`RUSTC_WRAPPER` at the `env:` level (§2.3).

| Variable | Effect |
|----------|--------|
| `ADK_BUILD_TOKEN_SCCACHE` | `0`, `false`, `no` or `off` (trimmed, case-insensitive) skips activation entirely. Unset — or any other value — leaves it enabled. |

No sccache on `PATH`, or a cache directory that cannot be created, leaves the child
environment byte-identical: the cache is dropped, never the build.

---

## 3. Env Var Matrix

| Scope | `RUSTC_WRAPPER` | Incremental | `SCCACHE_DIR` | `SCCACHE_CACHE_SIZE` | `SCCACHE_IDLE_TIMEOUT` | Source |
|-------|-----------------|-------------|---------------|----------------------|------------------------|--------|
| Local dev (bare `cargo build`) | none by default | disabled | n/a | n/a | n/a | `.cargo/config.toml` |
| Local dev (manual opt-in) | `sccache` | disabled | inherited or sccache platform default | inherited or upstream `10G` | inherited or upstream `600` | shell env |
| Campaign worktree build (activation eligible) | resolved `sccache` path | disabled | `$HOME/.cache/sccache` | `40G` | `0` | `build_token.py :: apply_sccache_env` (§2.4) |
| `scripts/build-release.sh` | resolved `sccache` path | disabled | `$HOME/.cache/sccache` | `40G` | `0` | `.cargo/config.toml` + `setup_sccache_env` |
| `scripts/deploy-release.sh` | resolved `sccache` path | disabled | `$HOME/.cache/sccache` | `40G` | `0` | `.cargo/config.toml` + `setup_sccache_env` |
| Installer source build (helper available) | resolved `sccache` path | disabled | `$HOME/.cache/sccache` | `40G` | `0` | `scripts/install.sh` + `setup_sccache_env` |
| CI Linux/Windows (`ci-*.yml`) | `sccache` | disabled | provided by `sccache-action` | workflow `10G` | inherited/upstream | workflow env + action |
| CI macOS hosted | none | disabled | n/a | n/a | n/a | workflow clears `RUSTC_WRAPPER` + `SCCACHE_GHA_ENABLED` |
| CI macOS self-hosted trusted | resolved `sccache` path when installed | disabled | `$HOME/.cache/sccache` | `20G` | inherited/upstream | `ci-macos-trusted.yml` + runner launchd env |

Helper rows show defaults when sccache is available and size/idle values are unset
or empty. Nonempty caller values override them. The campaign row also requires
both wrapper keys to be absent and no activation opt-out (§2.4).

For a manual opt-in build, set the wrapper and desired overrides explicitly:
`RUSTC_WRAPPER=sccache SCCACHE_DIR=/path SCCACHE_CACHE_SIZE=20G SCCACHE_IDLE_TIMEOUT=900 cargo build`.
The existing-daemon boundary in §2.2 still applies.

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

### 4.3 PR vs main GHA cache scoping

GitHub Actions cache scopes entries by ref. PR/feature-branch builds write to
their own cache scope and can read from the default-branch (`main`) cache.
PR writes therefore do not directly evict `main`'s cache shards under normal
ref isolation — only the *global* 10GB quota matters. With `target/` removed
from `actions/cache@v4` (§4.2), total cache usage drops by ~5–15GB, well below
the 10GB cap, and main-branch sccache shards should remain stable.

If future ops still observe `main` cache eviction under heavy PR traffic,
the next escalation is to set `SCCACHE_GHA_VERSION` per event type (separate
namespaces for `push to main` vs `pull_request`), or to disable
`SCCACHE_GHA_ENABLED` on PR jobs entirely and rely on per-job runner-local
state — at the cost of giving up cache reuse for PR builds. Do not apply that
escalation pre-emptively; verify with the per-job `sccache --show-stats`
output first.

---

## 5. Troubleshooting

- **`error: process didn't exit successfully: rustc`** with a wrapper-related
  message — earlier versions of this repo set `rustc-wrapper = "sccache"` in
  `.cargo/config.toml`, which hard-fails when sccache is not installed. The
  current `.cargo/config.toml` uses `rustc-wrapper = ""`; if Cargo reports
  `program not found ""`, upgrade Cargo to 1.85+ or remove that local key
  while using the older toolchain. If the wrapper is still `sccache`, check
  whether `~/.cargo/config.toml` or an env override reintroduced it.
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
