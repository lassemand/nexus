-- No sector source covers non-US listings (Polygon is US-only), so their
-- sector is unknown. NULL rather than a made-up default.
ALTER TABLE companies ALTER COLUMN sector DROP NOT NULL;
