-- Owner-controlled platform settings: a single row, seeded here.
-- Keep the fee caps in step with src/limits.rs (MAX_PRICE_KOBO) and the ranges with src/settings/service.rs.
CREATE TABLE platform_settings (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    platform_cut_bps int NOT NULL DEFAULT 500 CHECK (platform_cut_bps BETWEEN 0 AND 3000),
    fee_small_kobo bigint NOT NULL DEFAULT 3500 CHECK (fee_small_kobo BETWEEN 0 AND 1000000000),
    fee_large_kobo bigint NOT NULL DEFAULT 5000 CHECK (fee_large_kobo BETWEEN 0 AND 1000000000),
    fee_threshold_kobo bigint NOT NULL DEFAULT 5000000 CHECK (fee_threshold_kobo >= 0),
    stamp_duty_kobo bigint NOT NULL DEFAULT 5000 CHECK (stamp_duty_kobo BETWEEN 0 AND 1000000000),
    stamp_duty_threshold_kobo bigint NOT NULL DEFAULT 1000000 CHECK (stamp_duty_threshold_kobo >= 0),
    enabled_methods text[] NOT NULL DEFAULT '{mono_directpay}' CHECK (cardinality(enabled_methods) >= 1),
    payout_provider text NOT NULL DEFAULT 'mono',
    checkout_ttl_minutes int NOT NULL DEFAULT 30 CHECK (checkout_ttl_minutes BETWEEN 5 AND 120),
    ship_deadline_hours int NOT NULL DEFAULT 48 CHECK (ship_deadline_hours BETWEEN 1 AND 168),
    release_after_hours int NOT NULL DEFAULT 72 CHECK (release_after_hours BETWEEN 1 AND 720),
    backstop_days int NOT NULL DEFAULT 30 CHECK (backstop_days BETWEEN 1 AND 90),
    payout_settlement_buffer_hours int NOT NULL DEFAULT 2 CHECK (payout_settlement_buffer_hours BETWEEN 0 AND 72),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT platform_settings_backstop_outlasts_release CHECK (backstop_days * 24 > release_after_hours)
);
INSERT INTO platform_settings (id) VALUES (true);
