-- Seller payout accounts: one bank account per store. The account number and the
-- account holder's name are personal data and are stored encrypted (FieldCipher).
-- Only the last four digits are kept in the clear, to show the seller which account is set.
CREATE TYPE payout_account_status AS ENUM ('pending', 'verified');

CREATE TABLE payout_accounts (
    store_id                 uuid PRIMARY KEY REFERENCES stores (id),
    bank_code                text NOT NULL CHECK (bank_code ~ '^[0-9]{3,6}$'),
    account_number_enc       bytea NOT NULL,
    account_last4            text NOT NULL CHECK (account_last4 ~ '^[0-9]{4}$'),
    account_name_enc         bytea,
    status                   payout_account_status NOT NULL DEFAULT 'pending',
    -- Why an account awaits owner review: lookup_unsupported, lookup_unavailable,
    -- name_mismatch, checksum_failed. Always NULL once verified.
    pending_reason           text,
    verified_by              text CHECK (verified_by IN ('provider', 'owner')),
    verified_at              timestamptz,
    -- Set on the first set and on every change of the account (not on verification).
    changed_at               timestamptz NOT NULL DEFAULT now(),
    -- True when the store already had an account before the latest change. The payout
    -- task refuses to pay out within a hold window of changed_at only when this is true.
    previous_account_existed boolean NOT NULL DEFAULT false,
    updated_at               timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT payout_accounts_verified_complete CHECK (
        status <> 'verified'
        OR (verified_by IS NOT NULL AND verified_at IS NOT NULL
            AND account_name_enc IS NOT NULL AND pending_reason IS NULL)
    )
);
