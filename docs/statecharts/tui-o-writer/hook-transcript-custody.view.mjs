export const title = "재시작 뒤 원본 ticket 정산과 채널 문";

function waiting(v) {
  const held = v.barrier ? "barrier 보관, " : "";
  const notes = [];
  if (v.dev !== "same") notes.push("장치 바뀜");
  if (v.file !== "ok") notes.push({ late: "파일 잠시 없음", gone: "파일 없음", changed: "파일 바뀜" }[v.file]);
  if (v.fileCloser) notes.push(v.scen === "rawCloser" ? "개행 없는 closer" : "N1 closer 있음");
  if (v.suffixPrompt) notes.push("N2 입력 있음");
  if (v.switched) notes.push("앞 source의 ticket");
  if (!v.approved) notes.push("파일 승인 없음");
  if (v.otherBad) notes.push("다른 source 복구 실패");
  if (v.writer !== "alive") notes.push("writer 멈춤");
  const note = notes.length ? ` (${notes.join(", ")})` : "";
  if (v.endRec) {
    const end = v.cutKnown ? "N1 종료 기록됨" : "N1 종료 기록됨(길이 모름)";
    return held + end + (v.witFinal ? ", 원본 범위 확정" : ", 원본 범위 확인 중") + note;
  }
  if (v.n2 !== "absent") return held + "후속 N2가 뜸, N1 종료 기록 없음" + note;
  return held + (v.n1 === "alive" ? "N1 실행 중" : "N1 종료됨, 종료 기록 전") + note;
}

export function state(v) {
  if (v.boot !== "ready") return "부팅 거절됨";
  if (v.cleared) {
    if (v.outcome === "Executed") return "채널 열림: 결과와 완료 기록 뒤 정리";
    if (v.clearedBy === "n1idle") return "채널 열림: N1 자신의 idle로 정리";
    if (v.clearedBy === "n1end") {
      return v.outcome === "Empty"
        ? "채널 열림: N1 종료 기록으로 barrier 정리"
        : "채널 열림: N1 종료 기록으로 결과 없이 정리";
    }
    return "채널 열림: 후속 N2의 idle로 정리";
  }
  if (v.outcome === "Executed") {
    const file = v.file === "ok" ? "" : ` (${{ late: "파일 잠시 없음", gone: "파일 없음", changed: "파일 바뀜" }[v.file]})`;
    return (v.completion === "pending" ? "결과 저장됨, 본문 읽기 대기" : "본문 처리 끝, 정리 대기") + file;
  }
  return waiting(v);
}

const labels = {
  N1Closes: "N1이 턴을 끝냄",
  N1Dies: "N1이 죽음",
  FileAppears: "transcript가 다시 보임",
  N2Launch: "후속 N2가 뜸",
  N2Prompt: "N2에 입력이 들어와 파일 승인이 생김",
  N2TakesRow: "N2 턴이 채널 행을 차지함",
  N2Idle: "N2가 idle이 됨",
  WriterCapture: "writer가 N1 범위를 읽음",
  EndScan: "finalizer가 종료 길이까지 N1 source를 읽음",
  ProveEnd: "host가 N1 종료를 확인해 기록함",
  ProveSuperseded: "후속 실행의 시작이 N1 종료를 증명함",
  EndRefresh: "종료 기록과 저장 prefix로 파일을 승인함",
  WriterHalt: "source 하나의 복구가 실패해 writer가 멈춤",
  RawEOFLive: "현재 N1의 개행 없는 closer로 결과를 기록함",
  HistRawEOF: "종료 길이 안의 개행 없는 closer로 결과를 기록함",
  Settle: "N1 결과를 원본 ticket에 기록함",
  FormBarrier: "N2 입력으로 barrier를 저장함",
  OwnIdleAbstain: "N1 자신의 idle로 정리함",
  N2IdleAbstain: "N2의 idle로 정리함",
  EndAbstain: "N1 종료 기록으로 barrier를 정리함",
  ReleaseEnded: "N1 종료 기록으로 결과 없이 정산함",
  ReadCompletion: "봉인된 본문을 읽어 완료를 기록함",
  SkipCompletion: "본문을 읽을 수 없어 완료 기록을 건너뜀",
  Cleanup: "원본 행을 정리함",
};

export function step(action) {
  return labels[action] ?? action;
}

export function group(v) {
  if (v.boot !== "ready") return "blocked";
  if (v.cleared) return "open";
  return v.endRec || v.outcome === "Executed" ? "progress" : "waiting";
}

export const colors = { blocked: "orange", open: "teal", progress: "blue", waiting: "yellow" };
