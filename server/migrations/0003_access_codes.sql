CREATE TABLE access_codes (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code_hash   bytea NOT NULL UNIQUE,
    code_last4  text NOT NULL,
    label       text,
    max_uses    integer CHECK (max_uses IS NULL OR max_uses >= 1),
    uses        integer NOT NULL DEFAULT 0,
    expires_at  timestamptz,
    revoked_at  timestamptz,
    created_by  uuid NOT NULL REFERENCES users (id),
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE access_code_redemptions (
    code_id     uuid NOT NULL REFERENCES access_codes (id),
    user_id     uuid NOT NULL UNIQUE REFERENCES users (id),
    redeemed_at timestamptz NOT NULL DEFAULT now()
);

-- Failed redemption attempts, for rate limiting (10 per user per hour).
CREATE TABLE redeem_attempts (
    id      bigserial PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users (id),
    at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX redeem_attempts_user_at_idx ON redeem_attempts (user_id, at);
