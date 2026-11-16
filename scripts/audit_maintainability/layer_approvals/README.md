# 모듈 층 증가 승인

`module_layers` 검사(`checks/module_layer_gate.py`)는 main(base)과 후보를 같은 실행에서 각각 재고,
새로 늘어난 역방향 부채만 막는다. 정당한 증가는 이 디렉터리에 **PR마다 새 파일 하나**로 적는다.
계약 정본은 [`docs/architecture/layers.md`](../../../docs/architecture/layers.md)의 "증가 승인" 절이다.

- 파일 이름: `<이슈 번호>-<slug>.toml` (`^[0-9]+-[a-z0-9-]+\.toml$`).
- 효력: base에는 없고 후보에만 있는 파일의 항목만 효력이 있다. main에 착지한 파일은 다시는 효력이 없다.
- 착지한 파일은 고치거나 지우지 않는다. 수정·삭제는 검사 오류다. 같은 부채를 다시 올리려면 새 파일을 만든다.
- 쓰이지 않는 항목, `max_count` 초과, `TODO`로 시작하는 `reason`·`summary`는 오류다.

초안 명령은 현재 막힌 증가만 담은 `<이름>.toml`을 이 디렉터리에 쓴다(기존 파일은 덮어쓰지 않는다).
`summary`와 `TODO` 사유를 검토해 채우고, 이동은 주석 줄을 참고해 `move`로 직접 적는다.

```sh
python3 scripts/audit_maintainability/checks/module_layer_gate.py --draft-approval 7353-s14d
```

```toml
schema_version = 1
issue = 7353
summary = "S14d: services::x 분류 편입과 이동"
increase = [
  { section = "reverse_dependencies", id = "<sha256>", source = "src/lib.rs::crate::a", target = "src/lib.rs::crate::b", kind = "import", max_count = 2, reason = "admit-classification" },
]
move = [
  { section = "reverse_dependencies", old_id = "<sha256>", new_id = "<sha256>", reason = "a::x를 b::x로 이동" },
]
reclassify = [
  { prefix = "services::discord::tmux_error_detect", reason = "known 층 정교화" },
]
analyzer = [
  { digest = "<sha256>", reverse_removed = 0, reverse_added = 0, reason = "측정기 수리" },
]
```
