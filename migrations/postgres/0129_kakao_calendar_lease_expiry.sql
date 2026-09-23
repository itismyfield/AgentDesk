-- Keep lease recovery proportional to active work rather than retained history.
CREATE INDEX kakao_calendar_operations_lease_expiry
    ON kakao_calendar_operations (lease_expires_at)
    WHERE status IN ('preparing', 'dispatching');
