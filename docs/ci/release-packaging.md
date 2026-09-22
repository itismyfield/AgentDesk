# Common native release artifacts

The same binary and dashboard package serves a normal installation and its
configured runtime role. Runtime configuration, credentials and workspaces stay
outside the archive. Packaging uses tracked, explicitly selected runtime assets.

## Build and inspect

Python 3.11+, the repository Rust toolchain, and the Node version in `.nvmrc`
are required. Build on the target operating system:

```sh
bash scripts/build-release.sh --profile release-fast
python -m unittest tests.test_package_release
```

`--target <rust-target>` selects the Cargo target explicitly. The shared script
uses the build token, verifies migration checksums, builds the binary and verifies
the dashboard before packaging. `--prebuilt-dashboard` uses an already verified
`dashboard/dist`; `--skip-dashboard` deliberately makes a package without UI.
Published common releases require the dashboard.

Each archive has a SHA-256 sidecar and a `release-manifest.json` recording the
source commit, checkout cleanliness, version, build profile, target architecture
and every included file digest. `runtime/release-source.json` preserves the
existing runtime provenance format. `checksums.txt` preserves installer lookup.

```sh
python scripts/verify_release_artifacts.py dist --commit "$COMMIT" \
  --version "$VERSION" --profile release-fast \
  --target aarch64-apple-darwin --target x86_64-pc-windows-msvc \
  --target x86_64-unknown-linux-gnu
```

Verification rejects missing or duplicate targets, dirty source identity, wrong
commit/version/profile, unsafe archive paths, links, missing files and changed
digests. The packaging tests create real tar/zip fixtures; their binary headers
are fixtures and do not establish native runtime execution.

## CI and publication

PRs validate packaging and build macOS ARM64, Windows x64 and Linux x64 artifacts
without publishing. A matching `vVERSION` tag or explicit publish dispatch must
identify the selected commit. Publication waits for every native build and the
complete artifact verification. Creating an existing release fails; existing
assets are not overwritten. Local execution of these packaging tests does not
replace the three native CI builds.
