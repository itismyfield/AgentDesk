# S0 배포 스모크 replay

정식 범위는 2026-11-06 00:00 KST 이후 smoke run stamp가 있는 보존 로그 전수다. 시각은 **묶음 시작**이며 개별 시나리오 시작과 다르다. 경계 직전 mac-book 묶음(11-05 22:52:29 KST)의 보고서 완료는 22:55:19 local이며 다음 묶음은 11-06 00:20:55 KST다. mac-mini 직전 묶음(22:57:21 KST)은 둘 다 skip이므로 이 경계에서 실행 날짜 혼입은 관측되지 않았다. 운영 로그를 복사한 뒤 SHA256을 검증했고, 분석 중 운영 파일·프로세스·설정은 변경하지 않았다.

| 호스트 | 보존 deploy 로그 | 바이트 | snapshot 시각 |
| --- | ---: | ---: | --- |
| mac-book | 398 | 34757622 | 2026-11-13T14:19:07.251875+09:00 |
| mac-mini | 352 | 27428192 | 2026-11-13T14:19:27.360140+09:00 |

## 전수 결과와 원문 집계 비교

| 범위 | 호스트/시나리오/결과 | 건수 |
| --- | --- | ---: |
| KST 11-06 이후 전수 | mac-book/E-50/FAIL | 8 |
| KST 11-06 이후 전수 | mac-book/E-50/PASS | 26 |
| KST 11-06 이후 전수 | mac-book/E-50/SKIPPED | 14 |
| KST 11-06 이후 전수 | mac-book/E-51/FAIL | 8 |
| KST 11-06 이후 전수 | mac-book/E-51/PASS | 37 |
| KST 11-06 이후 전수 | mac-book/E-51/SKIPPED | 3 |
| KST 11-06 이후 전수 | mac-mini/E-50/SKIPPED | 46 |
| KST 11-06 이후 전수 | mac-mini/E-51/SKIPPED | 46 |
| 원문 수치 일치 추정 창 | mac-book/E-50/FAIL | 3 |
| 원문 수치 일치 추정 창 | mac-book/E-50/PASS | 24 |
| 원문 수치 일치 추정 창 | mac-book/E-50/SKIPPED | 13 |
| 원문 수치 일치 추정 창 | mac-book/E-51/FAIL | 7 |
| 원문 수치 일치 추정 창 | mac-book/E-51/PASS | 30 |
| 원문 수치 일치 추정 창 | mac-book/E-51/SKIPPED | 3 |
| 원문 수치 일치 추정 창 | mac-mini/E-50/SKIPPED | 38 |
| 원문 수치 일치 추정 창 | mac-mini/E-51/SKIPPED | 38 |

원문 E-51 30 PASS/7 FAIL과 E-50 24 PASS/3 FAIL은 **11-06 09:00 KST~11-12 12:58:35 KST 묶음 시작** 창에서 함께 재현된다. 원 run-list가 없어 `match_not_identity`이며 이 창을 원래 37건의 확정 identity로 주장하지 않는다. 정식 KST 경계에는 이 창 앞의 실행이 추가되고, 현재 snapshot에는 이후 실행도 포함된다. skip은 실행 0이며 PASS 분모에 넣지 않는다. smoke stamp 없는 최근 mtime 파일은 JSON의 `no_smoke_timestamp_recent_mtime`에 따로 두고 실행 수를 추정하지 않는다.

## 추정 7 FAIL 원문

