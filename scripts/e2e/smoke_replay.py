#!/usr/bin/env python3
"""Replay read-only deploy-log snapshots; missing live evidence stays unknown."""
import argparse
import collections
import datetime as dt
import hashlib
import json
import re
from pathlib import Path

KST = dt.timezone(dt.timedelta(hours=9))
CUTOFF = '20261105T150000Z'
STAMP = re.compile(r'post-deploy-smoke(?:-turns)?-(\d{8}T\d{6}Z-\d+)')
RESULT = re.compile(r'relay (E-5[01]) cell=(\S+) result=(\S+)')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def parse(text, host, source, sha):
    runs = set(STAMP.findall(text))
    if len(runs) > 1:
        raise ValueError(f'ambiguous smoke run: {source}')
    if not runs:
        return []
    run = runs.pop()
    if run[:16] < CUTOFF:
        return []
    stamp = dt.datetime.strptime(run[:16], '%Y%m%dT%H%M%SZ').replace(tzinfo=dt.timezone.utc)
    head = re.search(r'Deploy target pinned: ([a-f0-9]{40})', text)
    short_head = re.search(r'Build source:.*? head=([a-f0-9]+)', text)
    summaries = re.findall(r'turn scenarios: E-50=(\S+) E-51=(\S+)', text)
    found = {}
    for number, line in enumerate(text.splitlines(), 1):
        match = RESULT.search(line)
        if match:
            scenario, cell, result = match.groups()
            if scenario in found:
                raise ValueError(f'duplicate result in {source}: {scenario}')
            found[scenario] = (cell, result.upper(), line, number)
    rows = []
    for index, scenario in enumerate(('E-50', 'E-51')):
        cell, result, line, number = found.get(scenario, (None, summaries[-1][index].upper() if summaries else 'UNKNOWN', '', next((i for i, x in enumerate(text.splitlines(), 1) if 'turn scenarios:' in x), None)))
        if not found and not summaries:
            continue
        witness = 'unknown'
        if result == 'FAIL':
            for needle, name in [('HTTP 503', 'external_http_503'), ('cell-reset', 'cell_reset_http_409'), ('warm follow-up readiness', 'composer_readiness_counter'), ('dcserver log excerpt unavailable', 'observer_log_excerpt_unavailable'), ('timeout waiting', 'discord_response_timeout')]:
                if needle in line:
                    witness = name
                    break
        skip = next((x.split('=skipped:', 1)[-1].strip() for x in text.splitlines() if '=skipped:' in x and (f'relay {scenario}=' in x or 'relay E-50/E-51=' in x)), None)
        steps = re.findall(r'(\w+) ([0-9.]+)s', line.split('steps:', 1)[-1]) if 'steps:' in line else []
        markers = re.search(r'deliveries T1=(\S+) T2=(\S+) AFTER_CLEAR=(\S+)', line)
        path = re.search(r' path=(\S+)', line)
        rows.append(dict(
            run_id=run,
            scenario=scenario,
            deploy_host=host,
            smoke_started_kst=stamp.astimezone(KST).isoformat(),
            head=head.group(1) if head else None,
            head_short=short_head.group(1) if short_head else None,
            head_basis='deploy_target_pinned' if head else ('build_source_short' if short_head else 'unknown'),
            result=result,
            attempted=result in ('PASS', 'FAIL'),
            executed=False if result == 'SKIPPED' or witness == 'cell_reset_http_409' else None,
            cell=cell,
            reported_path=path.group(1) if path else None,
            actual_runtime_host=None,
            first_failing_step='cell-reset' if witness == 'cell_reset_http_409' else ('judge/log_counter' if witness == 'composer_readiness_counter' else (steps[-1][0] if result == 'FAIL' and steps else None)),
            failure_witness=witness if result == 'FAIL' else None,
            classification=('외부 503' if witness == 'external_http_503' else '불명') if result == 'FAIL' else None,
            classification_confidence='direct_http_status' if witness == 'external_http_503' else ('unknown' if result == 'FAIL' else None),
            skip_reason=skip if result == 'SKIPPED' else None,
            marker_counts=dict(zip(('T1', 'T2', 'AFTER_CLEAR'), markers.groups())) if markers else None,
            step_sum_s=round(sum(float(x[1]) for x in steps), 3) if steps else None,
            elapsed_s=None,
            acknowledged_posts=None,
            native_turns=None,
            usage=None,
            report_status=None,
            report_path=None,
            scenario_started_local=None,
            scenario_completed_local=None,
            host_metrics_during_scenario=None,
            log_rotation=None,
            evidence_path=str(source),
            evidence_line=number,
            evidence_sha256=sha,
        ))
    return rows


