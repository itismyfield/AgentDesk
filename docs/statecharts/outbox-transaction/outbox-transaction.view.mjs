export const title = "완료·slot·안내문을 같은 transaction에서 확정";
export const state = ({s}) => ({
  start: "호출 시작", begin: "기존 transaction 시작", complete: "완료 상태 갱신",
  release: "slot 해제", silent: "공지 대상 없는 완료", handoff: "잠긴 tx borrow를 서비스로 반환",
  validate: "source 검증", "validate-inner": "tx 안 source 재검증",
  "provider-route": "provider SQL 실행 connection 선택", "insert-route": "INSERT 실행 tx·connection 선택",
  bindings: "같은 connection에서 provider 행 조회", provider1: "첫 행 live default 관측",
  provider2: "다음 행 live default 관측", insert: "관측 값으로 안내문 INSERT",
  census: "INSERT 뒤 같은 task에서 census 관측", producer: "producer host·boot 관측",
  savepoint: "metadata savepoint 시작", metadata: "metadata UPDATE·savepoint 종료",
  commit: "기존 owner가 commit", rollback: "오류 전파·active tx rollback", done: "반환"
}[s.pc] ?? s.pc);
export const group = ({s}) => s.pc === "done" ? "종료" : s.active ? "같은 tx 유지" : "tx 밖";
export const colors = {"종료": "teal", "같은 tx 유지": "blue", "tx 밖": "gray"};
export const step = (action) => ({
  ReturnPreparation: "DB가 서비스에 tx borrow 반환", ChangeSettings: "다른 task의 설정 변경",
  Restart: "재시작: 미커밋 rollback·커밋된 완료 재확인", InsertError: "INSERT 오류",
  BindingQueryError: "provider 조회 오류", SavepointBeginError: "savepoint 시작 오류",
  MetadataOuterError: "savepoint 종료 실패·outer 오류", ValidateInner: "tx 안에서 source 다시 검증",
  ObserveCensus: "새 행에만 post-INSERT 관측", ObserveProducer: "producer stamp 관측",
  Commit: "완료·slot·notice 함께 commit", Rollback: "새 notice 0으로 반환"
}[action] ?? action);
