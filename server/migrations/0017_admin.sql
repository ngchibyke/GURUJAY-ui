-- Owner admin tools (Task 10): manual money overrides, payout re-queue after a
-- reversal, and per-order fee snapshots.
-- Personal data: none added. `manual_reference` is the bank or provider
-- transaction reference the owner checked (never an account number; the
-- route rejects long digit runs in notes). Old payout references are kept in
-- `payout_attempts` (our own references and provider ids only).

ALTER TABLE payouts
    -- Ledger posting keys for success/reversal are scoped to the round, so a
    -- payout reversed and then paid again (re-queue or mark-paid) can book its
    -- success once more. A reversal closes the round and starts the next one.
    ADD COLUMN round            integer NOT NULL DEFAULT 1 CHECK (round >= 1),
    -- Set when a success was reversed; cleared by a re-queue.
    ADD COLUMN reversed_at      timestamptz,
    -- Who settled a `successful` payout: the provider's answer or the owner
    -- (mark-paid). Provider answers never change an owner-settled payout.
    ADD COLUMN settled_by       text CHECK (settled_by IN ('provider', 'owner')),
    ADD COLUMN manual_reference text
        CHECK (manual_reference ~ '^[A-Za-z0-9_./:-]{1,100}$');

ALTER TABLE refunds
    ADD COLUMN settled_by       text CHECK (settled_by IN ('provider', 'owner')),
    ADD COLUMN manual_reference text
        CHECK (manual_reference ~ '^[A-Za-z0-9_./:-]{1,100}$');

-- One row per closed payout attempt (a reversed success). The payout row then
-- gets a NEW reference when re-queued; the old one stays here for support and
-- reconciliation, and is never sent again.
CREATE TABLE payout_attempts (
    id                 bigserial PRIMARY KEY,
    payout_id          uuid NOT NULL REFERENCES payouts (id),
    round              integer NOT NULL CHECK (round >= 1),
    reference          text NOT NULL UNIQUE CHECK (reference ~ '^[A-Z0-9]{1,24}$'),
    provider_payout_id text,
    outcome            text NOT NULL CHECK (outcome IN ('reversed')),
    recorded_at        timestamptz NOT NULL DEFAULT now(),
    UNIQUE (payout_id, round)
);

-- Append-only, like the ledger.
CREATE FUNCTION payout_attempts_reject_change() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'payout_attempts is append-only (% not allowed)', TG_OP;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER payout_attempts_no_update
    BEFORE UPDATE ON payout_attempts
    FOR EACH ROW EXECUTE FUNCTION payout_attempts_reject_change();
CREATE TRIGGER payout_attempts_no_delete
    BEFORE DELETE ON payout_attempts
    FOR EACH ROW EXECUTE FUNCTION payout_attempts_reject_change();
CREATE TRIGGER payout_attempts_no_truncate
    BEFORE TRUNCATE ON payout_attempts
    FOR EACH STATEMENT EXECUTE FUNCTION payout_attempts_reject_change();
ALTER TABLE payout_attempts ENABLE ALWAYS TRIGGER payout_attempts_no_update;
ALTER TABLE payout_attempts ENABLE ALWAYS TRIGGER payout_attempts_no_delete;
ALTER TABLE payout_attempts ENABLE ALWAYS TRIGGER payout_attempts_no_truncate;

-- The seller's payout fee schedule, snapshotted when the order is paid, so an
-- owner fee change never alters an order already paid. NULL on orders paid
-- before this migration: those use the schedule at release, as before.
ALTER TABLE orders
    ADD COLUMN fee_small_kobo            bigint CHECK (fee_small_kobo >= 0),
    ADD COLUMN fee_large_kobo            bigint CHECK (fee_large_kobo >= 0),
    ADD COLUMN fee_threshold_kobo        bigint CHECK (fee_threshold_kobo >= 0),
    ADD COLUMN stamp_duty_kobo           bigint CHECK (stamp_duty_kobo >= 0),
    ADD COLUMN stamp_duty_threshold_kobo bigint CHECK (stamp_duty_threshold_kobo >= 0);

-- Reconciliation and backstop lists.
CREATE INDEX payouts_attention_idx ON payouts (updated_at, id)
    WHERE status IN ('unknown', 'manual', 'failed');
CREATE INDEX refunds_attention_idx ON refunds (updated_at, id)
    WHERE status IN ('manual', 'failed');
CREATE INDEX checkouts_flagged_idx ON checkouts (paid_at, id) WHERE provider_flagged;
CREATE INDEX checkouts_mismatch_idx ON checkouts (created_at, id)
    WHERE status = 'amount_mismatch';
CREATE INDEX orders_backstop_flagged_idx ON orders (backstop_flagged_at, id)
    WHERE backstop_flagged_at IS NOT NULL;
