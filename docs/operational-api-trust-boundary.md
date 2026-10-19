# 운영 API의 LAN·Tailscale 신뢰 경계

## 결정과 적용 범위

[#5721의 2026-09-05 사용자 결정](https://github.com/itismyfield/AgentDesk/issues/5721)은
운영 기기를 Tailscale로 연결하고 같은 집 LAN의 기기도 신뢰한다는 전제에서
다음 운영 설정을 유지한다.

- `server.host: 0.0.0.0`
- `server.auth_token` 미설정
- `server.allow_insecure_nonloopback_bind: true`

이 설치에서는 신뢰하는 LAN·Tailscale 경계 안에서 운영 API를 토큰 없이 노출한다.
Tailscale 주소에만 바인드하거나 토큰을 설정하는 대안은 해당 결정에서 기각되었다.
이는 이 운영 환경에 대한 명시적 선택이며, 모든 LAN이나 Tailscale 연결이 안전하다는
보장 또는 다른 설치의 기본 정책이 아니다.

앞선 [#3870의 2026-06-30 운영 결정](https://github.com/itismyfield/AgentDesk/issues/3870#issuecomment-4839500190)도
Tailscale·신뢰 Wi-Fi 환경에서 같은 바인드와 opt-in을 유지했음을 기록한다.
#5721은 이 선택을 재확인한다. 여기 적힌 설정은 결정 기록이며 현재 실행 중인
서버 설정이나 네트워크 격리를 실측했다는 뜻은 아니다.

## 코드가 보장하는 경계

[#3870의 바인드 보호](../src/server/routes/mod.rs)는 비loopback 호스트에서
토큰이 없거나 공백뿐이고 opt-in도 없으면 실제 바인드 주소를 loopback으로 바꾼다.
`allow_insecure_nonloopback_bind: true`는 이 강제 loopback 동작을 의도적으로
해제한다. 이 플래그는 인증을 추가하거나 허용할 LAN·Tailscale 주소를 판별하지 않는다.

`0.0.0.0`은 모든 로컬 IPv4 인터페이스에 바인드한다. 따라서 집 LAN·Tailscale이라는
제한은 이 주소 자체로 집행되지 않는다.

[출처 필터](../src/server/web_surface/peer_filter.rs)는 HTTP 표면 전체(`/api/*`,
`/ws`, `/hooks/*`, `/tui/*`, 대시보드)에서 소켓 peer 주소만으로 출처를 가른다.
허용 대역은 loopback, Tailscale(100.64.0.0/10, fd7a:115c:a1e0::/48), 사설 LAN
(10/8·172.16/12·192.168/16·169.254/16, IPv6 fc00::/7·fe80::/10)이다. 프록시 헤더는 보지 않는다.
`server.peer_filter`가 동작을 정한다. `off`는 필터를 두지 않는다. `log`(기본)는
그 밖의 출처를 WARN으로만 기록하고 응답은 바꾸지 않는다. `enforce`는 403으로 거절한다.
기본값 `log`에서는 공인 IP 출처도 통과하므로, 공인 주소가 직접 붙은 호스트에서는
`enforce`로 설정해야 이 경계가 집행된다. 사설 대역 안의 기기는 이 필터로 구분하지 않는다.

[API 인증 미들웨어](../src/server/routes/auth.rs)는 토큰 미설정 시 일반 요청을
통과시킨다. 도달 가능한 호출자는 경로별 추가 제한이 없는 조회·변경 API를
인증 없이 호출할 수 있다. 기존 경로별 제한은 이 결정이나 바인드 opt-in으로
해제되지 않는다. 예를 들어 `/tui/*`, `/hooks/*` 및 내부 제어 경로는 loopback
접속 또는 설정된 Bearer 토큰을 요구하므로 토큰 미설정 상태의 원격 호출을 거부한다.
[`/api/discord/send`](../src/server/routes/health_api.rs)에도 별도의 loopback 또는
Bearer 검사가 있다.

[`/ws`](../src/server/ws.rs)는 토큰 미설정 시 토큰 검사 없이 연결을 허용한다.
토큰을 설정한 설치의 브라우저는 `POST /api/auth/ws-ticket`으로 발급한 1회용
`?ticket=` 값을 쓸 수 있다. 티켓은 Origin·Host에 묶이고 15초 후 만료된다.
네이티브 클라이언트는 Bearer 토큰을 보낼 수 있다. 티켓이 없고 토큰도 미설정이면
WebSocket 인증은 통과하므로 출처 필터 설정도 함께 확인해야 한다.

실제 노출 경로는 [`/api/docs`](../src/server/routes/docs/inventory/endpoints/public_surfaces.rs)
인벤토리에 정리돼 있다. `/tui/send`, `/tui/wait`, `/hooks/{provider}/{event}`는
loopback 피어나 설정된 유효한 Bearer 토큰만 허용하며, 토큰 미설정이면 loopback만
허용한다. `/ws`는 위 티켓/Bearer 규칙과 peer filter 설정을 따른다.

`server.allow_insecure_nonloopback_bind: true`가 무토큰 비loopback 바인드를 허용하면
dcserver는 이 선택과 문서 경로를 시작 로그의 INFO 항목으로 남긴다. `agentdesk doctor`도
`API bind exposure` 항목을 INFO로 표시한다. INFO는 사용자가 명시한 신뢰 경계 결정을
기록하며, 외부 네트워크 노출을 안전하다고 판정하지 않는다.

## 감수한 위험과 재검토 조건

신뢰 기기가 침해되거나 비신뢰 기기가 해당 포트에 접근하게 되면, 일반 API에는
그 호출자를 거부할 토큰 인증 경계가 없다. 조회 데이터 노출과 운영 상태 변경이
가능한 노출면이라는 점을 포함해 이 운영 선택을 해석해야 한다.

다음 변화가 있으면 무토큰 운영 결정을 다시 검토한다.

- 외부 노출, 포트 포워딩, 공개 프록시·터널 또는 공용 네트워크로 접근 범위가 넓어짐.
- 집 LAN·Tailscale에 더 이상 신뢰하지 않는 기기나 사용자가 접근할 수 있게 됨.
- 호스트·네트워크 접근 통제 또는 API의 권한·데이터 범위가 바뀜.

#5721의 결정에 따라 이 신뢰 경계를 벗어나는 배포에서는 `server.auth_token`
설정이 필수다. 접근 범위를 바꾸기 전에 바인드와 네트워크 통제도 함께 재검토한다.

`/ws`와 일반 API 인증은 서버 시작 시 읽은 동일한 `server.auth_token` 스냅샷을
사용한다. 실행 중 설정 파일이 바뀌어도 인증 상태는 즉시 바뀌지 않으며, 토큰 변경은
서버 재시작 후 적용된다. 재시작은 기존 WebSocket 연결과 발급된 ticket도 무효화한다.

`/api/*` 요청 제한(rate limit)은 #5721에서 구현 대상에서 제외하고 기록만 남긴
항목이다. 이 결정은 요청 폭주에 대한 방어를 보장하지 않는다.

`agentdesk doctor`와 부팅 로그는 현재 설정을 요약한다. 운영 중 실제 접근 가능한
인터페이스나 네트워크 장치의 신뢰 상태를 측정하지는 않는다.
