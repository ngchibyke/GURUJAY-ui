-- Reserved-quantity model: `stock` is units on hand (set by the seller), and
-- `reserved_quantity` is units held by pending checkouts. Available to buy is
-- `stock - reserved_quantity`. Checkout reserves, expiry releases, and a settled
-- payment moves units out of both (checkout::commit_reserved).
-- A seller can therefore edit on-hand stock without wiping out reservations,
-- but never below what is reserved.
ALTER TABLE product_variants
    ADD COLUMN reserved_quantity integer NOT NULL DEFAULT 0,
    ADD CONSTRAINT product_variants_reserved_nonneg CHECK (reserved_quantity >= 0),
    ADD CONSTRAINT product_variants_reserved_le_stock CHECK (reserved_quantity <= stock);