| 묶음 시작 KST | deploy 로그:행 | head | 첫 실패 단계 | 관측 | 원인 분류 |
| --- | --- | --- | --- | --- | --- |
| 2026-11-06T13:29:37+09:00 | deploy-release.96130.log:1304 | a2ab3f26df7059ecf13d9ae9f7fcffa418560089 | wait_for_discord_text | discord_response_timeout | 불명 |
| 2026-11-06T20:34:38+09:00 | deploy-release.23303.log:1290 | 9f13fe20575d342426fc37fdb0fac7847b78391c | judge/log_counter | composer_readiness_counter | 불명 |
| 2026-11-07T07:27:09+09:00 | deploy-release.15875.log:1285 | 4f0a0f4b1ca1d0a85964dbc666d60f2c7aaf1dd6 | cell-reset | cell_reset_http_409 | 불명 |
| 2026-11-08T15:28:06+09:00 | deploy-release.62406.log:1305 | 4e35af0c1e607a8ae5c569005c1d34fe0bf00334 | judge/log_counter | composer_readiness_counter | 불명 |
| 2026-11-09T05:26:42+09:00 | deploy-release.8707.log:1301 | 4aa7307bada171b12565f4aac305ec83658a2ed5 | judge/log_counter | composer_readiness_counter | 불명 |
| 2026-11-09T09:38:17+09:00 | deploy-release.35921.log:1306 | db2bbe33a97c08a2377074c92043f5d38b6d4e93 | judge/log_counter | composer_readiness_counter | 불명 |
| 2026-11-10T07:39:03+09:00 | deploy-release.99093.log:1303 | 60672f632ed714156a61f386b579457c36ec97ca | judge/log_counter | composer_readiness_counter | 불명 |

7건 모두 원문을 찾았지만 원인 확정은 **불명 7**이다. composer readiness 5건은 scenario report가 pass여도 deploy judge가 로그 카운터로 FAIL을 낸 사례이며 부하 원인은 아니다. 첫 응답 timeout 1건·reset HTTP 409 1건도 과부하/진짜 회귀를 확정할 증거가 없다. 시나리오 중 host 지표·comparable PASS 조건·native turn/usage는 미기록이므로 unknown이다. `path=herdr`는 config/보고 경로 표시라 실제 runtime host나 Herdr 전환을 입증하지 않는다. 외부 HTTP 503은 해당 상태를 직접 적은 실행만 분류하며 다른 실패에 전이하지 않는다.

## 1회 시간·턴 실측

| 호스트/시나리오 | deploy 판정/실제 scenario 실행 | report wall 표본 수 | wall min/median/max 초 | step 합 표본 수 | ACK post 표본 수 | native turn/token |
| --- | ---: | ---: | --- | ---: | ---: | --- |
| mac-book/E-50 | 34/34 | 34 | 58.0/67.0/98.0 | 34 | 34 | unknown/unknown |
| mac-book/E-51 | 45/44 | 44 | 60.0/66.5/88.0 | 44 | 44 | unknown/unknown |

각 실행의 wall·step 합·ACK post는 JSON에 있다. wall은 보고서의 timezone 없는 local start/end 차이(초 미만 절삭), step 합은 deploy 로그의 duration 합이다. 둘 다 provider inference 시간이나 묶음 전체 시간은 아니다. ACK post에는 !clear가 들어가므로 provider turn 수로 환산하지 않는다. 누락은 null이며 0으로 채우지 않는다.

## 전수 replay 표

