-- Background jobs (Task 9): expiry, ship deadline, auto-release, backstop,
-- payouts, the refund sweeper and the lost-webhook re-verify.
-- Personal data: none. job_runs holds job names, timestamps and counts only.

-- Owner kill switch for the ship-deadline job (it refunds and strikes sellers).
ALTER TABLE platform_settings
    ADD COLUMN jobs_ship_deadline_enabled boolean NOT NULL DEFAULT true;

-- Refund sweeper: when a `requested` refund may be retried next. Set by the
-- job when it claims the row, so a refund the provider keeps refusing backs
-- off instead of being re-sent every tick.
ALTER TABLE refunds ADD COLUMN next_attempt_at timestamptz;

-- Lost-webhook re-verify of expired checkouts: the job's own backoff.
ALTER TABLE checkouts
    ADD COLUMN reverify_attempts integer NOT NULL DEFAULT 0 CHECK (reverify_attempts >= 0),
    ADD COLUMN next_reverify_at  timestamptz;

-- One row per job: when it last ran and last finished without a failure,
-- and the counts of that run (for alerting on a stalled job; Task 10 shows it).
CREATE TABLE job_runs (
    job             text PRIMARY KEY CHECK (length(job) BETWEEN 1 AND 50),
    last_run_at     timestamptz NOT NULL,
    last_success_at timestamptz,
    processed       integer NOT NULL DEFAULT 0 CHECK (processed >= 0),
    failed          integer NOT NULL DEFAULT 0 CHECK (failed >= 0)
);

-- Partial indexes for each job's claim query (checkout expiry already has
-- checkouts_pending_expiry_idx from 0010).
CREATE INDEX orders_ship_due_idx ON orders (ship_by, id)
    WHERE status = 'paid' AND held_at IS NULL;
CREATE INDEX orders_release_due_idx ON orders (release_at, id)
    WHERE status = 'delivered' AND held_at IS NULL;
CREATE INDEX orders_backstop_due_idx ON orders (backstop_at, id)
    WHERE backstop_flagged_at IS NULL AND status NOT IN ('completed', 'refunded', 'cancelled');
CREATE INDEX payouts_send_due_idx ON payouts (next_attempt_at, id)
    WHERE status IN ('scheduled', 'unknown');
CREATE INDEX payouts_sending_lease_idx ON payouts (sending_until, id)
    WHERE status = 'sending';
CREATE INDEX refunds_requested_due_idx ON refunds (next_attempt_at, id)
    WHERE status = 'requested';
CREATE INDEX checkouts_reverify_due_idx ON checkouts (expires_at, id)
    WHERE status = 'expired' AND provider_payment_id IS NOT NULL;
