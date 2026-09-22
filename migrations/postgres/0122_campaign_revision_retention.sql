-- Each revision stores a full copy of the DAG, so an active campaign grew the
-- history unboundedly; 0121's "every accepted revision is retained" no longer
-- holds. Writes now keep only the newest few, and this reclaims the backlog that
-- accumulated before that bound existed, including campaigns that never get
-- written again. The literal mirrors campaigns::REVISION_RETENTION.
DELETE FROM campaign_revisions AS victim
USING (
    SELECT
        campaign_id,
        revision,
        ROW_NUMBER() OVER (PARTITION BY campaign_id ORDER BY revision DESC) AS recency
    FROM campaign_revisions
) AS ranked
WHERE victim.campaign_id = ranked.campaign_id
  AND victim.revision = ranked.revision
  AND ranked.recency > 10;
