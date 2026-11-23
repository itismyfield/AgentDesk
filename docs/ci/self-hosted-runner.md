# Self-hosted Rust CI runner

The **Rust tests build and plan** job and the lint jobs (**Lint runner** in PRs,
**Main lint and non-lib tests** on main) use self-hosted runners for pushes to
main, manual dispatches, and pull requests from branches in this repository.
The workflow routes fork pull requests to `ubuntu-latest`. The test workers
stay on GitHub-hosted runners and consume the nextest archive.

Two WSL runners split the work: label `adk-wsl-rust` takes the test build and
label `adk-wsl-lint` takes lint, so a lint run never queues the test build.

## Fork workflow approval

**Only the repository owner may approve a fork workflow run.** Before approval,
the owner checks whether the PR changes any workflow file and reviews the
execution paths the run would use. Agents and automation, including
coordinators, implementation lanes, and PR babysitters, must never approve a
fork workflow run.

The repository's `all_external_contributors` setting puts external fork runs
that require approval on hold. GitHub grants approval capability to users with
repository write access and exempts certain trusted users. The owner-only rule
above governs how this repository uses that capability.

The checked-in `pull_request` runner condition compares the head repository with
the current repository. A host-side start hook also checks the event and
repository and permits push and manual dispatch only on the main branch. A
rejected event fails the hook and causes ordinary success-gated steps to skip.

An approved fork can supply changes to workflow code and runner selection.
GitHub's runner still evaluates conditions such as `if: always()` after a hook
failure. The approval review is therefore part of the trust boundary. The
start hook adds defense in depth. Deployment depends on a dedicated unprivileged
runner account with its own home, toolchains, and build target, without access
to operator credentials, runtime files, or privilege escalation.

See [GitHub's fork approval documentation](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/approve-runs-from-forks)
and [guidance for self-hosted runners](https://docs.github.com/en/actions/reference/security/secure-use#hardening-for-self-hosted-runners).

## Preinstalled tools and the CI cargo slot

Self-hosted jobs skip `taiki-e/install-action` when `scripts/ci/preinstalled-tool.sh`
finds the pinned version on the runner PATH. A missing tool or another version
falls back to install-action, as on hosted runners.

| Tool | Version | Host path | Installed by |
| --- | --- | --- | --- |
| cargo-nextest | 0.9.149 | `/home/adk-ci/.cargo/bin/cargo-nextest` | runner `install.sh prepare` (digest pinned) |
| just | 1.51.0 | `/usr/local/bin/just` (root, 0755) | release tarball, checked against its `SHA256SUMS` |

Check on the host: `sudo -u adk-ci /home/adk-ci/.cargo/bin/cargo-nextest nextest --version`
and `/usr/local/bin/just --version`. To bump a version, change the install step
and its `Check preinstalled` step together (`scripts/ci/preinstalled_tools_wiring.rb`
rejects a mismatch). Then install the new prebuilt binary on the host. Until the
host has it, jobs download it through install-action.

The host cargo shim keeps the last N machine slots for CI jobs when
`/etc/adk/cargo_ci_reserve` holds N; the WSL host sets 1. CI cargo (runner
environment with `ADK_CARGO_PRIORITY`) may take any slot, and lane builds use the rest.
