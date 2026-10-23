-- Carts, checkouts, orders and their snapshotted items.
-- Keep the caps in step with src/limits.rs (MAX_LINE_QUANTITY, MAX_CHECKOUT_TOTAL_KOBO, MAX_PRICE_KOBO).
-- Personal data: none beyond user ids. Buyers' bank details are never stored, only
-- the provider's payment id and our reference.

CREATE TABLE cart_items (
    user_id    uuid NOT NULL REFERENCES users (id),
    -- A variant removed by its seller leaves the cart with it (it was never ordered,
    -- otherwise order_items would block the delete).
    variant_id uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    quantity   integer NOT NULL CHECK (quantity BETWEEN 1 AND 100),
    added_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, variant_id)
);
CREATE INDEX cart_items_variant_idx ON cart_items (variant_id);

CREATE TYPE checkout_status AS ENUM ('pending', 'paid', 'expired', 'amount_mismatch', 'refunded');

CREATE TABLE checkouts (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    buyer_id            uuid NOT NULL REFERENCES users (id),
    -- payments::new_reference("CK"): letters and digits only, at most 24 characters (Mono's cap).
    reference           text NOT NULL UNIQUE CHECK (reference ~ '^[A-Z0-9]{1,24}$'),
    method              text NOT NULL,
    -- provider.name() at creation; every later call goes through ProviderRegistry::by_name.
    provider            text NOT NULL,
    total_kobo          bigint NOT NULL CHECK (total_kobo > 0 AND total_kobo <= 5000000000),
    status              checkout_status NOT NULL DEFAULT 'pending',
    provider_payment_id text,
    -- The provider's hosted payment page; stored so an idempotent replay can return it.
    -- It carries our reference only, never personal data.
    payment_url         text,
    -- Client-chosen Idempotency-Key header (1-64 of [A-Za-z0-9_-]); unique per buyer.
    idempotency_key     text CHECK (idempotency_key ~ '^[A-Za-z0-9_-]{1,64}$'),
    collection_fee_kobo bigint CHECK (collection_fee_kobo >= 0),
    -- Set by the payment webhook (Task 6) when the provider flags or holds the payment.
    provider_flagged    boolean NOT NULL DEFAULT false,
    expires_at          timestamptz NOT NULL,
    paid_at             timestamptz,
    created_at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX checkouts_buyer_idx ON checkouts (buyer_id, created_at);
CREATE INDEX checkouts_pending_expiry_idx ON checkouts (expires_at) WHERE status = 'pending';
CREATE UNIQUE INDEX checkouts_buyer_idempotency_key_idx
    ON checkouts (buyer_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

CREATE TYPE order_status AS ENUM (
    'pending_payment', 'paid', 'picked_up', 'delivered', 'completed',
    'cancelled', 'refund_pending', 'refunded'
);

CREATE TABLE orders (
    id                        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    checkout_id               uuid NOT NULL REFERENCES checkouts (id),
    store_id                  uuid NOT NULL REFERENCES stores (id),
    buyer_id                  uuid NOT NULL REFERENCES users (id),
    status                    order_status NOT NULL DEFAULT 'pending_payment',
    items_total_kobo          bigint NOT NULL CHECK (items_total_kobo > 0),
    delivery_fee_kobo         bigint NOT NULL DEFAULT 0 CHECK (delivery_fee_kobo >= 0),
    -- Snapshotted from platform_settings at checkout; never recomputed from live settings.
    platform_cut_bps          integer NOT NULL CHECK (platform_cut_bps BETWEEN 0 AND 10000),
    platform_cut_kobo         bigint NOT NULL
        CHECK (platform_cut_kobo >= 0 AND platform_cut_kobo <= items_total_kobo),
    collection_fee_share_kobo bigint NOT NULL DEFAULT 0 CHECK (collection_fee_share_kobo >= 0),
    paid_at                   timestamptz,
    picked_up_at              timestamptz,
    delivered_at              timestamptz,
    completed_at              timestamptz,
    cancelled_at              timestamptz,
    backstop_flagged_at       timestamptz,
    -- A held order never auto-releases (complaints in plan 5, owner hold in Task 10).
    held_at                   timestamptz,
    hold_reason               text,
    -- Deadlines stored per order so settings changes are never retroactive:
    -- ship_by and backstop_at are set when paid (Task 6), release_at when delivered (Task 8).
    ship_by                   timestamptz,
    release_at                timestamptz,
    backstop_at               timestamptz,
    created_at                timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT orders_one_per_store UNIQUE (checkout_id, store_id)
);
CREATE INDEX orders_store_idx ON orders (store_id, created_at);
CREATE INDEX orders_buyer_idx ON orders (buyer_id, created_at);

-- Snapshotted at checkout: later product edits never change what was bought.
-- variant_id has no ON DELETE action, so a variant that was ordered cannot be deleted
-- (products::replace_variants answers 409 variant_in_use before reaching this).
CREATE TABLE order_items (
    order_id        uuid NOT NULL REFERENCES orders (id),
    variant_id      uuid NOT NULL REFERENCES product_variants (id),
    product_name    text NOT NULL,
    attributes      jsonb NOT NULL,
    unit_price_kobo bigint NOT NULL CHECK (unit_price_kobo > 0 AND unit_price_kobo <= 1000000000),
    quantity        integer NOT NULL CHECK (quantity BETWEEN 1 AND 100),
    PRIMARY KEY (order_id, variant_id)
);
CREATE INDEX order_items_variant_idx ON order_items (variant_id);
