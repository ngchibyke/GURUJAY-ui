-- Provider webhook dedupe, payouts (Task 8), refunds and seller strikes (Task 9).
-- Personal data: none. Webhook bodies are never stored (they can carry the payer's
-- bank account); only the provider's event id is kept. Seller bank details stay
-- encrypted in payout_accounts and are not copied here.

-- One row per provider event id. A row with processed_at NULL is not a duplicate:
-- the event is settled again on the provider's retry (Mono retries a non-200
-- delivery up to 25 times over 48h with the same event_id).
CREATE TABLE provider_events (
    provider     text NOT NULL,
    event_id     text NOT NULL CHECK (length(event_id) BETWEEN 1 AND 200),
    kind         text NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz,
    PRIMARY KEY (provider, event_id)
);
CREATE INDEX provider_events_unprocessed_idx ON provider_events (received_at)
    WHERE processed_at IS NULL;

CREATE TYPE payout_status AS ENUM (
    'scheduled', 'sending', 'pending', 'unknown', 'successful', 'failed', 'manual'
);

CREATE TABLE payouts (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id           uuid NOT NULL UNIQUE REFERENCES orders (id),
    -- payments::new_reference("PO"); never re-minted for the same payout.
    reference          text NOT NULL UNIQUE CHECK (reference ~ '^[A-Z0-9]{1,24}$'),
    provider           text NOT NULL,
    provider_payout_id text,
    -- May be <= 0 when fees exceed the order (status manual).
    amount_kobo        bigint NOT NULL,
    fee_kobo           bigint NOT NULL DEFAULT 0 CHECK (fee_kobo >= 0),
    status             payout_status NOT NULL DEFAULT 'scheduled',
    send_after         timestamptz,
    next_attempt_at    timestamptz,
    attempts           integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Sanitised error class only, never provider bodies or account numbers.
    last_error         text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX payouts_status_idx ON payouts (status, next_attempt_at);

-- requested: booked, provider not (yet) confirmed; processing: provider accepted
-- (the buyer gets the money within about 10 working days); completed/failed: the
-- provider's final answer; manual: the owner refunds by hand.
CREATE TYPE refund_status AS ENUM ('requested', 'processing', 'completed', 'failed', 'manual');

CREATE TABLE refunds (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id           uuid NOT NULL UNIQUE REFERENCES orders (id),
    -- payments::new_reference("RF"); reused on every retry.
    reference          text NOT NULL UNIQUE CHECK (reference ~ '^[A-Z0-9]{1,24}$'),
    provider           text NOT NULL,
    amount_kobo        bigint NOT NULL CHECK (amount_kobo > 0),
    reason             text NOT NULL,
    status             refund_status NOT NULL DEFAULT 'requested',
    provider_refund_id text,
    attempts           integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Claim on the provider call: at most one caller sends a refund at a time.
    sending_until      timestamptz,
    last_error         text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX refunds_status_idx ON refunds (status);

CREATE TABLE seller_strikes (
    id       bigserial PRIMARY KEY,
    store_id uuid NOT NULL REFERENCES stores (id),
    order_id uuid NOT NULL REFERENCES orders (id),
    reason   text NOT NULL,
    at       timestamptz NOT NULL DEFAULT now(),
    UNIQUE (order_id, reason)
);
CREATE INDEX seller_strikes_store_idx ON seller_strikes (store_id, at);
