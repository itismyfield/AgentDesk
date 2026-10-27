# 운영 API의 Tailscale 신뢰 경계

## 결정과 적용 범위

[#7354의 2026-10-24 소유자 결정](https://github.com/itismyfield/AgentDesk/issues/7354)은
**tailnet 밖 기기는 받지 않는다**는 것이다. 이 결정은
[#5721의 2026-09-05 사용자 결정](https://github.com/itismyfield/AgentDesk/issues/5721)의
"같은 집 LAN의 기기도 신뢰한다"는 전제를 대체한다. 신뢰 대상은 이 호스트의
프로세스(loopback)와 Tailscale tailnet 기기뿐이며, 같은 LAN에 있어도 tailnet에 없는
기기는 신뢰하지 않는다.

다음 운영 설정은 tailnet 경계 안에서 유지한다.

- `server.host: 0.0.0.0`
- `server.auth_token` 미설정
- `server.allow_insecure_nonloopback_bind: true`

tailnet 기기(맥북·맥미니·폰·iPad의 대시보드·CLI)가 토큰을 입력할 일은 없다(#5888).
Tailscale 주소에만 바인드하거나 토큰을 설정하는 대안은 #5721에서 기각되었고, 이번
결정에서도 바인드는 바꾸지 않는다. 이는 이 운영 환경에 대한 명시적 선택이며, 모든
Tailscale 연결이 안전하다는 보장 또는 다른 설치의 기본 정책이 아니다.

앞선 [#3870의 2026-06-30 운영 결정](https://github.com/itismyfield/AgentDesk/issues/3870#issuecomment-4839500190)도
Tailscale·신뢰 Wi-Fi 환경에서 같은 바인드와 opt-in을 유지했음을 기록한다.
여기 적힌 설정은 결정 기록이며 현재 실행 중인 서버 설정이나 네트워크 격리를
실측했다는 뜻은 아니다.

## 코드가 보장하는 경계

[#3870의 바인드 보호](../src/server/routes/mod.rs)는 비loopback 호스트에서
토큰이 없거나 공백뿐이고 opt-in도 없으면 실제 바인드 주소를 loopback으로 바꾼다.
`allow_insecure_nonloopback_bind: true`는 이 강제 loopback 동작을 의도적으로
해제한다. 이 플래그는 인증을 추가하거나 허용할 LAN·Tailscale 주소를 판별하지 않는다.

`0.0.0.0`은 모든 로컬 IPv4 인터페이스에 바인드한다. 공인 IPv4 주소가 직접 붙은
인터페이스가 있는 호스트도 그 주소로 오는 TCP 연결을 받는다. 따라서 tailnet이라는
제한은 이 주소 자체로 집행되지 않고, 아래 출처 필터가 HTTP 응답 단계에서 집행한다.

[출처 필터](../src/server/web_surface/peer_filter.rs)는 HTTP 표면 전체(`/api/*`,
`/ws`, `/hooks/*`, `/tui/*`, 대시보드)에서 소켓 peer 주소만으로 출처를 가른다.
프록시 헤더는 보지 않는다. 필터는 주소 대역을 판정할 뿐이며, peer가 이 소유자의
tailnet에 속한 기기인지는 확인하지 않는다. 분류는 loopback(127.0.0.0/8, ::1),
Tailscale(100.64.0.0/10, fd7a:115c:a1e0::/48), 사설 LAN(10/8·172.16/12·192.168/16·
169.254/16, Tailscale 대역 밖의 IPv6 fc00::/7·fe80::/10), 그 밖(공인 등)이다.
IPv4-mapped IPv6는 IPv4로 판정한다.

`server.peer_filter`가 동작을 정한다. 코드 기본값은 `log`이고 운영 노드는 `enforce`를
쓴다. `off`는 필터를 두지 않는다. `log`는 사설 LAN과 그 밖의 출처를 WARN으로만
기록하고 응답은 바꾸지 않는다. `enforce`는 그 밖의 출처(공인 IP 등)를 403
(`peer_not_allowed`)으로 거절한다.

현재 집행은 위 결정의 이전 단계다. 사설 LAN 출처는 어느 모드에서도 통과하되
`peer_class=private_lan` WARN으로 관측한다. 두 노드에 배포한 뒤 연속 5일 동안 이
WARN이 0건이면 기본값을 사설 LAN과 공인 IP를 모두 403으로 거절하는 `tailnet_only`로
바꾸고, `enforce`도 같은 뜻으로 옮긴다. 한 건이라도 나오면 전환하지 않고 출처 IP와
경로를 붙여 사용자 결정으로 올린다.

이 경계는 주소 대역에 기대므로 다음 구성에서는 판정이 달라진다. 이 설치에서 각
구성의 사용 여부는 확인하지 않았다.

- 같은 호스트의 프록시(Tailscale serve 등)를 거친 요청은 loopback peer로 보인다.
  공개 인터넷 요청을 이 호스트로 들여오는 구성(Tailscale Funnel 등)을 쓰면 그 요청도
  loopback으로 보이므로 이 경계로 가를 수 없다.
- subnet router를 거친 요청의 출발 주소는 SNAT 설정에 따라 다르다. SNAT를 켜면
  라우터의 Tailscale 주소로 보여 Tailscale로 분류되고, 끄면 원래 LAN 주소로 보여
  사설 LAN으로 분류된다.
- node sharing으로 이 기기를 공유받은 다른 tailnet의 Tailscale 사용자는 Tailscale
  주소로 접속하므로 Tailscale로 분류된다.

[사람 입력 API](../src/server/routes/agents_turn_deliver.rs)
`POST /api/agents/{id}/turn/deliver`는 본문의 `author_discord_user_id`만으로 허용하지
않는다. 호출자는 이 호스트의 비브라우저 프로세스(loopback peer이고, `Origin`이
없거나 이 서버의 loopback 주소이며, `Sec-Fetch-Site`가 없거나 `same-origin`·`none`이고,
`Forwarded`·`X-Forwarded-For`가 없음)이거나 설정된 `server.auth_token` Bearer를 보내야
한다. 그 밖의 호출자는 경로·본문을 읽기 전에 403(`caller_not_allowed`)을 받는다. 그
다음에 author가 대상 봇의 owner·allowed_user_ids인지 본다. 같은 호스트의 iMessage
브리지는 토큰 없이 loopback으로 이를 증명한다.

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

tailnet 기기나 이 호스트가 침해되거나 비신뢰 기기가 해당 포트에 접근하게 되면, 일반 API에는
그 호출자를 거부할 토큰 인증 경계가 없다. 조회 데이터 노출과 운영 상태 변경이
가능한 노출면이라는 점을 포함해 이 운영 선택을 해석해야 한다.

다음 변화가 있으면 무토큰 운영 결정을 다시 검토한다.

- 외부 노출, 포트 포워딩, 공개 프록시·터널 또는 공용 네트워크로 접근 범위가 넓어짐.
- tailnet에 신뢰하지 않는 기기나 사용자가 참여하거나, subnet router·Funnel·node
  sharing처럼 주소 대역 판정을 바꾸는 구성을 도입함.
- 호스트·네트워크 접근 통제 또는 API의 권한·데이터 범위가 바뀜.

이 신뢰 경계를 벗어나는 배포에서는 `server.auth_token`
설정이 필수다. 접근 범위를 바꾸기 전에 바인드와 네트워크 통제도 함께 재검토한다.

`/ws`와 일반 API 인증은 서버 시작 시 읽은 동일한 `server.auth_token` 스냅샷을
사용한다. 실행 중 설정 파일이 바뀌어도 인증 상태는 즉시 바뀌지 않으며, 토큰 변경은
서버 재시작 후 적용된다. 재시작은 기존 WebSocket 연결과 발급된 ticket도 무효화한다.

`/api/*` 요청 제한(rate limit)은 #5721에서 구현 대상에서 제외하고 기록만 남긴
항목이다. 이 결정은 요청 폭주에 대한 방어를 보장하지 않는다.

`agentdesk doctor`와 부팅 로그는 현재 설정을 요약한다. 운영 중 실제 접근 가능한
인터페이스나 네트워크 장치의 신뢰 상태를 측정하지는 않는다.