def augment(row, root):
    cell = row['cell']
    if not cell:
        return
    report = root/'reports'/row['deploy_host']/f"post-deploy-smoke-turns-{row['run_id']}--report.{cell}.json"
    if not report.exists():
        return
    data = json.loads(report.read_text())
    if data.get('schema_version') != 2:
        raise ValueError(f'unsupported report schema: {report}')
    matches = [s for s in data.get('scenarios', []) if s.get('id') == row['scenario']]
    if len(matches) != 1:
        raise ValueError(f'report scenario identity mismatch: {report}')
    scenario = matches[0]
    if data.get('run_id') != f"{row['run_id']}-{row['scenario']}-{cell}" or data.get('cell') != cell:
        raise ValueError(f'report run/cell identity mismatch: {report}')
    row.update(executed=scenario.get('executed'), report_status=scenario.get('status'), report_path=str(report), report_sha256=digest(report), scenario_started_local=scenario.get('started_at'), scenario_completed_local=scenario.get('completed_at'), acknowledged_posts=sum(s.get('send_state') == 'acknowledged' for s in scenario.get('prompt_sends', [])) if 'prompt_sends' in scenario else None)
    if row['scenario_started_local'] and row['scenario_completed_local']:
        row['elapsed_s'] = (dt.datetime.fromisoformat(row['scenario_completed_local'])-dt.datetime.fromisoformat(row['scenario_started_local'])).total_seconds()
        if row['elapsed_s'] < 0:
            raise ValueError(f'negative report elapsed: {report}')


def count(rows):
    return dict(sorted(collections.Counter(f"{r['deploy_host']}/{r['scenario']}/{r['result']}" for r in rows).items()))


def build(root):
    rows, inventories, no_smoke = [], [], []
    report_inventory = root/'report-inventory.mac-book.json'
    for item in json.loads(report_inventory.read_text()):
        if digest(Path(item['copy'])) != item['sha256']:
            raise ValueError(f"report snapshot changed: {item['copy']}")
    for host in ('mac-book', 'mac-mini'):
        inventory = root/f'inventory.{host}.json'
        data = json.loads(inventory.read_text())
        inventories.append(dict(host=host, path=str(inventory), sha256=digest(inventory), snapshot_at=data['snapshot_at'], files=len(data['files']), bytes=sum(x['bytes'] for x in data['files'])))
        for item in data['files']:
            source = Path(item['copy'])
            if digest(source) != item['sha256']:
                raise ValueError(f'snapshot changed: {source}')
            parsed = parse(source.read_text(errors='replace'), host, source, item['sha256'])
            rows.extend(parsed)
            if not STAMP.search(source.read_text(errors='replace')) and item['mtime'] >= dt.datetime(2026, 11, 6, tzinfo=KST).timestamp():
                no_smoke.append(dict(host=host, path=str(source), mtime_basis_only=True))
    rows.sort(key=lambda r:(r['smoke_started_kst'], r['deploy_host'], r['scenario']))
    keys = [(r['deploy_host'], r['run_id'], r['scenario']) for r in rows]
    if len(set(keys)) != len(keys):
        raise ValueError('duplicate host/run/scenario; refusing silent deduplication')
    for row in rows:
        augment(row, root)
    boundary_reports = [dict(path=str(p), sha256=digest(p), started_local=r.get('started_at'), completed_local=r.get('completed_at')) for p in (root/'reports/mac-book').glob('*20261105T135229Z-31815*json') for r in json.loads(p.read_text()).get('scenarios', [])]
    original = [r for r in rows if '20261106T000000Z' <= r['run_id'][:16] <= '20261112T035835Z']
    return dict(schema_version=1, cutoff_kst='2026-11-06T00:00:00+09:00', inventory=inventories, report_inventory=dict(path=str(report_inventory), sha256=digest(report_inventory)), time_basis='smoke_run_start_stamp_utc_rendered_kst', boundary_audit=dict(last_prior_book_smoke_kst='2026-11-05T22:52:29+09:00', last_prior_book_report_completed_local=max(r['completed_local'] for r in boundary_reports), reports=boundary_reports, last_prior_mini_smoke_kst='2026-11-05T22:57:21+09:00', note='Prior book report completed before cutoff; mini skipped both scenarios. Report local timestamps have no timezone.'), counts=count(rows), inferred_original_window=dict(status='match_not_identity', start='2026-11-06T09:00:00+09:00', end_smoke_start='2026-11-12T12:58:35+09:00', counts=count(original), reason='Original run list unavailable; both supplied aggregate counts match this UTC-stamp window.'), no_smoke_timestamp_recent_mtime=no_smoke, records=rows)


