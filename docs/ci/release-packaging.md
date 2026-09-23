# Common native release artifacts

The same binary and dashboard package serves a Hub and its Runners.
Runtime configuration, credentials and workspaces stay outside the archive.
Packaging uses tracked, explicitly selected runtime assets.

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

### Windows 사용자 실행 노드

`scripts/install-windows-runtime-task.ps1`은 기존 `%USERPROFILE%\.adk\release` 레이아웃의
`bin\agentdesk.exe`와 `config\agentdesk.yaml`을 확인하고, 해당 사용자로 로그온할 때
`agentdesk dcserver`를 직접 실행하는 작업을 등록한다. PowerShell launcher를 상주시키지
않는다. 중복 시작은 무시하고, 비정상 종료 후 1분 간격으로 최대 999회 재시도한다.
제한된 권한의 `Interactive` principal을 사용하며 암호나 provider 인증 파일을 복사하지 않는다.

```powershell
# 파일과 기존 작업 소유권 확인 후 변경 내용을 미리 보기
.\scripts\install-windows-runtime-task.ps1 -DatabaseSshAlias agentdesk-mac-mini -WhatIf
# 설치된 설정·DB migration 검증 이후 등록하고 시작
.\scripts\install-windows-runtime-task.ps1 -DatabaseSshAlias agentdesk-mac-mini -Start
```

SSH alias를 지정하면 PostgreSQL 터널도 별도 작업으로 등록한다. 기본값은 Windows의
`127.0.0.1:15433`에서 SSH 서버의 `127.0.0.1:5432`로 전달하는 연결이다. SSH는 기존
사용자 설정과 고정된 host key를 사용하며, 비대화식 인증 실패와 포트 충돌 시 종료한다.
실행 노드 DB 설정은 이 로컬 포트를 가리켜야 한다. 터널과 실행 노드 시작 순서의 일시적인
경쟁은 실행 노드의 실패 후 재시도로 복구한다. PostgreSQL LAN listen/HBA 변경은 필요 없다.

이 방식은 사용자가 로그인한 Windows PC의 실행 노드에 적합하다. 재부팅 후 로그온 전이나
로그아웃 상태에서의 실행은 제공하지 않는다. 항상 무인 실행해야 하는 Windows 서버는
별도 서비스 계정과 검증된 service host가 필요하다. 일반 console 실행 파일을 `sc create`에
등록하는 것만으로 서비스 계약이 생긴다고 가정하지 않는다. 설정에 service host가 없는 이
설치 경로의 재시작은 `Stop-ScheduledTask`와 `Start-ScheduledTask`로 관리한다.

Windows 방화벽이 API 수신을 차단하면 관리자 PowerShell에서 패키지의
`scripts/install-windows-worker-firewall.ps1`을 한 번 실행한다. 실행 노드 실행 계정에는
관리자 권한이 필요하지 않다. 규칙은 실행 파일·TCP 포트·허브 IP 주소로 범위를
제한한다. 다른 관리자 계정에서 실행할 수 있으므로 runtime 경로를 명시한다.

```powershell
.\scripts\install-windows-worker-firewall.ps1 -RuntimeRoot 'C:\Users\worker\.adk\release' -HubAddress '192.168.1.147' -WhatIf
.\scripts\install-windows-worker-firewall.ps1 -RuntimeRoot 'C:\Users\worker\.adk\release' -HubAddress '192.168.1.147'
```

포트는 실행 노드의 `server.port`와 같아야 한다. 허브의 고정 IP가 바뀌면 같은 명령을
새 주소로 다시 실행한다. 네트워크 프로필 전체의 기본 수신 정책은 변경하지 않는다.

## Deployment validation and local restart probes

Set `AGENTDESK_POST_DEPLOY_SMOKE_SCOPE=api` to validate API contracts without
sending provider turns or Discord test messages. The default `full` retains
conversation roundtrips. The report explicitly marks omitted coverage.
Account discovery for Qwen, Grok and AGY runs only for configured runtime IDs.

Release deployment preserves existing launchd `StandardOutPath` and
`StandardErrorPath`; missing values use the generated defaults. Local restart
probes send an Origin containing the actual loopback listener port. These probes
use the existing local maintenance authentication contract.
