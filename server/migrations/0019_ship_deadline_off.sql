-- Final fix wave (I1): the ship-deadline job ships switched OFF.
-- Part 2 has no pickup route (plan 3 adds it), so nothing can move an order
-- out of `paid`: with the job on, every paid order would be refunded and its
-- seller struck 48 hours after payment. The owner turns it on (settings
-- route, audited) once plan 3's pickup routes exist.
-- Personal data: none.

ALTER TABLE platform_settings
    ALTER COLUMN jobs_ship_deadline_enabled SET DEFAULT false;
UPDATE platform_settings SET jobs_ship_deadline_enabled = false;