def render(data):
    rows = data['records']
    lines = ['# S0 배포 스모크 replay', '', '정식 범위는 2026-11-06 00:00 KST 이후 smoke run stamp가 있는 보존 로그 전수다. 시각은 **묶음 시작**이며 개별 시나리오 시작과 다르다. 경계 직전 mac-book 묶음(11-05 22:52:29 KST)의 보고서 완료는 22:55:19 local이며 다음 묶음은 11-06 00:20:55 KST다. mac-mini 직전 묶음(22:57:21 KST)은 둘 다 skip이므로 이 경계에서 실행 날짜 혼입은 관측되지 않았다. 운영 로그를 복사한 뒤 SHA256을 검증했고, 분석 중 운영 파일·프로세스·설정은 변경하지 않았다.', '', '| 호스트 | 보존 deploy 로그 | 바이트 | snapshot 시각 |', '| --- | ---: | ---: | --- |']
    for x in data['inventory']:
        lines.append(f"| {x['host']} | {x['files']} | {x['bytes']} | {dt.datetime.fromisoformat(x['snapshot_at']).astimezone(KST).isoformat()} |")
    lines += ['', '## 전수 결과와 원문 집계 비교', '', '| 범위 | 호스트/시나리오/결과 | 건수 |', '| --- | --- | ---: |']
    for label, counts in [('KST 11-06 이후 전수', data['counts']), ('원문 수치 일치 추정 창', data['inferred_original_window']['counts'])]:
        for key, value in counts.items():
            lines.append(f'| {label} | {key} | {value} |')
    lines += ['', '원문 E-51 30 PASS/7 FAIL과 E-50 24 PASS/3 FAIL은 **11-06 09:00 KST~11-12 12:58:35 KST 묶음 시작** 창에서 함께 재현된다. 원 run-list가 없어 `match_not_identity`이며 이 창을 원래 37건의 확정 identity로 주장하지 않는다. 정식 KST 경계에는 이 창 앞의 실행이 추가되고, 현재 snapshot에는 이후 실행도 포함된다. skip은 실행 0이며 PASS 분모에 넣지 않는다. smoke stamp 없는 최근 mtime 파일은 JSON의 `no_smoke_timestamp_recent_mtime`에 따로 두고 실행 수를 추정하지 않는다.', '', '## 추정 7 FAIL 원문', '', '| 묶음 시작 KST | deploy 로그:행 | head | 첫 실패 단계 | 관측 | 원인 분류 |', '| --- | --- | --- | --- | --- | --- |']
    original_failures = [r for r in rows if r['scenario']=='E-51' and r['result']=='FAIL' and '20261106T000000Z' <= r['run_id'][:16] <= '20261112T035835Z']
    for r in original_failures:
        lines.append(f"| {r['smoke_started_kst']} | {Path(r['evidence_path']).name}:{r['evidence_line']} | {r['head']} | {r['first_failing_step']} | {r['failure_witness']} | {r['classification']} |")
    lines += ['', '7건 모두 원문을 찾았지만 원인 확정은 **불명 7**이다. composer readiness 5건은 scenario report가 pass여도 deploy judge가 로그 카운터로 FAIL을 낸 사례이며 부하 원인은 아니다. 첫 응답 timeout 1건·reset HTTP 409 1건도 과부하/진짜 회귀를 확정할 증거가 없다. 시나리오 중 host 지표·comparable PASS 조건·native turn/usage는 미기록이므로 unknown이다. `path=herdr`는 config/보고 경로 표시라 실제 runtime host나 Herdr 전환을 입증하지 않는다. 외부 HTTP 503은 해당 상태를 직접 적은 실행만 분류하며 다른 실패에 전이하지 않는다.', '', '## 1회 시간·턴 실측', '', '| 호스트/시나리오 | deploy 판정/실제 scenario 실행 | report wall 표본 수 | wall min/median/max 초 | step 합 표본 수 | ACK post 표본 수 | native turn/token |', '| --- | ---: | ---: | --- | ---: | ---: | --- |']
    import statistics
    for scenario in ('E-50', 'E-51'):
        selected = [r for r in rows if r['scenario']==scenario and r['attempted']]
        values = [r['elapsed_s'] for r in selected if r['elapsed_s'] is not None]
        stats = f'{min(values)}/{statistics.median(values)}/{max(values)}' if values else 'unknown'
        lines.append(f"| mac-book/{scenario} | {len(selected)}/{sum(r['executed'] is True for r in selected)} | {len(values)} | {stats} | {sum(r['step_sum_s'] is not None for r in selected)} | {sum(r['acknowledged_posts'] is not None for r in selected)} | unknown/unknown |")
    lines += ['', '각 실행의 wall·step 합·ACK post는 JSON에 있다. wall은 보고서의 timezone 없는 local start/end 차이(초 미만 절삭), step 합은 deploy 로그의 duration 합이다. 둘 다 provider inference 시간이나 묶음 전체 시간은 아니다. ACK post에는 !clear가 들어가므로 provider turn 수로 환산하지 않는다. 누락은 null이며 0으로 채우지 않는다.', '', '## 전수 replay 표', '', '| 묶음 시작 KST | deploy 호스트 | head | E-50 결과/실패 단계/원인 | E-51 결과/실패 단계/원인 | 원문 deploy 로그 |', '| --- | --- | --- | --- | --- | --- |']
    grouped = collections.defaultdict(list)
    for row in rows:
        grouped[(row['smoke_started_kst'], row['deploy_host'], row['run_id'])].append(row)
    for (stamp, host, run), pair in sorted(grouped.items()):
        by_id={r['scenario']:r for r in pair}
        cells=[f"{by_id[s]['result']} / {by_id[s]['first_failing_step'] or '—'} / {by_id[s]['classification'] or '—'}" if s in by_id else 'UNKNOWN' for s in ('E-50','E-51')]
        first=pair[0]
        lines.append(f"| {stamp} | {host} | {first['head'] or ((first['head_short']+' (short)') if first['head_short'] else 'unknown')} | {cells[0]} | {cells[1]} | {Path(first['evidence_path']).name} |")
    lines += ['', '## 재현·판정 경계', '', '```sh', 'python3 scripts/e2e/smoke_replay.py --snapshot /Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8120/logs/s0 --json docs/e2e-continuous/s0-replay.json --markdown docs/e2e-continuous/s0-replay.md', '```', '', '중복 키는 deploy host+smoke run ID+scenario이며 중복·다중 run stamp·report identity 불일치는 오류로 중단한다. start/result/마지막 summary를 한 실행으로 읽으며 summary를 추가 실행으로 세지 않는다. raw snapshot과 report inventory는 레인 logs/s0 아래에 있고 JSON이 SHA·파일·행을 가리킨다. 이번 산출물은 기존 배포 판정의 replay이며 신규 시험 인스턴스의 live PASS나 비용 n으로 합산하지 않는다.']
    return '\n'.join(lines)+'\n'


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapshot', type=Path, required=True)
    parser.add_argument('--json', type=Path, required=True)
    parser.add_argument('--markdown', type=Path, required=True)
    args=parser.parse_args()
    data=build(args.snapshot)
    args.json.write_text(json.dumps(data, ensure_ascii=False, indent=2)+'\n')
    args.markdown.write_text(render(data))
    print(json.dumps(dict(records=len(data['records']), counts=data['counts']), ensure_ascii=False))


if __name__ == '__main__':
    main()
