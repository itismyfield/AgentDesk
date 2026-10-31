# 공유 채널 home 일회 설정

이 절차는 기존 local `/node` 값의 자동 수집이나 자동 이관을 만들지 않는다. 담당 코디네이터가 승인한 `(provider, channel, home instance ID)` 목록으로 한 번 설정한다. 운영 설정 파일과 백업을 읽지 않는다.

1. 코디네이터가 기존 채널의 승인된 home 목록을 확정한다. 명시 선택이 없으면 `inherit`로 확정한다. 서로 다른 값이나 누락은 운영자가 결정할 때까지 활성화 대상에서 제외한다.
2. fallback routing은 계속 끈다. 코디네이터가 배포 창에서 Discord·HTTP·routine을 포함한 새 입력 admission을 보류한다. 이 창 안에서 두 Mac을 PG writer/reader 버전으로 전개하고, 입력을 다시 받기 전에 seed를 마친다. 로컬 값이 PG 결정 권위에서 빠지는 동안 새 세션이 잘못 배치되지 않도록 하는 일회 절차다.
3. `/node`의 PG writer가 준비된 버전에서 해당 provider의 채널마다 승인된 기기를 선택한다. 기본값은 picker의 `기본 라우팅`으로 저장한다. 오프라인인 등록 기기도 home으로 저장할 수 있다. 일반 입력이나 owner 행은 필요 없다.
4. UI가 `저장했습니다`를 응답했는지 확인한다. 이는 owner key를 쥔 동일 트랜잭션의 revision/operation 쓰기, commit, PG read-back이 끝났음을 뜻한다. `미확정`은 성공이 아니다. `/node`를 다시 열어 현재 값을 확인한 뒤 필요하면 새 명령으로 확정한다.
5. 다른 Mac의 새 runtime에서도 `/node`를 열어 같은 값을 확인한다. `기본 라우팅`은 PG에 `inherit`와 증가한 revision으로 남아야 한다. 로컬 map이나 오래된 설정을 다시 읽어 PG에 재적용하지 않는다.
6. 일회 seed 도구를 별도로 사용하는 경우 최초 revision `0`만 허용하고 `write_on_tx`의 Conflict를 덮어쓰지 않는다. 이미 있는 override/reset은 명시 명령의 결과이므로 그대로 둔다. 두 Mac의 reader floor와 채널 목록 확인이 끝난 뒤에만 P-D pilot을 켠다.

`/node` 설정은 execution owner를 바꾸거나 세션을 종료하지 않는다. 이 배포에서는 routing producer와 물리 효과 adapter를 활성화하지 않는다.
