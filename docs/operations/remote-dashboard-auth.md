# 원격 대시보드 인증

대시보드는 `server.auth_token`을 입력받아 같은 origin의 API 요청에 Bearer 헤더를 붙인다.
토큰은 탭 메모리에만 보관한다. 새로고침 후에는 다시 로그인하며, 로그아웃·토큰 교체·401
응답은 진행 중 요청, HTTP/React Query cache, 재연결 timer와 열린 WebSocket을 정리한다.
취소를 무시하고 늦게 도착한 응답도 이전 인증 세대의 데이터로 취급해 버린다.

`GET /api/auth/session`은 공개 상태 확인 API다. 응답의 `authenticated`가 현재 요청의
인증 여부를 나타낸다. `ok: true`는 상태 조회 성공이며 인증 성공을 대신하지 않는다.
응답은 `Cache-Control: no-store`로 제공한다. 기존 loopback peer + 동일 origin 신뢰와
명시적으로 허용한 무토큰 실행 모드는 유지한다.

브라우저 WebSocket 연결 순서:

1. 인증된 `POST /api/auth/ws-ticket` 요청으로 접속권을 받는다.
2. 15초 이내에 `/ws?ticket=...`로 연결한다. `since` replay cursor는 함께 사용할 수 있다.
3. 서버는 요청 Host와 Origin, 발급 시 Origin, 만료 시각을 확인하고 접속권을 한 번 소비한다.
4. 재연결마다 새 접속권을 발급받는다. 인증이 거절되면 재로그인 화면으로 전환한다.

서버는 접속권 원문 대신 SHA-256 식별자를 메모리에 저장하고, 미소비 접속권은 1,024개로
제한한다. 토큰·접속권을 로그로 출력하지 않는다. TLS reverse proxy를 사용하는 경우 공개
Host와 브라우저 Origin을 보존해야 한다. Forwarded 헤더만으로 다른 Origin을 신뢰하지 않는다.

네이티브 WebSocket 클라이언트는 upgrade 요청에 `Authorization: Bearer ...`를 사용할 수
있다. 장기 토큰을 `/ws?token=...`에 넣던 클라이언트는 이 헤더 또는 일회용 접속권으로
전환해야 한다. 새 대시보드와 서버를 같은 릴리스로 배포한다.

서버 토큰 교체는 기존 `config_live_reload` 계약대로 **서버 재시작이 필요**하다.
REST와 WebSocket이 같은 시작 시 설정을 사용하며, 재시작 시 기존 연결과 접속권이
무효화된다. 브라우저 로그아웃은 해당 탭의 자격 증명을 지우는 동작이며 서버 admin token
자체를 폐기하지 않는다.

검증 범위:

- Rust: 원격/loopback peer의 상태 응답과 API 인증, Origin, 만료, 재사용, 발급 상한,
  실제 TCP WebSocket upgrade의 ticket/Bearer 허용 및 장기 query token 거부.
- HTTP client: 인증 교체 중 취소를 무시한 응답, cache 오염 방지, 401 이후 재시도 중단.
- 브라우저 fixture: desktop/mobile에서 로그인 전 화면 차단, 잘못된 토큰, 재연결,
  토큰 교체 후 재로그인, logout, 새로고침, 저장소·URL 비밀값 부재, 가로 넘침.

브라우저 fixture의 API·WebSocket은 mock이다. 실제 LAN peer를 사용하는 배포 검증은
클러스터 구현 문서에서 별도로 기록한다.
