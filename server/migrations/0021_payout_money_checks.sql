-- Money constraints on payouts (final review M5), and corrections to two
-- earlier migration comments that cannot be edited in place (sqlx checksums
-- applied migrations):
--
-- * 0013_payments_payouts.sql says `payouts.amount_kobo` "May be <= 0 when
--   fees exceed the order". It is never negative: release books
--   `max(0, order - fees)`, and a zero payout goes to `manual`. The CHECK
--   below makes that hold in the database.
-- * 0017_admin.sql says the order fee snapshot columns are "NULL on orders
--   paid before this migration: those use the schedule at release". 0018
--   backfilled every order paid but not yet released, so a NULL snapshot is
--   left only on orders not yet paid, or already released, refunded or
--   cancelled before 0017 (whose payout, if any, is already fixed). Release
--   reads the live schedule only for a NULL row.
--
-- `settled_by IN ('provider', 'owner')` (or NULL) is already enforced by the
-- column CHECK added in 0017. What was missing is that a successful payout
-- always says who settled it: rows settled before 0017 are backfilled as the
-- provider's (mark-paid did not exist before 0017).

UPDATE payouts SET settled_by = 'provider'
WHERE status = 'successful' AND settled_by IS NULL;

ALTER TABLE payouts
    ADD CONSTRAINT payouts_amount_nonnegative CHECK (amount_kobo >= 0),
    ADD CONSTRAINT payouts_successful_has_settler
        CHECK (status <> 'successful' OR settled_by IS NOT NULL);
