-- memento-hygiene 심화 패스 (일요일 03:00 KST): cross-workspace 결정론적 정리.
-- 하드가드는 WHERE 에 강제 내장 — 무인 실행 안전장치. 캡 1500.
-- 절대 삭제 안 함: is_anchor / importance>=0.6 / age<14d / family / cookingheart·storycraft.
\set ON_ERROR_STOP on
BEGIN;

CREATE TEMP TABLE _mh_del ON COMMIT DROP AS
SELECT id, type
  FROM agent_memory.fragments
 WHERE valid_to IS NULL
   AND is_anchor = false
   AND importance < 0.6
   AND created_at < now() - interval '14 days'
   AND (workspace IS NULL OR workspace NOT ILIKE '%family%')
   AND lower(coalesce(topic,'')) NOT LIKE '%cookingheart%'
   AND lower(coalesce(topic,'')) NOT LIKE '%storycraft%'
   AND (
        (type = 'episode' AND access_count = 0)
     OR (type = 'error'   AND resolution_status = 'resolved' AND created_at < now() - interval '7 days')
     OR (type = 'fact'    AND topic = 'session_reflect' AND importance <= 0.5)
   )
 ORDER BY importance ASC, created_at ASC
 LIMIT 1500;

\echo DEEP_CANDIDATES
SELECT count(*) FROM _mh_del;
\echo DEEP_BYTYPE
SELECT type || '=' || count(*) FROM _mh_del GROUP BY type ORDER BY type;

\echo DEEP_DELETED
WITH del AS (
  DELETE FROM agent_memory.fragments f USING _mh_del d
   WHERE f.id = d.id
  RETURNING f.id
)
SELECT count(*) FROM del;

COMMIT;
