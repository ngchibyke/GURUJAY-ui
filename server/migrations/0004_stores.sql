CREATE TYPE store_status AS ENUM ('pending', 'active', 'suspended');

CREATE TABLE stores (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id            uuid NOT NULL REFERENCES users (id),
    name                text NOT NULL,
    slug                text NOT NULL UNIQUE,
    description         text NOT NULL DEFAULT '',
    logo_key            text,
    banner_key          text,
    pickup_address_enc  bytea NOT NULL,
    pickup_state        text NOT NULL CHECK (pickup_state = 'FCT'),
    delivery_courier    boolean NOT NULL,
    delivery_self       boolean NOT NULL,
    CHECK (delivery_courier OR delivery_self),
    status              store_status NOT NULL DEFAULT 'pending',
    created_at          timestamptz NOT NULL DEFAULT now(),
    deleted_at          timestamptz
);
CREATE INDEX stores_owner_idx ON stores (owner_id) WHERE deleted_at IS NULL;
