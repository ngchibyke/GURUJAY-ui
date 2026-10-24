-- Release and payout engine (Task 8).
-- Personal data: payouts gain a snapshot of the bank account paid (bank code,
-- the account number ENCRYPTED with FieldCipher, and its last four digits), so
-- support can see exactly where money went and an in-flight payout is never
-- redirected by a later account change. The number is decrypted only at send
-- time and is never logged. Retention: payouts are financial records (DPO item:
-- document the retention period alongside refunds and provider_events).

ALTER TABLE payouts
    -- Why the order was released: buyer_confirmed, auto_after_delivery, owner_decision.
    ADD COLUMN release_reason       text,
    -- Claim on the provider call (like refunds.sending_until). A `sending` row
    -- whose lease has expired is treated as `unknown`: the next send looks the
    -- payout up by reference before anything else and never re-creates blindly.
    ADD COLUMN sending_until        timestamptz,
    -- Account snapshot, taken when a send is claimed.
    ADD COLUMN bank_code            text CHECK (bank_code ~ '^[0-9]{3,6}$'),
    ADD COLUMN account_number_enc   bytea,
    ADD COLUMN account_last4        text CHECK (account_last4 ~ '^[0-9]{4}$'),
    -- When the provider accepted the payout (status pending); a payout still
    -- pending 24h later goes to `manual`, never to `failed` or a re-send.
    ADD COLUMN pending_since        timestamptz,
    -- Last payout_status poll; the job polls a pending payout at most hourly.
    ADD COLUMN last_polled_at       timestamptz,
    -- The provider's actual fee, once known (fee_kobo is what the seller was charged).
    ADD COLUMN provider_fee_kobo    bigint CHECK (provider_fee_kobo >= 0),
    ADD COLUMN settled_at           timestamptz;

-- Bank holidays (Africa/Lagos dates). Payouts wait for Mono's settlement on the
-- next working day; weekends and these days are skipped. The owner edits the
-- list (Task 10); seeded empty because Nigerian holidays are declared late.
CREATE TABLE bank_holidays (
    day  date PRIMARY KEY,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 100)
);

-- One row per ledger posting key: (kind, ref) such as ("release", order id) or
-- ("payout_reversal", payout id). Inserted in the same transaction as the
-- entries; a duplicate key skips the entries, so nothing can post twice.
CREATE TABLE ledger_postings (
    kind      text NOT NULL CHECK (length(kind) BETWEEN 1 AND 50),
    ref       text NOT NULL CHECK (length(ref) BETWEEN 1 AND 100),
    txn_id    uuid NOT NULL UNIQUE,
    posted_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, ref)
);

-- Append-only, like ledger_entries.
CREATE FUNCTION ledger_postings_reject_change() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'ledger_postings is append-only (% not allowed)', TG_OP;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER ledger_postings_no_update
    BEFORE UPDATE ON ledger_postings
    FOR EACH ROW EXECUTE FUNCTION ledger_postings_reject_change();
CREATE TRIGGER ledger_postings_no_delete
    BEFORE DELETE ON ledger_postings
    FOR EACH ROW EXECUTE FUNCTION ledger_postings_reject_change();
CREATE TRIGGER ledger_postings_no_truncate
    BEFORE TRUNCATE ON ledger_postings
    FOR EACH STATEMENT EXECUTE FUNCTION ledger_postings_reject_change();
ALTER TABLE ledger_postings ENABLE ALWAYS TRIGGER ledger_postings_no_update;
ALTER TABLE ledger_postings ENABLE ALWAYS TRIGGER ledger_postings_no_delete;
ALTER TABLE ledger_postings ENABLE ALWAYS TRIGGER ledger_postings_no_truncate;

CREATE INDEX payouts_pending_poll_idx ON payouts (last_polled_at) WHERE status = 'pending';
