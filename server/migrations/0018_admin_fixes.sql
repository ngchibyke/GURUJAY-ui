-- Task 10 fix round: owner resolution of checkouts that need hand handling,
-- refund timeout tracking, and the fee snapshot backfill.
-- Personal data: none added. `resolution_reference` is the bank or provider
-- reference the owner checked (never an account number: the route refuses a
-- bare 10-digit reference and long digit runs in notes).

-- An amount-mismatch or provider-flagged checkout the owner has handled
-- outside the system (refunded by hand, confirmed with the provider). It only
-- records that handling: no money moves and the ledger is untouched. A
-- resolved checkout leaves the attention lists; the flag itself still gates
-- payouts until it is cleared.
ALTER TABLE checkouts
    ADD COLUMN resolved_at          timestamptz,
    ADD COLUMN resolved_by          uuid REFERENCES users (id),
    ADD COLUMN resolution_reference text
        CHECK (resolution_reference ~ '^[A-Za-z0-9_./:-]{1,100}$'),
    ADD CONSTRAINT checkouts_resolution_complete CHECK (
        (resolved_at IS NULL) = (resolved_by IS NULL)
        AND (resolved_at IS NULL) = (resolution_reference IS NULL)
    );

DROP INDEX checkouts_flagged_idx;
DROP INDEX checkouts_mismatch_idx;
CREATE INDEX checkouts_flagged_idx ON checkouts (paid_at, id)
    WHERE provider_flagged AND resolved_at IS NULL;
CREATE INDEX checkouts_mismatch_idx ON checkouts (created_at, id)
    WHERE status = 'amount_mismatch' AND resolved_at IS NULL;

-- Refund sends whose provider call timed out: the provider may have paid the
-- buyer, so an owner marking the refund done by hand is warned.
ALTER TABLE refunds
    ADD COLUMN timeouts integer NOT NULL DEFAULT 0 CHECK (timeouts >= 0);

-- Orders paid before 0017 and not yet released get the fee schedule in force
-- now, so a later owner fee change no longer alters their payout.
UPDATE orders o SET
    fee_small_kobo            = s.fee_small_kobo,
    fee_large_kobo            = s.fee_large_kobo,
    fee_threshold_kobo        = s.fee_threshold_kobo,
    stamp_duty_kobo           = s.stamp_duty_kobo,
    stamp_duty_threshold_kobo = s.stamp_duty_threshold_kobo
FROM platform_settings s
WHERE s.id
  AND o.status IN ('paid', 'picked_up', 'delivered')
  AND o.fee_small_kobo IS NULL;
