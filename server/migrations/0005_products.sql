CREATE TYPE product_condition AS ENUM ('new', 'used', 'refurbished');

CREATE TABLE products (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    store_id        uuid NOT NULL REFERENCES stores (id),
    name            text NOT NULL,
    description     text NOT NULL,
    category        text NOT NULL,
    brand           text,
    condition       product_condition NOT NULL,
    photo_keys      text[] NOT NULL CHECK (cardinality(photo_keys) BETWEEN 1 AND 10),
    weight_grams    integer NOT NULL CHECK (weight_grams > 0),
    length_cm       integer CHECK (length_cm > 0),
    width_cm        integer CHECK (width_cm > 0),
    height_cm       integer CHECK (height_cm > 0),
    base_price_kobo bigint NOT NULL CHECK (base_price_kobo > 0),
    is_published    boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz
);
CREATE INDEX products_store_idx ON products (store_id) WHERE deleted_at IS NULL;

CREATE TABLE product_variants (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id  uuid NOT NULL REFERENCES products (id),
    attributes  jsonb NOT NULL,
    stock       integer NOT NULL CHECK (stock >= 0),
    price_kobo  bigint CHECK (price_kobo > 0),
    sku         text,
    CONSTRAINT product_variants_attrs_unique UNIQUE (product_id, attributes)
);
