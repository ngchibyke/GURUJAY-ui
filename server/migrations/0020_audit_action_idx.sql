-- The reconciliation's alert sections (late provider outcomes, payout amount
-- mismatches) read recent audit rows by action.
CREATE INDEX audit_log_action_at_idx ON audit_log (action, at);
