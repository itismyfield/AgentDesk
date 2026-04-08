# Phase 8: 보고 템플릿

실행 완료 후 아래 형식으로 결과를 출력한다:

```markdown
## Memory Merge 결과 ({날짜})

### 위생 정리
- 상대날짜 변환: {N}건
- 모순 제거: {N}건
- stale 제거: {N}건
- 중복 합침: {N}건
- 인덱스 수정: {N}건

### 분배 결과
- backend: {file|memento|mem0}
- 스캔: {N}개 파일 / {N}개 워크스페이스
- -> SAM: {N}건
- -> SAK: {N}건
- -> LTM: {N}건
- -> Memento 기록: {N}건
- -> Mem0 기록: {N}건
- -> backend 불일치 skip: {N}건
- -> System Prompt 검토 필요: {N}건
- 폐기: {N}건
```
