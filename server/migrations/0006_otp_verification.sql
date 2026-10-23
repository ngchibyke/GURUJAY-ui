-- Bind each OTP to the sign-up attempt that set the account's credentials.
-- The client holds a random verification id; only its SHA-256 is stored.
ALTER TABLE otp_codes ADD COLUMN verification_hash bytea;
-- When the code was handed to the SMS sender (NULL: issued while throttled, never sent).
-- The send throttle counts sent codes only.
ALTER TABLE otp_codes ADD COLUMN sent_at timestamptz;

-- Rows from before this migration carry no verification id: retire them so they can
-- never verify, and give them an unguessable placeholder hash.
UPDATE otp_codes
   SET verification_hash = sha256(convert_to(gen_random_uuid()::text || id::text, 'UTF8')),
       sent_at = created_at,
       consumed_at = COALESCE(consumed_at, now());

ALTER TABLE otp_codes ALTER COLUMN verification_hash SET NOT NULL;
CREATE UNIQUE INDEX otp_codes_verification_hash_idx ON otp_codes (verification_hash);

-- Failed sign-in attempts, for the per-phone cap (10 per rolling hour).
-- Keyed by the phone's blind index (never the phone itself) so unknown phones are
-- counted the same way as real accounts. Rows older than an hour are pruned on write.
CREATE TABLE sign_in_failures (
    id        bigserial PRIMARY KEY,
    phone_idx bytea NOT NULL,
    at        timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX sign_in_failures_phone_at_idx ON sign_in_failures (phone_idx, at);
CREATE INDEX sign_in_failures_at_idx ON sign_in_failures (at);
