-- Task 9 fix round: a row whose engine call failed (a lock timeout, say) is
-- pushed back here, so it cannot be reclaimed at the head of the batch every
-- tick and starve newer due rows. Rows long past are pruned by the job loop.
-- Personal data: none (a job name, a row id, counts and timestamps).
CREATE TABLE job_row_failures (
    job         text NOT NULL CHECK (length(job) BETWEEN 1 AND 50),
    row_id      uuid NOT NULL,
    failures    integer NOT NULL CHECK (failures >= 1),
    next_try_at timestamptz NOT NULL,
    PRIMARY KEY (job, row_id)
);
CREATE INDEX job_row_failures_next_try_idx ON job_row_failures (next_try_at);