| 묶음 시작 KST | deploy 호스트 | head | E-50 결과/실패 단계/원인 | E-51 결과/실패 단계/원인 | 원문 deploy 로그 |
| --- | --- | --- | --- | --- | --- |
| 2026-11-06T00:20:55+09:00 | mac-book | d79d8f6a60e7 (short) | FAIL / wait_for_discord_text / 불명 | PASS / — / — | deploy-release.5276.log |
| 2026-11-06T04:40:48+09:00 | mac-book | fba8d4699f9be24652131aac0cfb0e366b56550e | FAIL / wait_for_discord_text / 불명 | FAIL / judge/log_counter / 불명 | deploy-release.544.log |
| 2026-11-06T04:45:33+09:00 | mac-mini | fba8d4699f9be24652131aac0cfb0e366b56550e | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-fba8d4699f9be24652131aac0cfb0e366b56550e.646.log |
| 2026-11-06T08:20:47+09:00 | mac-book | 7a0c819e127964befd6bc0d022c5d4194d669c9c | SKIPPED / — / — | PASS / — / — | deploy-release.37780.log |
| 2026-11-06T08:24:16+09:00 | mac-mini | 7a0c819e127964befd6bc0d022c5d4194d669c9c | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-7a0c819e127964befd6bc0d022c5d4194d669c9c.37819.log |
| 2026-11-06T13:29:14+09:00 | mac-mini | a2ab3f26df7059ecf13d9ae9f7fcffa418560089 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-a2ab3f26df7059ecf13d9ae9f7fcffa418560089.96191.log |
| 2026-11-06T13:29:37+09:00 | mac-book | a2ab3f26df7059ecf13d9ae9f7fcffa418560089 | SKIPPED / — / — | FAIL / wait_for_discord_text / 불명 | deploy-release.96130.log |
| 2026-11-06T16:47:49+09:00 | mac-book | 0bf73d0e2f6a83f7501fe9f8d555282e0e5d1c6a | SKIPPED / — / — | PASS / — / — | deploy-release.95866.log |
| 2026-11-06T16:51:20+09:00 | mac-mini | 0bf73d0e2f6a83f7501fe9f8d555282e0e5d1c6a | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-0bf73d0e2f6a83f7501fe9f8d555282e0e5d1c6a.95943.log |
| 2026-11-06T20:34:38+09:00 | mac-book | 9f13fe20575d342426fc37fdb0fac7847b78391c | SKIPPED / — / — | FAIL / judge/log_counter / 불명 | deploy-release.23303.log |
| 2026-11-06T20:38:06+09:00 | mac-mini | 9f13fe20575d342426fc37fdb0fac7847b78391c | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-9f13fe20575d342426fc37fdb0fac7847b78391c.23393.log |
| 2026-11-06T22:57:44+09:00 | mac-book | a2fdbaf6c52190a8cb1d6b45989fe9f76eb12433 | SKIPPED / — / — | PASS / — / — | deploy-release.84365.log |
| 2026-11-06T23:01:22+09:00 | mac-mini | a2fdbaf6c52190a8cb1d6b45989fe9f76eb12433 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-a2fdbaf6c52190a8cb1d6b45989fe9f76eb12433.84450.log |
| 2026-11-07T07:27:09+09:00 | mac-book | 4f0a0f4b1ca1d0a85964dbc666d60f2c7aaf1dd6 | SKIPPED / — / — | FAIL / cell-reset / 불명 | deploy-release.15875.log |
| 2026-11-07T07:28:17+09:00 | mac-mini | 4f0a0f4b1ca1d0a85964dbc666d60f2c7aaf1dd6 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-4f0a0f4b1ca1d0a85964dbc666d60f2c7aaf1dd6.15924.log |
| 2026-11-07T11:50:38+09:00 | mac-book | 5f992ab0671ac3725928513357462f7c342fa247 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.62168.log |
| 2026-11-07T17:13:36+09:00 | mac-book | 567a860d362fcfc4bcbf2efa0b254904c5c56555 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.60773.log |
| 2026-11-07T17:16:26+09:00 | mac-mini | 567a860d362fcfc4bcbf2efa0b254904c5c56555 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-567a860d362fcfc4bcbf2efa0b254904c5c56555.60882.log |
| 2026-11-08T03:35:23+09:00 | mac-book | 77d5a3a813605944dbe62592ba43b1a435251b14 | SKIPPED / — / — | PASS / — / — | deploy-release.99400.log |
| 2026-11-08T03:38:57+09:00 | mac-mini | 77d5a3a813605944dbe62592ba43b1a435251b14 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-77d5a3a813605944dbe62592ba43b1a435251b14.99475.log |
| 2026-11-08T05:49:16+09:00 | mac-book | 05ffdc444145894915b054733da50295e9fafc88 | PASS / — / — | PASS / — / — | deploy-release.35770.log |
| 2026-11-08T05:53:56+09:00 | mac-mini | 05ffdc444145894915b054733da50295e9fafc88 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-05ffdc444145894915b054733da50295e9fafc88.35889.log |
| 2026-11-08T09:24:08+09:00 | mac-book | 0bcce42042b359e3acc93b5489563fb13635ac38 | PASS / — / — | PASS / — / — | deploy-release.68240.log |
| 2026-11-08T09:28:42+09:00 | mac-mini | 0bcce42042b359e3acc93b5489563fb13635ac38 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-0bcce42042b359e3acc93b5489563fb13635ac38.68329.log |
| 2026-11-08T11:38:08+09:00 | mac-book | 2b469823b8a6cb01b29d77bcd9cedec15b376d9b | PASS / — / — | SKIPPED / — / — | deploy-release.85218.log |
| 2026-11-08T11:41:47+09:00 | mac-mini | 2b469823b8a6cb01b29d77bcd9cedec15b376d9b | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-2b469823b8a6cb01b29d77bcd9cedec15b376d9b.85285.log |
| 2026-11-08T15:28:06+09:00 | mac-book | 4e35af0c1e607a8ae5c569005c1d34fe0bf00334 | PASS / — / — | FAIL / judge/log_counter / 불명 | deploy-release.62406.log |
| 2026-11-08T15:32:48+09:00 | mac-mini | 4e35af0c1e607a8ae5c569005c1d34fe0bf00334 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-4e35af0c1e607a8ae5c569005c1d34fe0bf00334.62526.log |
| 2026-11-08T18:57:45+09:00 | mac-book | cecf67e96fce31ff4ec4d144011fdfd120ab8488 | SKIPPED / — / — | PASS / — / — | deploy-release.35568.log |
| 2026-11-08T19:01:11+09:00 | mac-mini | cecf67e96fce31ff4ec4d144011fdfd120ab8488 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-cecf67e96fce31ff4ec4d144011fdfd120ab8488.35667.log |
| 2026-11-08T22:23:56+09:00 | mac-book | 195b3cb11170c3896002c79c2fc33e57e88483d0 | SKIPPED / — / — | PASS / — / — | deploy-release.95319.log |
| 2026-11-08T22:27:30+09:00 | mac-mini | 195b3cb11170c3896002c79c2fc33e57e88483d0 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-195b3cb11170c3896002c79c2fc33e57e88483d0.95379.log |
| 2026-11-09T00:30:22+09:00 | mac-book | 8562ce07df0f92cda86c184bf445a58e2aaa6111 | SKIPPED / — / — | PASS / — / — | deploy-release.19271.log |
| 2026-11-09T00:33:54+09:00 | mac-mini | 8562ce07df0f92cda86c184bf445a58e2aaa6111 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-8562ce07df0f92cda86c184bf445a58e2aaa6111.19362.log |
| 2026-11-09T03:58:15+09:00 | mac-book | 794c0a7985ae15e5a370d572c8e783d7351efa95 | SKIPPED / — / — | PASS / — / — | deploy-release.93762.log |
| 2026-11-09T04:01:57+09:00 | mac-mini | 794c0a7985ae15e5a370d572c8e783d7351efa95 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-794c0a7985ae15e5a370d572c8e783d7351efa95.93871.log |
| 2026-11-09T05:26:42+09:00 | mac-book | 4aa7307bada171b12565f4aac305ec83658a2ed5 | SKIPPED / — / — | FAIL / judge/log_counter / 불명 | deploy-release.8707.log |
| 2026-11-09T05:30:23+09:00 | mac-mini | 4aa7307bada171b12565f4aac305ec83658a2ed5 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-4aa7307bada171b12565f4aac305ec83658a2ed5.8819.log |
| 2026-11-09T09:38:17+09:00 | mac-book | db2bbe33a97c08a2377074c92043f5d38b6d4e93 | PASS / — / — | FAIL / judge/log_counter / 불명 | deploy-release.35921.log |
| 2026-11-09T09:43:02+09:00 | mac-mini | db2bbe33a97c08a2377074c92043f5d38b6d4e93 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-db2bbe33a97c08a2377074c92043f5d38b6d4e93.36016.log |
| 2026-11-09T12:46:49+09:00 | mac-book | d75f772bd23d27b24d54af134753dbee456ebda2 | PASS / — / — | PASS / — / — | deploy-release.17399.log |
| 2026-11-09T12:51:30+09:00 | mac-mini | d75f772bd23d27b24d54af134753dbee456ebda2 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-d75f772bd23d27b24d54af134753dbee456ebda2.17533.log |
| 2026-11-09T15:38:48+09:00 | mac-book | 7a24376d71efdbde2ba9163b4abc72684e8d26b6 | PASS / — / — | PASS / — / — | deploy-release.92361.log |
| 2026-11-09T15:43:40+09:00 | mac-mini | 7a24376d71efdbde2ba9163b4abc72684e8d26b6 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-7a24376d71efdbde2ba9163b4abc72684e8d26b6.92461.log |
| 2026-11-09T20:31:42+09:00 | mac-book | 13d54d91e175bbfa1e6cde0be1fd631820a14d73 | PASS / — / — | PASS / — / — | deploy-release.78219.log |
| 2026-11-09T20:36:32+09:00 | mac-mini | 13d54d91e175bbfa1e6cde0be1fd631820a14d73 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-13d54d91e175bbfa1e6cde0be1fd631820a14d73.78320.log |
| 2026-11-09T22:39:11+09:00 | mac-book | c9e1edce4f5dc0030b859354048150f311753782 | PASS / — / — | PASS / — / — | deploy-release.72612.log |
| 2026-11-09T22:44:16+09:00 | mac-mini | c9e1edce4f5dc0030b859354048150f311753782 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-c9e1edce4f5dc0030b859354048150f311753782.72704.log |
| 2026-11-10T00:38:10+09:00 | mac-book | 6aa89c408cd365220f843e908820ecf95daf14f5 | FAIL / wait_for_discord_text / 불명 | PASS / — / — | deploy-release.25357.log |
| 2026-11-10T00:43:13+09:00 | mac-mini | 6aa89c408cd365220f843e908820ecf95daf14f5 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-6aa89c408cd365220f843e908820ecf95daf14f5.25439.log |
| 2026-11-10T04:28:03+09:00 | mac-book | 7d0bea8ed1cad2d8875e40b735cadd18bafe2675 | FAIL / wait_for_discord_text / 불명 | PASS / — / — | deploy-release.45879.log |
| 2026-11-10T04:33:29+09:00 | mac-mini | 7d0bea8ed1cad2d8875e40b735cadd18bafe2675 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-7d0bea8ed1cad2d8875e40b735cadd18bafe2675.45959.log |
| 2026-11-10T07:39:03+09:00 | mac-book | 60672f632ed714156a61f386b579457c36ec97ca | PASS / — / — | FAIL / judge/log_counter / 불명 | deploy-release.99093.log |
| 2026-11-10T07:44:13+09:00 | mac-mini | 60672f632ed714156a61f386b579457c36ec97ca | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-60672f632ed714156a61f386b579457c36ec97ca.99211.log |
| 2026-11-10T10:28:21+09:00 | mac-book | e2f0da2fa6f2d9d610155fb39f284e111e2015ca | PASS / — / — | PASS / — / — | deploy-release.2979.log |
| 2026-11-10T10:33:00+09:00 | mac-mini | e2f0da2fa6f2d9d610155fb39f284e111e2015ca | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-e2f0da2fa6f2d9d610155fb39f284e111e2015ca.3077.log |
| 2026-11-10T12:58:33+09:00 | mac-book | 975200a8e73aa169b22f4d5c91e90bc54dd93693 | PASS / — / — | PASS / — / — | deploy-release.35189.log |
| 2026-11-10T13:03:11+09:00 | mac-mini | 975200a8e73aa169b22f4d5c91e90bc54dd93693 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-975200a8e73aa169b22f4d5c91e90bc54dd93693.35278.log |
| 2026-11-10T15:31:00+09:00 | mac-book | 5918c79bb34d39357b16d16ef32c1e2d081f99b9 | PASS / — / — | PASS / — / — | deploy-release.36803.log |
| 2026-11-10T15:35:36+09:00 | mac-mini | 5918c79bb34d39357b16d16ef32c1e2d081f99b9 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-5918c79bb34d39357b16d16ef32c1e2d081f99b9.36902.log |
| 2026-11-10T18:37:25+09:00 | mac-book | 73615f1feeaadbf693c30197c80b05feef7c329b | FAIL / wait_for_discord_text / 불명 | PASS / — / — | deploy-release.62056.log |
| 2026-11-10T18:42:21+09:00 | mac-mini | 73615f1feeaadbf693c30197c80b05feef7c329b | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-73615f1feeaadbf693c30197c80b05feef7c329b.62136.log |
| 2026-11-10T22:29:04+09:00 | mac-book | e285a5cff1e8f8eadfc87e297ad7a202531f3ed3 | PASS / — / — | PASS / — / — | deploy-release.40149.log |
| 2026-11-10T22:33:51+09:00 | mac-mini | e285a5cff1e8f8eadfc87e297ad7a202531f3ed3 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-e285a5cff1e8f8eadfc87e297ad7a202531f3ed3.40249.log |
| 2026-11-10T23:33:59+09:00 | mac-book | eb116bf8d8a31e3dc5de58f716fc65abad80f652 | PASS / — / — | PASS / — / — | deploy-release.69565.log |
| 2026-11-10T23:38:49+09:00 | mac-mini | eb116bf8d8a31e3dc5de58f716fc65abad80f652 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-eb116bf8d8a31e3dc5de58f716fc65abad80f652.69671.log |
| 2026-11-11T03:25:59+09:00 | mac-book | 2c025e4c81309e10bb5a94f0d12c05c917106320 | PASS / — / — | PASS / — / — | deploy-release.67205.log |
| 2026-11-11T03:30:40+09:00 | mac-mini | 2c025e4c81309e10bb5a94f0d12c05c917106320 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-2c025e4c81309e10bb5a94f0d12c05c917106320.67249.log |
| 2026-11-11T08:00:36+09:00 | mac-book | ef57cb585975c26cb1ff2b7366ec2aa7879e71de | PASS / — / — | PASS / — / — | deploy-release.54525.log |
| 2026-11-11T08:05:27+09:00 | mac-mini | ef57cb585975c26cb1ff2b7366ec2aa7879e71de | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-ef57cb585975c26cb1ff2b7366ec2aa7879e71de.54598.log |
| 2026-11-11T10:47:58+09:00 | mac-book | 92ef5b00b205679965c24c5c017f7237e8b5d57f | PASS / — / — | PASS / — / — | deploy-release.40521.log |
| 2026-11-11T10:52:41+09:00 | mac-mini | 92ef5b00b205679965c24c5c017f7237e8b5d57f | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-92ef5b00b205679965c24c5c017f7237e8b5d57f.40591.log |
| 2026-11-11T14:57:20+09:00 | mac-book | 289f41b1d0f48fab0dc426e652308eced4326fce | PASS / — / — | PASS / — / — | deploy-release.43827.log |
| 2026-11-11T15:02:10+09:00 | mac-mini | 289f41b1d0f48fab0dc426e652308eced4326fce | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-289f41b1d0f48fab0dc426e652308eced4326fce.43962.log |
| 2026-11-11T17:24:28+09:00 | mac-book | 02d5119ea1ecf28e1240e15f40207ad3cc256764 | PASS / — / — | PASS / — / — | deploy-release.77154.log |
| 2026-11-11T17:29:26+09:00 | mac-mini | 02d5119ea1ecf28e1240e15f40207ad3cc256764 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-02d5119ea1ecf28e1240e15f40207ad3cc256764.77242.log |
| 2026-11-11T23:33:02+09:00 | mac-book | b52297188e621e529b5d9f9006fcab0528f129b6 | PASS / — / — | PASS / — / — | deploy-release.83455.log |
| 2026-11-11T23:38:06+09:00 | mac-mini | b52297188e621e529b5d9f9006fcab0528f129b6 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-b52297188e621e529b5d9f9006fcab0528f129b6.83536.log |
| 2026-11-12T04:22:47+09:00 | mac-book | dc6f17d8d36418eec6ce17fd8c0d2ea9c6e01ea4 | PASS / — / — | PASS / — / — | deploy-release.62169.log |
| 2026-11-12T04:27:27+09:00 | mac-mini | dc6f17d8d36418eec6ce17fd8c0d2ea9c6e01ea4 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-dc6f17d8d36418eec6ce17fd8c0d2ea9c6e01ea4.62288.log |
| 2026-11-12T07:51:55+09:00 | mac-book | 230c4fd7b407132d6f9c9d428f4f3270a6213a01 | PASS / — / — | PASS / — / — | deploy-release.4146.log |
| 2026-11-12T07:56:46+09:00 | mac-mini | 230c4fd7b407132d6f9c9d428f4f3270a6213a01 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-230c4fd7b407132d6f9c9d428f4f3270a6213a01.4206.log |
| 2026-11-12T12:58:35+09:00 | mac-book | 0051e898e4e43012c232c244a521c5a280765fb8 | PASS / — / — | PASS / — / — | deploy-release.3839.log |
| 2026-11-12T13:03:15+09:00 | mac-mini | 0051e898e4e43012c232c244a521c5a280765fb8 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-0051e898e4e43012c232c244a521c5a280765fb8.3994.log |
| 2026-11-12T17:39:48+09:00 | mac-book | 29aac79246ac0b0920903608a92ddc83f9a3cfe6 | PASS / — / — | PASS / — / — | deploy-release.38492.log |
| 2026-11-12T17:44:50+09:00 | mac-mini | 29aac79246ac0b0920903608a92ddc83f9a3cfe6 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-29aac79246ac0b0920903608a92ddc83f9a3cfe6.38547.log |
| 2026-11-12T18:56:06+09:00 | mac-book | 4e0db0cccd59169a09f25fe162d8cc1b2755e96e | FAIL / final_checks / 불명 | PASS / — / — | deploy-release.61619.log |
| 2026-11-12T19:01:02+09:00 | mac-mini | 4e0db0cccd59169a09f25fe162d8cc1b2755e96e | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-4e0db0cccd59169a09f25fe162d8cc1b2755e96e.61684.log |
| 2026-11-12T23:53:46+09:00 | mac-book | 59bc11edf28411304cee5e75cc2fc4fdc051c483 | PASS / — / — | PASS / — / — | deploy-release.71166.log |
| 2026-11-12T23:58:39+09:00 | mac-mini | 59bc11edf28411304cee5e75cc2fc4fdc051c483 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-59bc11edf28411304cee5e75cc2fc4fdc051c483.71221.log |
| 2026-11-13T07:58:33+09:00 | mac-book | 5970508240dde40f15cf1d1153d42a0caaf52644 | FAIL / final_checks / 외부 503 | PASS / — / — | deploy-release.40266.log |
| 2026-11-13T08:03:19+09:00 | mac-mini | 5970508240dde40f15cf1d1153d42a0caaf52644 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-5970508240dde40f15cf1d1153d42a0caaf52644.40351.log |
| 2026-11-13T11:40:49+09:00 | mac-book | 79bebdd4eaa29b289bf88412825baa7d70a6ac77 | FAIL / wait_for_discord_text / 불명 | PASS / — / — | deploy-release.18774.log |
| 2026-11-13T11:45:49+09:00 | mac-mini | 79bebdd4eaa29b289bf88412825baa7d70a6ac77 | SKIPPED / — / — | SKIPPED / — / — | deploy-release.cluster-79bebdd4eaa29b289bf88412825baa7d70a6ac77.18899.log |

## 재현·판정 경계

```sh
python3 scripts/e2e/smoke_replay.py --snapshot /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s0 --json docs/e2e-continuous/s0-replay.json --markdown docs/e2e-continuous/s0-replay.md
```

중복 키는 deploy host+smoke run ID+scenario이며 중복·다중 run stamp·report identity 불일치는 오류로 중단한다. start/result/마지막 summary를 한 실행으로 읽으며 summary를 추가 실행으로 세지 않는다. raw snapshot과 report inventory는 레인 logs/s0 아래에 있고 JSON이 SHA·파일·행을 가리킨다. 이번 산출물은 기존 배포 판정의 replay이며 신규 시험 인스턴스의 live PASS나 비용 n으로 합산하지 않는다.
