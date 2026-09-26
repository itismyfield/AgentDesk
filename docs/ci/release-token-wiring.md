# Release Cargo wiring

`python3 scripts/check_release_token_wiring.py --evidence /tmp/release-token-evidence`
runs both release scripts with stub Cargo for `release` and `release-fast`.
Build uses an explicit supported target (also avoiding Bash 3.2's nounset
error on the current script's empty native-target array).
Every Cargo invocation records argv, the holder marker and a check of the
temporary token's inode, parent holder PID and live exclusive-lock contention.
Any release invocation outside the holder fails the aggregate check, even when
the script ignores the command's status. Read-only `cargo metadata` may be unwrapped.

The build script runs through packaging. The deploy copy ends immediately after
the unique top-level `_clean_release_build_cache_after_staging` call returns;
this covers its build and post-staging clean without running promotion or restart.
Completion and presence of the required Cargo phases are independently checked.
New Cargo phases after this boundary require extending the fixture's coverage.

The existing scanner in `tests/test_build_token_serialization_5663.py`
(`cargo_sites`, `release_cargo_sites`, `WiringTests`) remains a supplementary
repository inventory, including Makefile/install.sh's declared exceptions and
diagnostic-FD wiring. Runtime observation owns executed release-script wiring;
the scanner cannot interpret arrays assembled at runtime, functions or eval.
Both run in the contracts shard of `scripts/ci-script-checks.sh`.

## Side-effect inventory and isolation

Before implementing the fixture, the release scripts' external commands were audited:

| Script | External commands and entry points |
| --- | --- |
| build-release.sh | dirname, Python, rustc, sed, cargo, bash; checksum, dashboard and packaging helpers |
| deploy-release.sh | uname, dirname, basename, mkdir, nohup, security, cat, codesign, grep, head, tail, tr, sed, awk, cargo, jq, find, sort, shasum, sha256sum, date, git, gh, bash, node, npm, Python, mv, cp, rm, chmod, touch, rsync, stat, strings, mktemp, xattr, curl, ssh, tmux, env, lockf, flock, sleep, id, launchctl, lsof, chflags, ruby, psql, cksum, install, ln, wc, hostname, cmp; staged binary and helper scripts |

The sourced `_defaults.sh` also probes host resources and can prepend Homebrew
to PATH. The fixture replaces it with deployment-environment doubles. It copies
no operator config, repository symlinks or live binary. HOME, TMPDIR, release root
and PATH are temporary and readonly; PATH contains only doubles, with no host
fallback. The environment is constructed from scratch, excluding ambient shell,
Python, token-holder and release overrides. Filesystem doubles resolve paths
inside the fixture before creating/copying; rm and rsync never delete anything.
Network and service commands cannot reach their real executables. Unknown stub
calls and missing commands leave a failure record, including ignored errors.
The shell's builtin kill is shadowed by a refusal function.

Only Bash, the Python interpreter and the copied build-token helper execute real
code. The helper's canonical token constant is relocated before import; its
host process probe is stubbed and compiler-cache environment is disabled.
This is a fixture for reviewed repository shell inputs, not a sandbox for
arbitrary hostile shell code: absolute executables and shell redirections must
be audited when expanding coverage. The observed prefix uses only stub commands
and redirects to fixture paths or `/dev/null`. No real build, deployment,
launchd operation, network access or operating release-path write is required.
