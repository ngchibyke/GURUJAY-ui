-- Hard caps (global constraints): unit price <= 1,000,000,000 kobo, stock <= 100,000.
-- Keep in step with src/limits.rs. Added NOT VALID then validated, so the table lock stays short.
ALTER TABLE products
    ADD CONSTRAINT products_base_price_cap CHECK (base_price_kobo <= 1000000000) NOT VALID;
ALTER TABLE product_variants
    ADD CONSTRAINT product_variants_price_cap CHECK (price_kobo <= 1000000000) NOT VALID,
    ADD CONSTRAINT product_variants_stock_cap CHECK (stock <= 100000) NOT VALID;
ALTER TABLE products VALIDATE CONSTRAINT products_base_price_cap;
ALTER TABLE product_variants VALIDATE CONSTRAINT product_variants_price_cap;
ALTER TABLE product_variants VALIDATE CONSTRAINT product_variants_stock_cap;
